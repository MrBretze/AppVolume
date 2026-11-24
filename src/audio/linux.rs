//! Linux backend speaking the PulseAudio native protocol (pure Rust, no libpulse).
//!
//! Works with PulseAudio and with PipeWire through `pipewire-pulse`. One connection is
//! kept for requests and a second one, on its own thread, receives change events.

use super::{
	AppStream, AudioBackend, AudioError, AudioResult, IconHint, OutputDevice, SYSTEM_KEY, Volume,
	group_streams,
};

use crate::names::{self, Mapped};

use std::collections::HashMap;
use std::ffi::CString;
use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulseaudio::protocol::{
	self, AuthParams, ChannelVolume, Command, CommandReply, MoveStreamParams, Prop, Props,
	SetClientNameReply, SetStreamMuteParams, SetStreamVolumeParams, SinkInfoList, SinkInputInfo,
	SinkInputInfoList, SubscriptionEventFacility, SubscriptionMask,
};

/// Icon theme names tried for [`SYSTEM_KEY`] (freedesktop icon themes).
const SYSTEM_ICON_NAMES: &[&str] = &[
	"preferences-desktop-sound",
	"audio-volume-high",
	"multimedia-volume-control",
];

/// Raw PulseAudio volume for 100 % (`PA_VOLUME_NORM`).
const VOLUME_NORM: f32 = 0x10000 as f32;
/// PulseAudio accepts more, but 150 % is what pavucontrol offers and is already loud.
const MAX_VOLUME: Volume = 1.5;
/// `PA_NATIVE_COOKIE_LENGTH`
const COOKIE_LENGTH: usize = 256;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_NAME: &str = "AppVolume (OpenDeck)";

pub struct PulseBackend {
	conn: Mutex<Option<Connection>>,
}

impl PulseBackend {
	pub fn new() -> Self {
		Self {
			conn: Mutex::new(None),
		}
	}

	/// Run `f` on the shared connection, (re)connecting first if needed.
	/// The connection is dropped on error so the next call starts fresh.
	fn with_conn<T>(&self, f: impl FnOnce(&mut Connection) -> AudioResult<T>) -> AudioResult<T> {
		let mut guard = self.conn.lock().unwrap_or_else(|e| e.into_inner());
		if guard.is_none() {
			*guard = Some(Connection::open()?);
		}
		let result = f(guard.as_mut().expect("connection was just opened"));
		if matches!(
			result,
			Err(AudioError::Unavailable(_) | AudioError::Other(_))
		) {
			*guard = None;
		}
		result
	}

	fn streams_of(conn: &mut Connection, app_key: &str) -> AudioResult<Vec<SinkInputInfo>> {
		let inputs: SinkInputInfoList = conn.request(&Command::GetSinkInputInfoList)?;
		let matching: Vec<_> = inputs
			.into_iter()
			.filter(|s| stream_key(&s.props, &s.name) == app_key)
			.collect();
		if matching.is_empty() {
			return Err(AudioError::NotFound);
		}
		Ok(matching)
	}
}

impl AudioBackend for PulseBackend {
	fn apps(&self) -> AudioResult<Vec<AppStream>> {
		self.with_conn(|conn| {
			let sinks: SinkInfoList = conn.request(&Command::GetSinkInfoList)?;
			let sink_names: HashMap<u32, String> = sinks
				.into_iter()
				.map(|s| (s.index, s.name.to_string_lossy().into_owned()))
				.collect();
			let inputs: SinkInputInfoList = conn.request(&Command::GetSinkInputInfoList)?;
			let streams = inputs
				.into_iter()
				.filter(|s| s.has_volume)
				.map(|s| to_app_stream(&s, sink_names.get(&s.sink_index).cloned()))
				.collect();
			Ok(group_streams(streams))
		})
	}

	fn outputs(&self) -> AudioResult<Vec<OutputDevice>> {
		self.with_conn(|conn| {
			let sinks: SinkInfoList = conn.request(&Command::GetSinkInfoList)?;
			Ok(sinks
				.into_iter()
				.map(|s| {
					let id = s.name.to_string_lossy().into_owned();
					let name = s
						.description
						.map(|d| d.to_string_lossy().into_owned())
						.filter(|d| !d.is_empty())
						.unwrap_or_else(|| id.clone());
					OutputDevice { id, name }
				})
				.collect())
		})
	}

	fn set_volume(&self, app_key: &str, volume: Volume) -> AudioResult<()> {
		let raw = (volume.clamp(0.0, MAX_VOLUME) * VOLUME_NORM).round() as u32;
		self.with_conn(|conn| {
			for stream in Self::streams_of(conn, app_key)? {
				let channels = stream.cvolume.channels().len().max(1) as u8;
				let mut cvolume = ChannelVolume::empty();
				for _ in 0..channels {
					cvolume.push(protocol::Volume::from_u32_clamped(raw));
				}
				conn.ack(&Command::SetSinkInputVolume(SetStreamVolumeParams {
					index: stream.index,
					volume: cvolume,
				}))?;
			}
			Ok(())
		})
	}

	fn set_muted(&self, app_key: &str, muted: bool) -> AudioResult<()> {
		self.with_conn(|conn| {
			for stream in Self::streams_of(conn, app_key)? {
				conn.ack(&Command::SetSinkInputMute(SetStreamMuteParams {
					index: stream.index,
					mute: muted,
				}))?;
			}
			Ok(())
		})
	}

	fn move_to_output(&self, app_key: &str, output_id: &str) -> AudioResult<()> {
		let device_name = CString::new(output_id).map_err(|_| AudioError::NotFound)?;
		self.with_conn(|conn| {
			for stream in Self::streams_of(conn, app_key)? {
				conn.ack(&Command::MoveSinkInput(MoveStreamParams {
					index: Some(stream.index),
					device_index: None,
					device_name: Some(device_name.clone()),
				}))?;
			}
			Ok(())
		})
	}

	fn max_volume(&self) -> Volume {
		MAX_VOLUME
	}

	fn watch(&self, on_change: Arc<dyn Fn() + Send + Sync>) {
		let spawned = std::thread::Builder::new()
			.name("pulse-events".into())
			.spawn(move || {
				loop {
					if let Err(error) = watch_events(&*on_change) {
						log::debug!("PulseAudio event connection lost: {error}");
					}
					std::thread::sleep(Duration::from_secs(2));
				}
			});
		if let Err(error) = spawned {
			log::error!("Could not start the PulseAudio event thread: {error}");
		}
	}
}

/// Block on a dedicated connection and report mixer changes until it fails.
fn watch_events(on_change: &(dyn Fn() + Send + Sync)) -> AudioResult<()> {
	let mut conn = Connection::open()?;
	// Events arrive whenever they happen: no read timeout on this connection.
	conn.sock
		.get_ref()
		.set_read_timeout(None)
		.map_err(io_error)?;
	let mask = SubscriptionMask::SINK | SubscriptionMask::SINK_INPUT | SubscriptionMask::SERVER;
	conn.ack(&Command::Subscribe(mask))?;
	on_change();
	loop {
		let (_, command) =
			protocol::read_command_message(&mut conn.sock, conn.version).map_err(protocol_error)?;
		if let Command::SubscribeEvent(event) = command
			&& matches!(
				event.event_facility,
				SubscriptionEventFacility::Sink
					| SubscriptionEventFacility::SinkInput
					| SubscriptionEventFacility::Server
			) {
			on_change();
		}
	}
}

struct Connection {
	sock: BufReader<UnixStream>,
	version: u16,
	seq: u32,
}

impl Connection {
	fn open() -> AudioResult<Self> {
		let path = socket_path()
			.ok_or_else(|| AudioError::Unavailable("no PulseAudio socket found".into()))?;
		let stream = UnixStream::connect(&path).map_err(|e| {
			AudioError::Unavailable(format!("cannot connect to {}: {e}", path.display()))
		})?;
		stream
			.set_read_timeout(Some(IO_TIMEOUT))
			.map_err(io_error)?;
		stream
			.set_write_timeout(Some(IO_TIMEOUT))
			.map_err(io_error)?;

		let mut conn = Connection {
			sock: BufReader::new(stream),
			version: protocol::MAX_VERSION,
			seq: 0,
		};

		// PipeWire ignores the cookie's content but, like PulseAudio, rejects one of the wrong
		// length; libpulse sends zeros when it has none (e.g. inside the Flatpak sandbox).
		let cookie = pulseaudio::cookie_path_from_env()
			.and_then(|path| std::fs::read(path).ok())
			.filter(|c| c.len() == COOKIE_LENGTH)
			.unwrap_or_else(|| vec![0; COOKIE_LENGTH]);
		let auth: protocol::AuthReply = conn.request(&Command::Auth(AuthParams {
			version: protocol::MAX_VERSION,
			supports_shm: false,
			supports_memfd: false,
			cookie,
		}))?;
		conn.version = protocol::MAX_VERSION.min(auth.version);

		let mut props = Props::new();
		props.set(
			Prop::ApplicationName,
			CString::new(CLIENT_NAME).expect("no NUL in client name"),
		);
		let _: SetClientNameReply = conn.request(&Command::SetClientName(props))?;
		Ok(conn)
	}

	fn next_seq(&mut self) -> u32 {
		self.seq = self.seq.wrapping_add(1);
		self.seq
	}

	fn send(&mut self, command: &Command) -> AudioResult<()> {
		let seq = self.next_seq();
		protocol::write_command_message(self.sock.get_mut(), seq, command, self.version)
			.map_err(protocol_error)
	}

	fn request<T: CommandReply>(&mut self, command: &Command) -> AudioResult<T> {
		self.send(command)?;
		let (_, reply) = protocol::read_reply_message::<T>(&mut self.sock, self.version)
			.map_err(protocol_error)?;
		Ok(reply)
	}

	fn ack(&mut self, command: &Command) -> AudioResult<()> {
		self.send(command)?;
		protocol::read_ack_message(&mut self.sock).map_err(protocol_error)?;
		Ok(())
	}
}

fn io_error(error: std::io::Error) -> AudioError {
	AudioError::Unavailable(error.to_string())
}

fn protocol_error(error: protocol::ProtocolError) -> AudioError {
	match error {
		protocol::ProtocolError::ServerError(protocol::PulseError::NoEntity) => {
			AudioError::NotFound
		}
		protocol::ProtocolError::ServerError(code) => {
			AudioError::Other(format!("PulseAudio server error: {code:?}"))
		}
		protocol::ProtocolError::Io(e) => AudioError::Unavailable(e.to_string()),
		other => AudioError::Other(other.to_string()),
	}
}

/// Find the PulseAudio socket, including the paths used inside a Flatpak sandbox.
fn socket_path() -> Option<PathBuf> {
	let mut candidates = Vec::new();
	if let Ok(server) = std::env::var("PULSE_SERVER") {
		// Only local sockets are supported: "unix:/path" or a plain path.
		for entry in server.split_whitespace() {
			let path = entry.strip_prefix("unix:").unwrap_or(entry);
			if path.starts_with('/') {
				candidates.push(PathBuf::from(path));
			}
		}
	}
	if let Ok(dir) = std::env::var("PULSE_RUNTIME_PATH") {
		candidates.push(PathBuf::from(dir).join("native"));
	}
	if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
		candidates.push(PathBuf::from(dir).join("pulse/native"));
	}
	candidates.push(PathBuf::from("/run/flatpak/pulse/native"));
	if let Ok(meta) = std::fs::metadata("/proc/self") {
		use std::os::unix::fs::MetadataExt;
		candidates.push(PathBuf::from(format!(
			"/run/user/{}/pulse/native",
			meta.uid()
		)));
	}
	candidates.into_iter().find(|p| p.exists())
}

fn prop(props: &Props, key: Prop) -> Option<String> {
	prop_bytes(props.get(key))
}

fn prop_raw(props: &Props, key: &str) -> Option<String> {
	let key = CString::new(key).ok()?;
	prop_bytes(props.get_bytes(key.as_c_str()))
}

fn prop_bytes(bytes: Option<&[u8]>) -> Option<String> {
	let bytes = bytes?;
	let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
	let value = String::from_utf8_lossy(bytes).trim().to_owned();
	(!value.is_empty()).then_some(value)
}

/// Identifier grouping the streams of one application; desktop sounds share [`SYSTEM_KEY`].
fn stream_key(props: &Props, stream_name: &CString) -> String {
	identify(props, stream_name).0
}

/// Grouping key and `app-names.json` mapping of a stream.
fn identify(props: &Props, stream_name: &CString) -> (String, Option<Mapped>) {
	let app_id = prop_raw(props, "pipewire.access.portal.app_id");
	let binary = prop(props, Prop::ApplicationProcessBinary);
	let app_name = prop(props, Prop::ApplicationName);
	let stream_name = stream_name.to_string_lossy().into_owned();
	let mapped = names::lookup(
		[
			app_id.as_deref(),
			binary.as_deref(),
			app_name.as_deref(),
			Some(stream_name.as_str()),
		]
		.into_iter()
		.flatten(),
	);
	let is_event = prop(props, Prop::MediaRole).as_deref() == Some("event");
	if mapped == Some(Mapped::System) || is_event {
		return (SYSTEM_KEY.to_owned(), mapped);
	}
	let key = binary.or(app_name).unwrap_or(stream_name).to_lowercase();
	(key, mapped)
}

fn to_app_stream(stream: &SinkInputInfo, output_id: Option<String>) -> AppStream {
	let props = &stream.props;
	let binary = prop(props, Prop::ApplicationProcessBinary);
	let app_name = prop(props, Prop::ApplicationName);
	let (key, mapped) = identify(props, &stream.name);
	let reported: Vec<String> = [
		prop_raw(props, "pipewire.access.portal.app_id"),
		binary.clone(),
		app_name.clone(),
		Some(stream.name.to_string_lossy().into_owned()),
	]
	.into_iter()
	.flatten()
	.fold(Vec::new(), |mut acc, n| {
		if !acc.contains(&n) {
			acc.push(n);
		}
		acc
	});
	let name = match mapped {
		Some(Mapped::Name(name)) => name,
		_ => app_name
			.as_deref()
			.map(clean_app_name)
			.or_else(|| binary.as_deref().map(capitalize))
			.unwrap_or_else(|| stream.name.to_string_lossy().into_owned()),
	};

	if key == SYSTEM_KEY {
		return AppStream {
			key,
			name: "System".to_owned(),
			icon: IconHint {
				names: SYSTEM_ICON_NAMES.iter().map(|n| (*n).to_owned()).collect(),
				exe_path: None,
			},
			volume: average_volume(stream),
			muted: stream.muted,
			output_id,
			reported,
		};
	}

	let names = [
		prop(props, Prop::ApplicationIconName),
		prop_raw(props, "pipewire.access.portal.app_id"),
		prop(props, Prop::ApplicationId),
		binary.clone(),
		app_name.map(|n| n.to_lowercase()),
		Some(key.clone()),
	]
	.into_iter()
	.flatten()
	.fold(Vec::new(), |mut acc, n| {
		if !acc.contains(&n) {
			acc.push(n);
		}
		acc
	});

	AppStream {
		key,
		name,
		icon: IconHint {
			names,
			exe_path: None,
		},
		volume: average_volume(stream),
		muted: stream.muted,
		output_id,
		reported,
	}
}

fn average_volume(stream: &SinkInputInfo) -> Volume {
	let channels = stream.cvolume.channels();
	if channels.is_empty() {
		return 0.0;
	}
	channels
		.iter()
		.map(|v| v.as_u32() as f32 / VOLUME_NORM)
		.sum::<f32>()
		/ channels.len() as f32
}

/// "ALSA plug-in [java]" (apps playing through the ALSA compatibility layer) → "java".
fn clean_app_name(name: &str) -> String {
	name.strip_prefix("ALSA plug-in [")
		.and_then(|rest| rest.strip_suffix(']'))
		.unwrap_or(name)
		.to_owned()
}

fn capitalize(s: &str) -> String {
	let mut chars = s.chars();
	match chars.next() {
		Some(first) => first.to_uppercase().chain(chars).collect(),
		None => String::new(),
	}
}

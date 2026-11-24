//! Platform-independent view of the system mixer.
//!
//! Each platform module implements [`AudioBackend`]. All methods are blocking and are
//! called from `tokio::task::spawn_blocking` (see [`crate::state`]).

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

/// [`AppStream::key`] shared by desktop-environment and system sounds, shown as "System".
/// Which apps count as system is set in `app-names.json` (see [`crate::names`]).
pub const SYSTEM_KEY: &str = "system";

/// Name to show for an app: system sounds get the translated "System".
pub fn display_name<'a>(key: &str, name: &'a str) -> &'a str {
	if key == SYSTEM_KEY {
		crate::i18n::t("system")
	} else {
		name
	}
}

/// Volume is linear in the mixer's own scale: `1.0` is 100 %.
pub type Volume = f32;

/// One application, which may own several audio streams (all controlled together).
#[derive(Clone, Debug, PartialEq)]
pub struct AppStream {
	/// Stable identifier used to remember the app across restarts (lowercase binary name).
	pub key: String,
	/// Human-readable name shown on the LCD.
	pub name: String,
	/// Hints used to find the app's icon.
	pub icon: IconHint,
	/// Current volume (`1.0` = 100 %).
	pub volume: Volume,
	pub muted: bool,
	/// [`OutputDevice::id`] the app currently plays to, if known.
	pub output_id: Option<String>,
	/// Names the app reports (app id, executable, stream name…), as matched against
	/// `app-names.json`. Only used for diagnostics (`--list`).
	pub reported: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct IconHint {
	/// Candidate icon names (freedesktop icon theme names, `.desktop` ids, binary names) or absolute paths.
	pub names: Vec<String>,
	/// Executable path (Windows: the icon is extracted from it).
	pub exe_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutputDevice {
	/// Backend-specific identifier (PulseAudio sink name, Windows endpoint id).
	pub id: String,
	/// Human-readable name shown on the LCD.
	pub name: String,
}

#[derive(Debug)]
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub enum AudioError {
	/// The mixer cannot be reached at all (e.g. no PulseAudio socket inside the Flatpak sandbox).
	Unavailable(String),
	/// The platform has no per-application mixer support.
	#[cfg_attr(any(target_os = "linux", target_os = "windows"), allow(dead_code))]
	Unsupported,
	/// The requested app or device does not exist (anymore).
	NotFound,
	Other(String),
}

impl fmt::Display for AudioError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			AudioError::Unavailable(reason) => write!(f, "audio server unavailable: {reason}"),
			AudioError::Unsupported => write!(
				f,
				"per-application volume is not supported on this platform"
			),
			AudioError::NotFound => write!(f, "application or device not found"),
			AudioError::Other(reason) => write!(f, "{reason}"),
		}
	}
}

impl std::error::Error for AudioError {}

pub type AudioResult<T> = Result<T, AudioError>;

pub trait AudioBackend: Send + Sync + 'static {
	/// Applications currently owning at least one playback stream, sorted by name.
	fn apps(&self) -> AudioResult<Vec<AppStream>>;

	/// Available playback devices.
	fn outputs(&self) -> AudioResult<Vec<OutputDevice>>;

	/// Set the volume of every stream of the app.
	fn set_volume(&self, app_key: &str, volume: Volume) -> AudioResult<()>;

	/// Mute or unmute every stream of the app.
	fn set_muted(&self, app_key: &str, muted: bool) -> AudioResult<()>;

	/// Route every stream of the app to the given output device.
	fn move_to_output(&self, app_key: &str, output_id: &str) -> AudioResult<()>;

	/// Highest volume the backend accepts (`1.0` when it cannot amplify).
	fn max_volume(&self) -> Volume;

	/// Start watching the mixer and call `on_change` whenever something changes.
	/// Backends without change notifications rely on the periodic refresh instead.
	fn watch(&self, _on_change: Arc<dyn Fn() + Send + Sync>) {}
}

/// Create the backend for the current platform.
pub fn backend() -> Arc<dyn AudioBackend> {
	#[cfg(target_os = "linux")]
	return Arc::new(linux::PulseBackend::new());
	#[cfg(target_os = "windows")]
	return Arc::new(windows::WasapiBackend::new());
	#[cfg(not(any(target_os = "linux", target_os = "windows")))]
	return Arc::new(unsupported::UnsupportedBackend);
}

/// Group streams that belong to the same app and sort the result by display name.
/// Shared by backends that discover one entry per stream.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn group_streams(streams: Vec<AppStream>) -> Vec<AppStream> {
	let mut apps: Vec<AppStream> = Vec::new();
	for stream in streams {
		if let Some(app) = apps.iter_mut().find(|a| a.key == stream.key) {
			// The loudest stream represents the app; a single unmuted stream makes it unmuted.
			app.volume = app.volume.max(stream.volume);
			app.muted &= stream.muted;
			if app.output_id.is_none() {
				app.output_id = stream.output_id;
			}
			for name in stream.icon.names {
				if !app.icon.names.contains(&name) {
					app.icon.names.push(name);
				}
			}
			if app.icon.exe_path.is_none() {
				app.icon.exe_path = stream.icon.exe_path;
			}
			for name in stream.reported {
				if !app.reported.contains(&name) {
					app.reported.push(name);
				}
			}
		} else {
			apps.push(stream);
		}
	}
	apps.sort_by(|a, b| {
		a.name
			.to_lowercase()
			.cmp(&b.name.to_lowercase())
			.then(a.key.cmp(&b.key))
	});
	apps
}

#[cfg(test)]
mod tests {
	use super::*;

	fn stream(key: &str, name: &str, volume: Volume, muted: bool) -> AppStream {
		AppStream {
			key: key.into(),
			name: name.into(),
			icon: IconHint {
				names: vec![key.into()],
				exe_path: None,
			},
			volume,
			muted,
			output_id: None,
			reported: Vec::new(),
		}
	}

	#[test]
	fn groups_streams_of_the_same_app() {
		let apps = group_streams(vec![
			stream("firefox", "Firefox", 0.4, true),
			stream("discord", "Discord", 1.0, false),
			stream("firefox", "Firefox", 0.8, false),
		]);
		assert_eq!(apps.len(), 2);
		assert_eq!(apps[0].key, "discord");
		assert_eq!(apps[1].key, "firefox");
		assert_eq!(apps[1].volume, 0.8);
		assert!(!apps[1].muted);
	}
}

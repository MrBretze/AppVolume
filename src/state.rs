//! Plugin-wide state: the latest mixer snapshot, icon cache, per-dial state, and the
//! code that turns them into LCD feedback.

use crate::audio::{self, AppStream, AudioBackend, AudioError, OutputDevice, Volume};
use crate::dial::{Dial, Mode};
use crate::i18n::t;
use crate::icons;
use crate::render::{self, OutputLine, Screen, VolumeLine};

use std::collections::{HashMap, HashSet};
use std::sync::mpsc;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use openaction::{Action, Instance, InstanceId};
use serde_json::{Map, Value};
use tokio::sync::Notify;

/// After a local volume change, ignore the mixer's (possibly older) value for this long,
/// so the bar does not jump back while the dial is being turned.
const PENDING_VOLUME_HOLD: Duration = Duration::from_millis(1000);

pub static PLUGIN: LazyLock<Plugin> = LazyLock::new(Plugin::new);

pub struct Plugin {
	pub backend: Arc<dyn AudioBackend>,
	snapshot: RwLock<Snapshot>,
	pub dials: DashMap<InstanceId, Dial>,
	/// App key → icon data URL.
	icons: DashMap<String, String>,
	fallback_icon: String,
	/// Frames of the "output changed" animation, rendered once.
	check_frames: Vec<String>,
	/// App key → volume sent to the mixer and when.
	pending_volumes: DashMap<String, (Volume, Instant)>,
	volume_writer: mpsc::Sender<(String, Volume)>,
	/// Signalled when the mixer reports a change.
	pub changed: Arc<Notify>,
}

#[derive(Default)]
pub struct Snapshot {
	pub apps: Vec<AppStream>,
	pub outputs: Vec<OutputDevice>,
	pub status: Status,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Status {
	#[default]
	Ok,
	/// The audio server cannot be reached (e.g. Flatpak sandbox without audio access).
	Unavailable,
	/// No per-app mixer support on this platform.
	Unsupported,
}

impl Plugin {
	fn new() -> Self {
		let backend = audio::backend();
		Self {
			volume_writer: spawn_volume_writer(backend.clone()),
			backend,
			snapshot: RwLock::default(),
			dials: DashMap::new(),
			icons: DashMap::new(),
			fallback_icon: icons::fallback_icon(),
			check_frames: icons::check_frames(),
			pending_volumes: DashMap::new(),
			changed: Arc::new(Notify::new()),
		}
	}

	/// Highest volume in percent: the user's setting, limited by what the mixer accepts.
	pub fn max_percent(&self) -> u32 {
		let backend = (self.backend.max_volume() * 100.0).round() as u32;
		crate::config::get().max_volume.min(backend)
	}

	/// Apps with locally pending volumes applied.
	pub fn apps(&self) -> Vec<AppStream> {
		let mut apps = self
			.snapshot
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.apps
			.clone();
		for app in &mut apps {
			if let Some(entry) = self.pending_volumes.get(&app.key)
				&& entry.1.elapsed() < PENDING_VOLUME_HOLD
			{
				app.volume = entry.0;
			}
		}
		apps
	}

	pub fn outputs(&self) -> Vec<OutputDevice> {
		self.snapshot
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.outputs
			.clone()
	}

	fn status(&self) -> Status {
		self.snapshot
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.status
	}

	/// Queue a volume change; the latest value per app wins if several are queued.
	pub fn set_volume(&self, app_key: &str, volume: Volume) {
		let volume = volume.clamp(0.0, self.max_percent() as f32 / 100.0);
		self.pending_volumes
			.insert(app_key.to_owned(), (volume, Instant::now()));
		if self
			.volume_writer
			.send((app_key.to_owned(), volume))
			.is_err()
		{
			log::error!("Volume writer thread is gone");
		}
	}

	fn icon(&self, app_key: &str) -> String {
		self.icons
			.get(app_key)
			.map(|i| i.clone())
			.unwrap_or_else(|| self.fallback_icon.clone())
	}
}

/// Apply volume changes in order on one thread, coalescing bursts from fast dial turns.
fn spawn_volume_writer(backend: Arc<dyn AudioBackend>) -> mpsc::Sender<(String, Volume)> {
	let (tx, rx) = mpsc::channel::<(String, Volume)>();
	let spawned = std::thread::Builder::new()
		.name("volume-writer".into())
		.spawn(move || {
			while let Ok(first) = rx.recv() {
				let mut latest: Vec<(String, Volume)> = vec![first];
				while let Ok((key, volume)) = rx.try_recv() {
					match latest.iter_mut().find(|(k, _)| *k == key) {
						Some(entry) => entry.1 = volume,
						None => latest.push((key, volume)),
					}
				}
				for (key, volume) in latest {
					match backend.set_volume(&key, volume) {
						Ok(()) => log::debug!("Mixer: {key} set to {:.0}%", volume * 100.0),
						Err(error) => log::warn!("Could not set the volume of {key}: {error}"),
					}
				}
			}
		});
	if let Err(error) = spawned {
		log::error!("Could not start the volume writer thread: {error}");
	}
	tx
}

/// Re-read the mixer and load icons of newly seen apps.
pub async fn refresh() {
	let backend = PLUGIN.backend.clone();
	let known_icons: HashSet<String> = PLUGIN.icons.iter().map(|e| e.key().clone()).collect();
	let result = tokio::task::spawn_blocking(move || {
		crate::names::reload_if_changed();
		let apps = backend.apps();
		let outputs = backend.outputs();
		let new_icons: HashMap<String, String> = match &apps {
			Ok(apps) => apps
				.iter()
				.filter(|a| !known_icons.contains(&a.key))
				.map(|a| (a.key.clone(), icons::app_icon(&a.icon)))
				.collect(),
			Err(_) => HashMap::new(),
		};
		(apps, outputs, new_icons)
	})
	.await;

	let Ok((apps, outputs, new_icons)) = result else {
		log::error!("Mixer refresh task panicked");
		return;
	};
	for (key, icon) in new_icons {
		PLUGIN.icons.insert(key, icon);
	}

	let mut snapshot = PLUGIN.snapshot.write().unwrap_or_else(|e| e.into_inner());
	match (apps, outputs) {
		(Ok(apps), Ok(outputs)) => {
			if snapshot.status != Status::Ok {
				log::info!("Connected to the mixer");
			}
			// Log the app list only when it changes: the refresh runs every 2 s.
			let keys = |apps: &[AppStream]| apps.iter().map(|a| a.key.clone()).collect::<Vec<_>>();
			if keys(&snapshot.apps) != keys(&apps) || snapshot.outputs.len() != outputs.len() {
				log::debug!(
					"Mixer: apps {:?}, {} outputs",
					apps.iter()
						.map(|a| format!("{} {:.0}%", a.key, a.volume * 100.0))
						.collect::<Vec<_>>(),
					outputs.len()
				);
			}
			*snapshot = Snapshot {
				apps,
				outputs,
				status: Status::Ok,
			};
		}
		(Err(error), _) | (_, Err(error)) => {
			let status = match error {
				AudioError::Unsupported => Status::Unsupported,
				_ => Status::Unavailable,
			};
			if snapshot.status != status {
				log::warn!("Mixer unavailable: {error}");
			}
			*snapshot = Snapshot {
				status,
				..Snapshot::default()
			};
		}
	}
	drop(snapshot);
	PLUGIN
		.pending_volumes
		.retain(|_, (_, at)| at.elapsed() < PENDING_VOLUME_HOLD);
}

/// Bring a dial's LCD up to date, sending only what changed since last time.
pub async fn redraw(instance: &Instance) {
	let apps = PLUGIN.apps();
	let outputs = PLUGIN.outputs();
	let status = PLUGIN.status();

	let (new_layout, diff) = {
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		let screen = screen_for(&dial, &apps, &outputs, status);
		let new_layout = (dial.sent_layout != Some(screen.layout)).then_some(screen.layout);
		if new_layout.is_some() {
			// Switching layouts resets every item, so everything must be sent again.
			dial.sent.clear();
			dial.sent_layout = Some(screen.layout);
		}
		let diff: Map<String, Value> = screen
			.feedback
			.into_iter()
			.filter(|(k, v)| dial.sent.get(k) != Some(v))
			.collect();
		for (k, v) in &diff {
			dial.sent.insert(k.clone(), v.clone());
		}
		(new_layout, diff)
	};

	if new_layout.is_some() || !diff.is_empty() {
		// Values are left out: the icon is a long data URL.
		log::debug!(
			"Redraw {}: layout {:?}, keys {:?}",
			instance.instance_id,
			new_layout,
			diff.keys().collect::<Vec<_>>()
		);
	}
	let mut result = Ok(());
	if let Some(layout) = new_layout {
		result = instance.set_feedback_layout(layout.to_owned()).await;
	}
	if result.is_ok() && !diff.is_empty() {
		result = instance.set_feedback(&Value::Object(diff)).await;
	}
	if let Err(error) = result {
		log::warn!("Could not update dial {}: {error}", instance.instance_id);
		forget_sent(&instance.instance_id);
	}
}

/// Make the next [`redraw`] send the whole screen again.
pub fn forget_sent(instance_id: &str) {
	if let Some(mut dial) = PLUGIN.dials.get_mut(instance_id) {
		dial.forget_sent();
	}
}

/// [`forget_sent`] for every dial.
///
/// Never call `PLUGIN.dials.get_mut` while iterating `PLUGIN.dials`: DashMap would
/// deadlock on the shard lock the iterator holds.
pub fn forget_all_sent() {
	for mut dial in PLUGIN.dials.iter_mut() {
		dial.forget_sent();
	}
}

fn screen_for(dial: &Dial, apps: &[AppStream], outputs: &[OutputDevice], status: Status) -> Screen {
	let max_percent = PLUGIN.max_percent();
	let fallback = &PLUGIN.fallback_icon;
	match status {
		Status::Unavailable => {
			return render::volume_screen(
				fallback,
				t("no_audio_access"),
				VolumeLine::Message(t("see_readme")),
				max_percent,
			);
		}
		Status::Unsupported => {
			return render::volume_screen(
				fallback,
				t("not_supported"),
				VolumeLine::Message(t("on_this_os")),
				max_percent,
			);
		}
		Status::Ok => {}
	}

	if let Some(confirm) = &dial.confirm {
		let frame = PLUGIN
			.check_frames
			.get(confirm.frame())
			.map(String::as_str)
			.unwrap_or("");
		return render::confirm_screen(frame, &confirm.output_name);
	}

	let selected = dial.selected_app(apps);
	match (dial.mode, selected) {
		(Mode::Output, Some(app)) => {
			let line = outputs.get(dial.output_cursor).map(|o| OutputLine {
				name: &o.name,
				index: dial.output_cursor,
				count: outputs.len(),
				is_current: app.output_id.as_deref() == Some(o.id.as_str()),
			});
			render::output_screen(&PLUGIN.icon(&app.key), line)
		}
		(_, Some(app)) => render::volume_screen(
			&PLUGIN.icon(&app.key),
			audio::display_name(&app.key, &app.name),
			VolumeLine::Level {
				volume: app.volume,
				muted: app.muted,
			},
			max_percent,
		),
		(_, None) => match dial.remembered_app() {
			Some((key, name)) => render::volume_screen(
				&PLUGIN.icon(key),
				audio::display_name(key, name),
				VolumeLine::Message(t("not_playing")),
				max_percent,
			),
			None => render::volume_screen(
				fallback,
				t("no_audio"),
				VolumeLine::Message(t("nothing_playing")),
				max_percent,
			),
		},
	}
}

/// Redraw every visible dial.
pub async fn redraw_all() {
	for instance in openaction::visible_instances(crate::action::AppVolumeAction::UUID).await {
		redraw(&instance).await;
	}
}

//! Per-dial state and what pressing / turning a dial does.
//!
//! Volume mode: turn = volume, click = next app, long press = output mode.
//! Output mode: turn = browse outputs, click = move the app there, long press = cancel.

use crate::action::{ClickAction, Settings};
use crate::audio::AppStream;
use crate::config;
use crate::state::{self, PLUGIN};

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use openaction::Instance;
use serde_json::{Map, Value};

/// Time between two frames of the "output changed" animation.
const CONFIRM_FRAME: Duration = Duration::from_millis(40);
/// How long the "output changed" screen stays (animation, then the result held still).
const CONFIRM_DURATION: Duration = Duration::from_millis(1400);

static PRESS_IDS: AtomicU64 = AtomicU64::new(0);
static CONFIRM_IDS: AtomicU64 = AtomicU64::new(0);

/// "Output changed" screen shown after a successful output change.
pub struct Confirmation {
	id: u64,
	pub output_name: String,
	started: Instant,
}

impl Confirmation {
	/// Index into the check animation frames.
	pub fn frame(&self) -> usize {
		let frame = self.started.elapsed().as_millis() / CONFIRM_FRAME.as_millis();
		(frame as usize).min(crate::icons::CHECK_FRAME_COUNT - 1)
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
	Volume,
	Output,
}

pub struct Dial {
	pub mode: Mode,
	/// App chosen by the user (persisted in the action settings).
	app_key: Option<String>,
	app_name: Option<String>,
	click: ClickAction,
	/// Which app to show while none was chosen, so dials start on different apps.
	default_slot: usize,
	/// Index into the output list while in output mode.
	pub output_cursor: usize,
	press: Option<Press>,
	last_input: Instant,
	/// Shown instead of the volume while set (see [`animate_confirmation`]).
	pub confirm: Option<Confirmation>,
	/// Layout and feedback values last sent to the device (see [`state::redraw`]).
	pub(crate) sent_layout: Option<&'static str>,
	pub(crate) sent: Map<String, Value>,
}

struct Press {
	id: u64,
	/// The dial was turned while held: neither a click nor a long press.
	rotated: bool,
	long_fired: bool,
}

impl Dial {
	pub fn new(settings: &Settings, default_slot: usize) -> Self {
		Self {
			mode: Mode::Volume,
			app_key: settings.app.clone(),
			app_name: settings.app_name.clone(),
			click: settings.click,
			default_slot,
			output_cursor: 0,
			press: None,
			last_input: Instant::now(),
			confirm: None,
			sent_layout: None,
			sent: Map::new(),
		}
	}

	/// The app this dial controls, if it is currently playing.
	pub fn selected_app<'a>(&self, apps: &'a [AppStream]) -> Option<&'a AppStream> {
		match &self.app_key {
			Some(key) => apps.iter().find(|a| &a.key == key),
			None if apps.is_empty() => None,
			None => Some(&apps[self.default_slot % apps.len()]),
		}
	}

	/// Key and display name of the chosen app, even when it is not playing.
	pub fn remembered_app(&self) -> Option<(&str, &str)> {
		let key = self.app_key.as_deref()?;
		Some((key, self.app_name.as_deref().unwrap_or(key)))
	}

	pub fn forget_sent(&mut self) {
		self.sent.clear();
		self.sent_layout = None;
	}

	/// Settings changed in the property inspector.
	pub fn apply_settings(&mut self, settings: &Settings) {
		self.app_key = settings.app.clone();
		self.app_name = settings.app_name.clone();
		self.click = settings.click;
	}

	fn settings(&self) -> Settings {
		Settings {
			app: self.app_key.clone(),
			app_name: self.app_name.clone(),
			click: self.click,
		}
	}
}

pub async fn on_rotate(instance: &Instance, ticks: i16, pressed: bool) {
	let apps = PLUGIN.apps();
	let outputs_len = PLUGIN.outputs().len();
	let volume_change = {
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		dial.last_input = Instant::now();
		dial.confirm = None;
		let held = pressed || dial.press.is_some();
		if let Some(press) = dial.press.as_mut() {
			press.rotated = true;
		}
		match dial.mode {
			Mode::Volume => {
				let change = dial.selected_app(&apps).map(|app| {
					let config = config::get();
					let step = if held {
						config.fine_step
					} else {
						config.volume_step
					} as i32;
					let percent = (app.volume * 100.0).round() as i32 + i32::from(ticks) * step;
					log::debug!(
						"Volume {}: {:.0}% -> {}% (step {step})",
						app.key,
						app.volume * 100.0,
						percent.max(0)
					);
					(app.key.clone(), percent.max(0) as f32 / 100.0)
				});
				if change.is_none() {
					log::debug!("Rotate ignored: no app playing for this dial");
				}
				change
			}
			Mode::Output => {
				if outputs_len > 0 {
					let cursor = dial.output_cursor as i64 + i64::from(ticks);
					dial.output_cursor = cursor.rem_euclid(outputs_len as i64) as usize;
				}
				log::debug!("Output cursor -> {}/{outputs_len}", dial.output_cursor + 1);
				None
			}
		}
	};
	if let Some((key, volume)) = volume_change {
		PLUGIN.set_volume(&key, volume);
	}
	// Other dials may show the same app.
	state::redraw_all().await;
}

pub async fn on_down(instance: &Instance) {
	let id = PRESS_IDS.fetch_add(1, Ordering::Relaxed);
	{
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		dial.last_input = Instant::now();
		dial.confirm = None;
		dial.press = Some(Press {
			id,
			rotated: false,
			long_fired: false,
		});
	}

	// Fire the long press while the dial is still held, so the user sees the mode change.
	let instance_id = instance.instance_id.clone();
	tokio::spawn(async move {
		tokio::time::sleep(config::get().long_press()).await;
		let fire =
			PLUGIN
				.dials
				.get_mut(&instance_id)
				.is_some_and(|mut dial| match dial.press.as_mut() {
					Some(press) if press.id == id && !press.rotated => {
						press.long_fired = true;
						true
					}
					_ => false,
				});
		if fire && let Some(instance) = openaction::get_instance(instance_id).await {
			log::debug!("Long press on {}", instance.instance_id);
			on_long_press(&instance).await;
		}
	});
}

pub async fn on_up(instance: &Instance) {
	let press = PLUGIN
		.dials
		.get_mut(&instance.instance_id)
		.and_then(|mut dial| dial.press.take());
	match press {
		Some(press) if !press.rotated && !press.long_fired => {
			log::debug!("Click on {}", instance.instance_id);
			on_click(instance).await;
		}
		Some(press) => log::debug!(
			"Release ignored (rotated: {}, long press: {})",
			press.rotated,
			press.long_fired
		),
		None => log::debug!("Release without a press"),
	}
}

async fn on_click(instance: &Instance) {
	let (mode, click) = match PLUGIN.dials.get(&instance.instance_id) {
		Some(dial) => (dial.mode, dial.click),
		None => return,
	};
	match (mode, click) {
		(Mode::Volume, ClickAction::NextApp) => select_next_app(instance).await,
		(Mode::Volume, ClickAction::Mute) => toggle_mute(instance).await,
		(Mode::Output, _) => apply_output(instance).await,
	}
}

async fn toggle_mute(instance: &Instance) {
	let apps = PLUGIN.apps();
	let target = PLUGIN
		.dials
		.get(&instance.instance_id)
		.and_then(|dial| dial.selected_app(&apps).map(|a| (a.key.clone(), !a.muted)));
	let Some((key, muted)) = target else {
		log::debug!("Mute: no app playing for this dial");
		let _ = instance.show_alert().await;
		return;
	};
	log::debug!("Mute {key}: {muted}");
	let backend = PLUGIN.backend.clone();
	match tokio::task::spawn_blocking(move || backend.set_muted(&key, muted)).await {
		Ok(Ok(())) => {}
		Ok(Err(error)) => {
			log::warn!("Could not change mute: {error}");
			let _ = instance.show_alert().await;
		}
		Err(_) => log::error!("Mute task panicked"),
	}
	PLUGIN.changed.notify_one();
}

async fn on_long_press(instance: &Instance) {
	let apps = PLUGIN.apps();
	let outputs = PLUGIN.outputs();
	let entered = {
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		dial.last_input = Instant::now();
		match dial.mode {
			Mode::Output => {
				log::debug!("Output mode cancelled");
				dial.mode = Mode::Volume;
				true
			}
			Mode::Volume => match dial.selected_app(&apps) {
				Some(app) => {
					let current = app.output_id.as_deref();
					dial.output_cursor = outputs
						.iter()
						.position(|o| Some(o.id.as_str()) == current)
						.unwrap_or(0);
					log::debug!(
						"Output mode for {} (current output: {current:?}, {} outputs)",
						app.key,
						outputs.len()
					);
					dial.mode = Mode::Output;
					true
				}
				None => {
					log::debug!("Output mode refused: no app playing for this dial");
					false
				}
			},
		}
	};
	if entered {
		state::redraw(instance).await;
	} else {
		let _ = instance.show_alert().await;
	}
}

async fn select_next_app(instance: &Instance) {
	let apps = PLUGIN.apps();
	if apps.is_empty() {
		log::debug!("Next app: nothing is playing");
		let _ = instance.show_alert().await;
		return;
	}
	let settings = {
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		let next = match dial
			.selected_app(&apps)
			.and_then(|a| apps.iter().position(|b| b.key == a.key))
		{
			Some(i) => (i + 1) % apps.len(),
			None => 0,
		};
		log::debug!("Next app: {} ({}/{})", apps[next].key, next + 1, apps.len());
		dial.app_key = Some(apps[next].key.clone());
		dial.app_name = Some(apps[next].name.clone());
		dial.settings()
	};
	if let Err(error) = instance.set_settings(&settings).await {
		log::warn!("Could not save the selected app: {error}");
	}
	state::redraw(instance).await;
}

async fn apply_output(instance: &Instance) {
	let apps = PLUGIN.apps();
	let outputs = PLUGIN.outputs();
	let target = {
		let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) else {
			return;
		};
		dial.mode = Mode::Volume;
		let app = dial.selected_app(&apps).map(|a| a.key.clone());
		let output = outputs
			.get(dial.output_cursor)
			.map(|o| (o.id.clone(), o.name.clone()));
		app.zip(output)
	};

	let Some((app_key, (output_id, output_name))) = target else {
		log::debug!("Apply output: no app or no output selected");
		state::redraw(instance).await;
		let _ = instance.show_alert().await;
		return;
	};
	log::debug!("Moving {app_key} to {output_id}");
	let backend = PLUGIN.backend.clone();
	let result =
		tokio::task::spawn_blocking(move || backend.move_to_output(&app_key, &output_id)).await;
	match result {
		Ok(Ok(())) => {
			let id = CONFIRM_IDS.fetch_add(1, Ordering::Relaxed);
			if let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) {
				dial.confirm = Some(Confirmation {
					id,
					output_name,
					started: Instant::now(),
				});
			}
			tokio::spawn(animate_confirmation(instance.instance_id.clone(), id));
		}
		Ok(Err(error)) => {
			log::warn!("Could not change the output: {error}");
			state::redraw(instance).await;
			let _ = instance.show_alert().await;
		}
		Err(_) => log::error!("Output change task panicked"),
	}
	PLUGIN.changed.notify_one();
}

/// Play the "output changed" animation, then go back to the volume screen.
/// Stops early if the confirmation was replaced or cancelled by input.
async fn animate_confirmation(instance_id: String, id: u64) {
	loop {
		let finished = match PLUGIN.dials.get_mut(&instance_id) {
			Some(mut dial) => match &dial.confirm {
				Some(c) if c.id == id => {
					let done = c.started.elapsed() >= CONFIRM_DURATION;
					if done {
						dial.confirm = None;
					}
					done
				}
				// Cancelled by input or replaced by a newer confirmation.
				_ => return,
			},
			None => return,
		};
		if let Some(instance) = openaction::get_instance(instance_id.clone()).await {
			state::redraw(&instance).await;
		}
		if finished {
			return;
		}
		tokio::time::sleep(CONFIRM_FRAME).await;
	}
}

/// Leave output mode on dials that have been idle for a while.
pub fn expire_output_modes() {
	let timeout = config::get().output_timeout();
	for mut dial in PLUGIN.dials.iter_mut() {
		if dial.mode == Mode::Output && dial.press.is_none() && dial.last_input.elapsed() > timeout
		{
			log::debug!("Output mode timed out");
			dial.mode = Mode::Volume;
		}
	}
}

//! The single action of the plugin: one per-application volume dial.

use crate::dial::{self, Dial};
use crate::state::{self, PLUGIN};

use openaction::*;
use serde::{Deserialize, Serialize};

/// Per-dial settings saved by OpenDeck.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
	/// Key of the app the dial controls (see [`crate::audio::AppStream::key`]).
	pub app: Option<String>,
	/// Its display name, shown while the app is not playing.
	pub app_name: Option<String>,
	/// What a short click does in volume mode.
	pub click: ClickAction,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClickAction {
	/// Switch to the next app that is playing audio.
	#[default]
	NextApp,
	/// Mute / unmute the app.
	Mute,
}

/// Sent to the property inspector so it can list the apps and adapt its options.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InspectorState<'a> {
	event: &'static str,
	apps: Vec<InspectorApp<'a>>,
	/// Highest volume the mixer accepts, in percent (100 on Windows, 150 on Linux).
	backend_max_volume: u32,
	platform: &'static str,
}

#[derive(Serialize)]
struct InspectorApp<'a> {
	key: &'a str,
	name: &'a str,
}

async fn send_inspector_state(instance: &Instance) {
	let apps = PLUGIN.apps();
	let state = InspectorState {
		event: "appList",
		apps: apps
			.iter()
			.map(|a| InspectorApp {
				key: &a.key,
				name: crate::audio::display_name(&a.key, &a.name),
			})
			.collect(),
		backend_max_volume: (PLUGIN.backend.max_volume() * 100.0).round() as u32,
		platform: std::env::consts::OS,
	};
	if let Err(error) = instance.send_to_property_inspector(&state).await {
		log::warn!("Could not reach the property inspector: {error}");
	}
}

pub struct AppVolumeAction;

#[async_trait]
impl Action for AppVolumeAction {
	const UUID: ActionUuid = "com.mrbretzel.appvolume.dial";
	type Settings = Settings;

	async fn will_appear(
		&self,
		instance: &Instance,
		settings: &Self::Settings,
	) -> OpenActionResult<()> {
		let slot = instance.coordinates.map(|c| c.column as usize).unwrap_or(0);
		log::info!(
			"Dial {} appeared (app: {:?})",
			instance.instance_id,
			settings.app
		);
		PLUGIN
			.dials
			.insert(instance.instance_id.clone(), Dial::new(settings, slot));
		state::redraw(instance).await;
		Ok(())
	}

	async fn will_disappear(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
	) -> OpenActionResult<()> {
		log::debug!("Dial {} disappeared", instance.instance_id);
		PLUGIN.dials.remove(&instance.instance_id);
		Ok(())
	}

	async fn dial_rotate(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
		ticks: i16,
		pressed: bool,
	) -> OpenActionResult<()> {
		log::debug!(
			"Dial {} rotate ticks={ticks} pressed={pressed}",
			instance.instance_id
		);
		dial::on_rotate(instance, ticks, pressed).await;
		Ok(())
	}

	async fn dial_down(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
	) -> OpenActionResult<()> {
		log::debug!("Dial {} down", instance.instance_id);
		dial::on_down(instance).await;
		Ok(())
	}

	async fn dial_up(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
	) -> OpenActionResult<()> {
		log::debug!("Dial {} up", instance.instance_id);
		dial::on_up(instance).await;
		Ok(())
	}

	async fn did_receive_settings(
		&self,
		instance: &Instance,
		settings: &Self::Settings,
	) -> OpenActionResult<()> {
		log::debug!("Dial {} settings: {settings:?}", instance.instance_id);
		if let Some(mut dial) = PLUGIN.dials.get_mut(&instance.instance_id) {
			dial.apply_settings(settings);
		}
		state::redraw(instance).await;
		Ok(())
	}

	async fn property_inspector_did_appear(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
	) -> OpenActionResult<()> {
		send_inspector_state(instance).await;
		Ok(())
	}

	async fn send_to_plugin(
		&self,
		instance: &Instance,
		_settings: &Self::Settings,
		payload: &serde_json::Value,
	) -> OpenActionResult<()> {
		if payload["cmd"] == "getApps" {
			send_inspector_state(instance).await;
		}
		Ok(())
	}
}

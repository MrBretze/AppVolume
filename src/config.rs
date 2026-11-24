//! Plugin-wide settings, edited in the property inspector and stored by OpenDeck as the
//! plugin's global settings. Per-dial settings are [`crate::action::Settings`].

use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Debug logs when the user never saved the setting. Handy while developing.
pub const DEBUG_DEFAULT: bool = false;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
	/// Volume change per dial tick, in percent.
	pub volume_step: u32,
	/// Volume change per tick while the dial is held down, in percent.
	pub fine_step: u32,
	/// Highest volume the dial goes to, in percent (above 100 only where the mixer allows it).
	pub max_volume: u32,
	/// Hold time for a long press, in milliseconds.
	pub long_press_ms: u64,
	/// Output mode closes after this many seconds without input.
	pub output_timeout_s: u64,
	/// LCD language: "auto" (OpenDeck's language), "en" or "fr".
	pub language: String,
	/// Write debug logs to the plugin log.
	pub debug: bool,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			volume_step: 5,
			fine_step: 1,
			max_volume: 150,
			long_press_ms: 500,
			output_timeout_s: 30,
			language: "auto".to_owned(),
			debug: DEBUG_DEFAULT,
		}
	}
}

impl Config {
	/// Keep every value in a usable range, whatever the settings file contains.
	fn sanitized(mut self) -> Self {
		self.volume_step = self.volume_step.clamp(1, 25);
		self.fine_step = self.fine_step.clamp(1, 10);
		self.max_volume = self.max_volume.clamp(100, 150);
		self.long_press_ms = self.long_press_ms.clamp(200, 2000);
		self.output_timeout_s = self.output_timeout_s.clamp(5, 300);
		if !matches!(self.language.as_str(), "auto" | "en" | "fr") {
			self.language = "auto".to_owned();
		}
		self
	}

	pub fn long_press(&self) -> Duration {
		Duration::from_millis(self.long_press_ms)
	}

	pub fn output_timeout(&self) -> Duration {
		Duration::from_secs(self.output_timeout_s)
	}
}

static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(RwLock::default);

/// Current settings (a copy: never hold a lock across an `.await`).
pub fn get() -> Config {
	CONFIG.read().map(|c| c.clone()).unwrap_or_default()
}

/// Apply settings received from OpenDeck. Returns the sanitized settings.
pub fn set(value: &serde_json::Value) -> Config {
	let config = serde_json::from_value::<Config>(value.clone())
		.unwrap_or_else(|error| {
			log::warn!("Invalid global settings ({error}), using defaults");
			Config::default()
		})
		.sanitized();
	if let Ok(mut guard) = CONFIG.write() {
		*guard = config.clone();
	}
	apply_log_level(config.debug);
	crate::i18n::set_preference(&config.language);
	log::info!("Settings: {config:?}");
	config
}

pub fn apply_log_level(debug: bool) {
	log::set_max_level(if debug {
		log::LevelFilter::Debug
	} else {
		log::LevelFilter::Info
	});
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn missing_fields_use_defaults() {
		let config: Config = serde_json::from_str(r#"{"volumeStep": 10}"#).unwrap();
		assert_eq!(config.volume_step, 10);
		assert_eq!(config.long_press_ms, 500);
	}

	#[test]
	fn out_of_range_values_are_clamped() {
		let config: Config =
			serde_json::from_str(r#"{"volumeStep": 0, "maxVolume": 400, "language": "de"}"#)
				.unwrap();
		let config = config.sanitized();
		assert_eq!(config.volume_step, 1);
		assert_eq!(config.max_volume, 150);
		assert_eq!(config.language, "auto");
	}
}

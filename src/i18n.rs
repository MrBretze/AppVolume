//! LCD text in the user's language.
//!
//! Strings live in the `"Localization"` section of `assets/lang/<language>.json` (Stream Deck
//! SDK format; the rest of those files translates the manifest). They are embedded in the
//! binary, so a missing or edited file in the installed plugin cannot break the LCD.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};

use serde::Deserialize;

const ENGLISH: &str = include_str!("../assets/lang/en.json");
const FRENCH: &str = include_str!("../assets/lang/fr.json");

#[derive(Deserialize)]
struct LangFile {
	#[serde(rename = "Localization")]
	localization: HashMap<String, String>,
}

static ENGLISH_STRINGS: LazyLock<HashMap<String, String>> = LazyLock::new(|| parse(ENGLISH));
static FRENCH_STRINGS: LazyLock<HashMap<String, String>> = LazyLock::new(|| parse(FRENCH));

/// Language OpenDeck (or the environment) reports, set once at startup.
static HOST_LANGUAGE: OnceLock<String> = OnceLock::new();
/// Whether the LCD currently uses French.
static FRENCH_SELECTED: AtomicBool = AtomicBool::new(false);

fn parse(file: &str) -> HashMap<String, String> {
	serde_json::from_str::<LangFile>(file)
		.map(|f| f.localization)
		.unwrap_or_else(|error| {
			log::error!("Invalid language file: {error}");
			HashMap::new()
		})
}

/// Read the host language from OpenDeck's registration info (`-info` argument),
/// falling back to the `LC_ALL` / `LC_MESSAGES` / `LANG` environment variables.
pub fn init(args: &[String]) {
	let language = language_from_info(args)
		.or_else(language_from_env)
		.unwrap_or_else(|| "en".to_owned());
	let _ = HOST_LANGUAGE.set(language);
	set_preference("auto");
}

/// Apply the language setting: "auto" follows the host language, otherwise a language code.
pub fn set_preference(preference: &str) {
	let language = match preference {
		"auto" => HOST_LANGUAGE.get().map(String::as_str).unwrap_or("en"),
		other => other,
	};
	FRENCH_SELECTED.store(language.starts_with("fr"), Ordering::Relaxed);
	log::info!(
		"LCD language: {} (setting: {preference})",
		if language.starts_with("fr") {
			"fr"
		} else {
			"en"
		}
	);
}

fn language_from_info(args: &[String]) -> Option<String> {
	let position = args.iter().position(|a| a.eq_ignore_ascii_case("-info"))?;
	let info: serde_json::Value = serde_json::from_str(args.get(position + 1)?).ok()?;
	info["application"]["language"]
		.as_str()
		.map(str::to_lowercase)
}

fn language_from_env() -> Option<String> {
	["LC_ALL", "LC_MESSAGES", "LANG"]
		.iter()
		.filter_map(|var| std::env::var(var).ok())
		.find(|v| !v.is_empty() && v != "C" && v != "POSIX")
		.map(|v| v.to_lowercase())
}

/// Text for `key` in the chosen language, then English, then the key itself.
pub fn t(key: &'static str) -> &'static str {
	let english = &*ENGLISH_STRINGS;
	let selected = if FRENCH_SELECTED.load(Ordering::Relaxed) {
		&*FRENCH_STRINGS
	} else {
		english
	};
	selected
		.get(key)
		.or_else(|| english.get(key))
		.map(String::as_str)
		.unwrap_or(key)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn keys(file: &str) -> Vec<String> {
		let mut keys: Vec<_> = parse(file).into_keys().collect();
		keys.sort();
		keys
	}

	#[test]
	fn languages_have_the_same_keys() {
		assert!(!keys(ENGLISH).is_empty());
		assert_eq!(keys(ENGLISH), keys(FRENCH));
	}

	#[test]
	fn reads_language_from_opendeck_info() {
		let args = [
			"-info".to_owned(),
			r#"{"application":{"language":"fr"}}"#.to_owned(),
		];
		assert_eq!(language_from_info(&args).as_deref(), Some("fr"));
	}
}

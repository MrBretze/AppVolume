//! User-editable display names: `app-names.json` next to the plugin binary.
//!
//! Some apps report unclear names to the mixer (Discord: "WEBRTC VoiceEngine", desktop
//! shells: "plasmashell"). The file maps those to the name to show, or to `@system` to
//! group them under System. It is reloaded when it changes; a copy is embedded in the
//! binary for when the file is missing or invalid.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};
use std::time::SystemTime;

use serde::Deserialize;

pub const FILE_NAME: &str = "app-names.json";
const EMBEDDED: &str = include_str!("../assets/app-names.json");
const SYSTEM_TARGET: &str = "@system";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mapped {
	/// Group under [`crate::audio::SYSTEM_KEY`].
	System,
	/// Show this name.
	Name(String),
}

#[derive(Deserialize)]
struct NamesFile {
	names: HashMap<String, String>,
}

#[derive(Default)]
struct Table {
	/// Lowercase name → target.
	exact: HashMap<String, Mapped>,
	/// Lowercase prefixes (entries ending in `*`), longest first.
	prefixes: Vec<(String, Mapped)>,
	/// Modification time of the file the table was loaded from.
	loaded_from: Option<SystemTime>,
}

static TABLE: LazyLock<RwLock<Table>> = LazyLock::new(|| RwLock::new(load()));

fn file_path() -> Option<PathBuf> {
	Some(std::env::current_exe().ok()?.parent()?.join(FILE_NAME))
}

fn modified() -> Option<SystemTime> {
	std::fs::metadata(file_path()?).ok()?.modified().ok()
}

fn parse(contents: &str) -> Result<Table, serde_json::Error> {
	let file: NamesFile = serde_json::from_str(contents)?;
	let mut table = Table::default();
	for (from, to) in file.names {
		let target = if to.trim().eq_ignore_ascii_case(SYSTEM_TARGET) {
			Mapped::System
		} else {
			Mapped::Name(to.trim().to_owned())
		};
		let from = from.trim().to_lowercase();
		match from.strip_suffix('*') {
			Some(prefix) => table.prefixes.push((prefix.to_owned(), target)),
			None => {
				table.exact.insert(from, target);
			}
		}
	}
	table
		.prefixes
		.sort_by_key(|(prefix, _)| std::cmp::Reverse(prefix.len()));
	Ok(table)
}

fn load() -> Table {
	let from_file = file_path().and_then(|path| {
		let contents = std::fs::read_to_string(&path).ok()?;
		match parse(&contents) {
			Ok(table) => {
				log::info!("Loaded app names from {}", path.display());
				Some(table)
			}
			Err(error) => {
				log::warn!("Ignoring {}: {error}", path.display());
				None
			}
		}
	});
	let mut table =
		from_file.unwrap_or_else(|| parse(EMBEDDED).expect("embedded app-names.json is valid"));
	table.loaded_from = modified();
	table
}

/// Reload the file if it was saved since it was last read. Cheap: one `stat`.
pub fn reload_if_changed() {
	let current = modified();
	let stale = TABLE
		.read()
		.map(|t| t.loaded_from != current)
		.unwrap_or(true);
	if stale {
		let table = load();
		if let Ok(mut guard) = TABLE.write() {
			*guard = table;
		}
	}
}

/// Mapping for the first candidate that has one (candidates in priority order).
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub fn lookup<'a>(candidates: impl IntoIterator<Item = &'a str>) -> Option<Mapped> {
	let table = TABLE.read().ok()?;
	candidates.into_iter().find_map(|candidate| {
		let candidate = candidate.trim().to_lowercase();
		if candidate.is_empty() {
			return None;
		}
		table.exact.get(&candidate).cloned().or_else(|| {
			table
				.prefixes
				.iter()
				.find(|(prefix, _)| candidate.starts_with(prefix.as_str()))
				.map(|(_, target)| target.clone())
		})
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn find(table: &Table, name: &str) -> Option<Mapped> {
		let name = name.to_lowercase();
		table.exact.get(&name).cloned().or_else(|| {
			table
				.prefixes
				.iter()
				.find(|(p, _)| name.starts_with(p.as_str()))
				.map(|(_, t)| t.clone())
		})
	}

	#[test]
	fn embedded_file_parses() {
		let table = parse(EMBEDDED).unwrap();
		assert_eq!(find(&table, "plasmashell"), Some(Mapped::System));
		assert_eq!(find(&table, "gsd-media-keys"), Some(Mapped::System));
		assert_eq!(
			find(&table, "WEBRTC VoiceEngine"),
			Some(Mapped::Name("Discord".into()))
		);
		assert_eq!(
			find(&table, "com.discordapp.discord"),
			Some(Mapped::Name("Discord".into()))
		);
		assert_eq!(find(&table, "firefox"), None);
	}

	#[test]
	fn longest_prefix_wins() {
		let table = parse(r#"{"names": {"foo*": "A", "foobar*": "B"}}"#).unwrap();
		assert_eq!(find(&table, "foobarbaz"), Some(Mapped::Name("B".into())));
		assert_eq!(find(&table, "fooqux"), Some(Mapped::Name("A".into())));
	}
}

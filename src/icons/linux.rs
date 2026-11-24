//! Freedesktop icon lookup: `.desktop` files and icon themes.
//!
//! Inside the OpenDeck Flatpak the host's directories are only visible when the user
//! grants access (see the README), so host paths under `/run/host` are searched too.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

const THEMES: &[&str] = &["hicolor", "Papirus", "breeze", "Adwaita"];
const SIZES: &[&str] = &[
	"256x256", "512x512", "128x128", "96x96", "64x64", "48x48", "32x32",
];

/// `XDG_DATA_DIRS`-style base directories, most specific first.
static DATA_DIRS: LazyLock<Vec<PathBuf>> = LazyLock::new(data_dirs);

static DESKTOP_ICONS: LazyLock<DesktopIcons> = LazyLock::new(index_desktop_files);

/// `Icon=` values of the installed `.desktop` files, keyed by lowercase name.
#[derive(Default)]
struct DesktopIcons {
	/// Desktop file id and `StartupWMClass`: these name the app itself.
	exact: HashMap<String, String>,
	/// Executable from `Exec=`: weaker, because launchers share one (every Steam game
	/// runs `steam steam://rungameid/…`).
	by_exec: HashMap<String, String>,
}

pub fn find_icon_file(names: &[String]) -> Option<PathBuf> {
	let found = |map: &HashMap<String, String>, name: &str| {
		map.get(&name.to_lowercase()).and_then(|i| lookup(i))
	};
	for name in names {
		if let Some(path) = found(&DESKTOP_ICONS.exact, name).or_else(|| lookup(name)) {
			return Some(path);
		}
	}
	names
		.iter()
		.find_map(|name| found(&DESKTOP_ICONS.by_exec, name))
}

/// Resolve an icon name (or absolute path) to a PNG/SVG file.
fn lookup(name: &str) -> Option<PathBuf> {
	if name.starts_with('/') {
		let path = Path::new(name);
		return path.is_file().then(|| path.to_path_buf());
	}
	if name.is_empty() || name.contains('/') {
		return None;
	}
	for base in DATA_DIRS.iter() {
		let icons = base.join("icons");
		for theme in THEMES {
			let theme = icons.join(theme);
			if !theme.is_dir() {
				continue;
			}
			let mut candidates = vec![theme.join("scalable/apps").join(format!("{name}.svg"))];
			candidates.extend(
				SIZES
					.iter()
					.map(|s| theme.join(s).join("apps").join(format!("{name}.png"))),
			);
			// Layout used by breeze and Papirus: apps/48/name.svg, 48x48/apps/name.svg
			candidates.push(theme.join("apps/48").join(format!("{name}.svg")));
			candidates.push(theme.join("48x48/apps").join(format!("{name}.svg")));
			candidates.push(theme.join("64x64/apps").join(format!("{name}.svg")));
			if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
				return Some(found);
			}
		}
		for ext in ["svg", "png"] {
			let pixmap = base.join("pixmaps").join(format!("{name}.{ext}"));
			if pixmap.is_file() {
				return Some(pixmap);
			}
		}
	}
	None
}

fn data_dirs() -> Vec<PathBuf> {
	let home = std::env::var_os("HOME").map(PathBuf::from);
	let mut dirs = Vec::new();
	let mut push = |p: PathBuf| {
		if !dirs.contains(&p) && p.is_dir() {
			dirs.push(p);
		}
	};

	if let Some(home) = &home {
		push(home.join(".local/share"));
		push(home.join(".local/share/flatpak/exports/share"));
	}
	push(PathBuf::from("/var/lib/flatpak/exports/share"));
	// Host system directories, visible inside a Flatpak with --filesystem=host-os.
	push(PathBuf::from("/run/host/usr/local/share"));
	push(PathBuf::from("/run/host/usr/share"));
	if let Ok(xdg) = std::env::var("XDG_DATA_DIRS") {
		for dir in xdg.split(':').filter(|d| !d.is_empty()) {
			push(PathBuf::from(dir));
		}
	}
	push(PathBuf::from("/usr/local/share"));
	push(PathBuf::from("/usr/share"));
	dirs
}

fn index_desktop_files() -> DesktopIcons {
	let mut map = DesktopIcons::default();
	// Reverse order so that more specific directories (user, flatpak) win.
	for base in DATA_DIRS.iter().rev() {
		let Ok(entries) = std::fs::read_dir(base.join("applications")) else {
			continue;
		};
		for entry in entries.flatten() {
			let path = entry.path();
			if path.extension().is_some_and(|e| e == "desktop")
				&& let Ok(contents) = std::fs::read_to_string(&path)
			{
				index_desktop_entry(&path, &contents, &mut map);
			}
		}
	}
	map
}

fn index_desktop_entry(path: &Path, contents: &str, map: &mut DesktopIcons) {
	let mut in_main_section = false;
	let mut icon = None;
	let mut keys = Vec::new();
	let mut exec_key = None;
	if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
		keys.push(stem.to_lowercase());
	}
	for line in contents.lines() {
		let line = line.trim();
		if line.starts_with('[') {
			in_main_section = line == "[Desktop Entry]";
			continue;
		}
		if !in_main_section {
			continue;
		}
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		let value = value.trim();
		match key.trim() {
			"Icon" => icon = Some(value.to_owned()),
			"StartupWMClass" => keys.push(value.to_lowercase()),
			"Exec" => {
				exec_key = exec_binary(value).map(|b| b.to_lowercase());
			}
			_ => {}
		}
	}
	if let Some(icon) = icon.filter(|i| !i.is_empty()) {
		if let Some(key) = exec_key {
			map.by_exec.insert(key, icon.clone());
		}
		for key in keys {
			map.exact.insert(key, icon.clone());
		}
	}
}

/// Executable name from an `Exec=` line, skipping `env VAR=x` and `flatpak run` wrappers.
fn exec_binary(exec: &str) -> Option<String> {
	let mut words = exec.split_whitespace().map(|w| w.trim_matches('"'));
	let mut word = words.next()?;
	loop {
		let name = word.rsplit('/').next().unwrap_or(word);
		match name {
			"env" => word = words.find(|w| !w.contains('='))?,
			"flatpak" => {
				// flatpak run [--options] app.id
				return words
					.find(|w| *w != "run" && !w.starts_with('-'))
					.map(str::to_owned);
			}
			_ => return Some(name.to_owned()),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn exec_binary_handles_wrappers() {
		assert_eq!(
			exec_binary("/usr/bin/firefox %u").as_deref(),
			Some("firefox")
		);
		assert_eq!(
			exec_binary("env FOO=1 BAR=2 spotify --uri=%U").as_deref(),
			Some("spotify")
		);
		assert_eq!(
			exec_binary(
				"/usr/bin/flatpak run --branch=stable --arch=x86_64 com.discordapp.Discord"
			)
			.as_deref(),
			Some("com.discordapp.Discord")
		);
	}

	#[test]
	fn desktop_entry_maps_exec_and_wm_class() {
		let mut map = DesktopIcons::default();
		let contents = "[Desktop Entry]\nName=Foo\nExec=/opt/foo/foo-bin %U\nIcon=foo-icon\nStartupWMClass=FooApp\n[Desktop Action new]\nIcon=other\n";
		index_desktop_entry(Path::new("/x/org.foo.Foo.desktop"), contents, &mut map);
		assert_eq!(
			map.exact.get("org.foo.foo").map(String::as_str),
			Some("foo-icon")
		);
		assert_eq!(
			map.exact.get("fooapp").map(String::as_str),
			Some("foo-icon")
		);
		assert_eq!(
			map.by_exec.get("foo-bin").map(String::as_str),
			Some("foo-icon")
		);
	}
}

//! Freedesktop icon lookup: `.desktop` files and the user's icon theme.
//!
//! Icons are looked up the way the desktop does it (icon theme specification): in the
//! current theme, then in the themes it inherits from, then in hicolor. Inside the
//! OpenDeck Flatpak the host's directories are visible under `/run/host`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use super::ICON_SIZE;

/// Fallback theme of every theme (icon theme specification).
const HICOLOR: &str = "hicolor";

/// `XDG_DATA_DIRS`-style base directories, most specific first.
static DATA_DIRS: LazyLock<Vec<PathBuf>> = LazyLock::new(data_dirs);

static DESKTOP_ICONS: LazyLock<DesktopIcons> = LazyLock::new(index_desktop_files);

/// The current theme and the themes it inherits from, hicolor last.
static THEMES: LazyLock<Vec<Theme>> = LazyLock::new(load_themes);

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
	// Every name in the desktop's themes first, then in the other installed themes.
	search(names, lookup).or_else(|| search(names, lookup_other_themes))
}

fn search(names: &[String], lookup: fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
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

/// Resolve an icon name (or absolute path) to a PNG/SVG file, as the desktop would.
fn lookup(name: &str) -> Option<PathBuf> {
	if name.starts_with('/') {
		let path = Path::new(name);
		return path.is_file().then(|| path.to_path_buf());
	}
	if !is_icon_name(name) {
		return None;
	}
	THEMES
		.iter()
		.find_map(|theme| theme.find(name))
		.or_else(|| {
			DATA_DIRS.iter().find_map(|base| {
				["svg", "png"]
					.iter()
					.map(|ext| base.join("pixmaps").join(format!("{name}.{ext}")))
					.find(|p| p.is_file())
			})
		})
}

/// Another theme's picture of the app still beats the generic icon: inside the Flatpak,
/// the icons that Flatpak apps install are out of reach.
fn lookup_other_themes(name: &str) -> Option<PathBuf> {
	if !is_icon_name(name) {
		return None;
	}
	OTHER_THEMES.iter().find_map(|theme| theme.find(name))
}

fn is_icon_name(name: &str) -> bool {
	!name.is_empty() && !name.contains('/')
}

/// Installed themes outside [`THEMES`], by name.
static OTHER_THEMES: LazyLock<Vec<Theme>> = LazyLock::new(|| {
	let mut names: Vec<String> = DATA_DIRS
		.iter()
		.map(|d| d.join("icons"))
		.filter_map(|dir| std::fs::read_dir(dir).ok())
		.flat_map(|entries| entries.flatten())
		.filter_map(|entry| entry.file_name().into_string().ok())
		.filter(|name| !THEMES.iter().any(|t| &t.name == name))
		.collect();
	names.sort_unstable();
	names.dedup();
	names
		.iter()
		.filter_map(|name| load_theme(name).map(|(theme, _)| theme))
		.filter(|theme| !theme.dirs.is_empty())
		.collect()
});

/// One icon theme: its directories in every base directory where it is installed.
struct Theme {
	name: String,
	/// `<base>/icons/<name>` directories that exist, most specific first.
	roots: Vec<PathBuf>,
	/// Subdirectories from `index.theme`, best match for [`ICON_SIZE`] first.
	dirs: Vec<String>,
}

impl Theme {
	fn find(&self, name: &str) -> Option<PathBuf> {
		self.dirs.iter().find_map(|dir| {
			self.roots.iter().find_map(|root| {
				["svg", "svgz", "png"]
					.iter()
					.map(|ext| root.join(dir).join(format!("{name}.{ext}")))
					.find(|p| p.is_file())
			})
		})
	}
}

/// A subdirectory section of `index.theme`.
#[derive(Debug, PartialEq)]
struct ThemeDir {
	/// Pixel sizes the directory's icons are meant for (scale included).
	min: u32,
	max: u32,
}

impl ThemeDir {
	fn parse(section: &HashMap<String, String>) -> Option<Self> {
		let number = |key: &str| section.get(key).and_then(|v| v.trim().parse::<u32>().ok());
		let size = number("Size")?;
		let scale = number("Scale").unwrap_or(1).max(1);
		let (min, max) = match section.get("Type").map(|t| t.trim()) {
			Some("Scalable") => (
				number("MinSize").unwrap_or(size),
				number("MaxSize").unwrap_or(size),
			),
			Some("Fixed") => (size, size),
			// "Threshold", the default type.
			_ => {
				let threshold = number("Threshold").unwrap_or(2);
				(size.saturating_sub(threshold), size + threshold)
			}
		};
		Some(Self {
			min: min * scale,
			max: max * scale,
		})
	}

	/// Lower is better. Icons smaller than the target look blurry once scaled up, so
	/// they rank after any larger one.
	fn distance(&self, target: u32) -> u32 {
		if (self.min..=self.max).contains(&target) {
			0
		} else if self.max < target {
			10_000 + (target - self.max)
		} else {
			self.min - target
		}
	}
}

fn theme_roots(name: &str) -> Vec<PathBuf> {
	let mut bases: Vec<PathBuf> = Vec::new();
	if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
		bases.push(home.join(".icons"));
	}
	bases.extend(DATA_DIRS.iter().map(|d| d.join("icons")));
	bases
		.into_iter()
		.map(|base| base.join(name))
		.filter(|root| root.is_dir())
		.collect()
}

fn load_theme(name: &str) -> Option<(Theme, Vec<String>)> {
	let roots = theme_roots(name);
	let index = roots
		.iter()
		.find_map(|root| std::fs::read_to_string(root.join("index.theme")).ok())?;
	let (dirs, inherits) = parse_index_theme(&index);
	Some((
		Theme {
			name: name.to_owned(),
			roots,
			dirs,
		},
		inherits,
	))
}

/// Subdirectories sorted for [`ICON_SIZE`], and the inherited themes.
fn parse_index_theme(contents: &str) -> (Vec<String>, Vec<String>) {
	let ini = parse_ini(contents);
	let main = ini.get("Icon Theme");
	let list = |key: &str| -> Vec<String> {
		main.and_then(|m| m.get(key))
			.map(|v| {
				v.split(',')
					.map(str::trim)
					.filter(|s| !s.is_empty())
					.map(str::to_owned)
					.collect()
			})
			.unwrap_or_default()
	};
	let mut dirs: Vec<(u32, String)> = list("Directories")
		.into_iter()
		.chain(list("ScaledDirectories"))
		.filter_map(|dir| {
			let section = ini.get(&dir)?;
			// Monochrome icons are for panels, not for an app's logo.
			if dir.contains("symbolic") {
				return None;
			}
			let parsed = ThemeDir::parse(section)?;
			Some((parsed.distance(ICON_SIZE), dir))
		})
		.collect();
	// Stable sort: equal distances keep the theme's own order.
	dirs.sort_by_key(|(distance, _)| *distance);
	let mut seen = std::collections::HashSet::new();
	let dirs = dirs
		.into_iter()
		.map(|(_, dir)| dir)
		.filter(|dir| seen.insert(dir.clone()))
		.collect();
	(dirs, list("Inherits"))
}

/// The current theme, then what it inherits (depth first, as the specification says),
/// then hicolor.
fn load_themes() -> Vec<Theme> {
	let current = current_theme_name();
	let mut themes: Vec<Theme> = Vec::new();
	let mut pending = vec![current.clone()];
	while let Some(name) = pending.pop() {
		if themes.iter().any(|t| t.name == name) || themes.len() > 16 {
			continue;
		}
		match load_theme(&name) {
			Some((theme, inherits)) => {
				themes.push(theme);
				pending.extend(inherits.into_iter().rev());
			}
			None => log::debug!("Icon theme {name} not found"),
		}
	}
	// hicolor always comes last, even when a theme inherits it in the middle.
	if let Some(position) = themes.iter().position(|t| t.name == HICOLOR) {
		let hicolor = themes.remove(position);
		themes.push(hicolor);
	} else if let Some((hicolor, _)) = load_theme(HICOLOR) {
		themes.push(hicolor);
	}
	log::info!(
		"Icon themes: {}",
		themes
			.iter()
			.map(|t| t.name.as_str())
			.collect::<Vec<_>>()
			.join(", ")
	);
	themes
}

/// Name of the icon theme the desktop uses.
fn current_theme_name() -> String {
	let config = std::env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
	let desktop = std::env::var("XDG_CURRENT_DESKTOP")
		.unwrap_or_default()
		.to_uppercase();
	let is_kde = desktop.split(':').any(|d| d == "KDE");
	let read = |file: &str, section: &str, key: &str| -> Option<String> {
		let contents = std::fs::read_to_string(config.as_ref()?.join(file)).ok()?;
		parse_ini(&contents)
			.get(section)?
			.get(key)
			.map(|v| v.trim().to_owned())
			.filter(|v| !v.is_empty())
	};
	let from_kde = || {
		is_kde
			.then(|| read("kdeglobals", "Icons", "Theme"))
			.flatten()
	};
	// KDE and other desktops write their theme here for GTK apps; the OpenDeck Flatpak
	// can read these two files.
	let from_gtk = || {
		read("gtk-4.0/settings.ini", "Settings", "gtk-icon-theme-name")
			.or_else(|| read("gtk-3.0/settings.ini", "Settings", "gtk-icon-theme-name"))
	};
	let from_gsettings = || {
		let output = std::process::Command::new("gsettings")
			.args(["get", "org.gnome.desktop.interface", "icon-theme"])
			.output()
			.ok()?;
		let name = String::from_utf8(output.stdout).ok()?;
		let name = name.trim().trim_matches('\'');
		(output.status.success() && !name.is_empty()).then(|| name.to_owned())
	};
	from_kde()
		.or_else(from_gtk)
		.or_else(from_gsettings)
		.unwrap_or_else(|| if is_kde { "breeze" } else { "Adwaita" }.to_owned())
}

/// Sections of an INI-style file (`index.theme`, `kdeglobals`, `settings.ini`).
/// Keys lose KDE's `[$e]`-style suffixes; localized keys (`Name[fr]`) are dropped.
fn parse_ini(contents: &str) -> HashMap<String, HashMap<String, String>> {
	let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
	let mut current = None;
	for line in contents.lines() {
		let line = line.trim();
		if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
			continue;
		}
		if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
			current = Some(name.to_owned());
			continue;
		}
		let (Some(section), Some((key, value))) = (&current, line.split_once('=')) else {
			continue;
		};
		let key = key.trim();
		let key = match key.split_once('[') {
			Some((base, rest)) if rest.starts_with('$') => base,
			Some(_) => continue,
			None => key,
		};
		sections
			.entry(section.clone())
			.or_default()
			.entry(key.to_owned())
			.or_insert_with(|| value.trim().to_owned());
	}
	sections
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

	const BREEZE_INDEX: &str = "[Icon Theme]\nName=Breeze Dark\nName[fr]=Brise sombre\nInherits=breeze,hicolor\nDirectories=apps/16,apps/48,preferences/32,status/symbolic\n\n[apps/16]\nSize=16\nType=Fixed\n\n[apps/48]\nSize=48\nType=Scalable\nMinSize=48\nMaxSize=256\n\n[preferences/32]\nSize=32\nType=Scalable\nMinSize=32\nMaxSize=256\n\n[status/symbolic]\nSize=16\nType=Scalable\nMinSize=8\nMaxSize=512\n";

	#[test]
	fn index_theme_orders_directories_for_the_lcd_icon() {
		let (dirs, inherits) = parse_index_theme(BREEZE_INDEX);
		assert_eq!(dirs, ["apps/48", "preferences/32", "apps/16"]);
		assert_eq!(inherits, ["breeze", "hicolor"]);
	}

	#[test]
	fn larger_icons_beat_smaller_ones() {
		let fixed = |size: u32| ThemeDir {
			min: size,
			max: size,
		};
		assert!(fixed(64).distance(ICON_SIZE) < fixed(48).distance(ICON_SIZE));
		assert!(fixed(256).distance(ICON_SIZE) < fixed(48).distance(ICON_SIZE));
		assert!(fixed(48).distance(ICON_SIZE) < fixed(16).distance(ICON_SIZE));
	}

	#[test]
	fn kde_keys_lose_their_suffix() {
		let ini = parse_ini("[Icons]\nTheme[$e]=Papirus-Dark\n");
		assert_eq!(ini["Icons"]["Theme"], "Papirus-Dark");
	}

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

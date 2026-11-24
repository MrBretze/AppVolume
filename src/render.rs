//! What each dial's LCD segment shows, as `setFeedback` payloads for the layouts in
//! `assets/layouts/`. Key names here must match the `key` fields of those files.

use crate::i18n::t;

use serde_json::{Map, Value, json};

pub const LAYOUT_VOLUME: &str = "layouts/volume.json";
pub const LAYOUT_VOLUME_CAPPED: &str = "layouts/volume-capped.json";
pub const LAYOUT_OUTPUT: &str = "layouts/output.json";
pub const LAYOUT_CONFIRM: &str = "layouts/confirm.json";

const WHITE: &str = "#e8eaed";
const GREY: &str = "#9aa0a6";
const DIM: &str = "#80868b";
const GREEN: &str = "#81c995";

/// Rough number of characters that fit on one 122 px wide line at 15 px.
const LINE_CHARS: usize = 16;

pub struct Screen {
	pub layout: &'static str,
	pub feedback: Map<String, Value>,
}

/// What the volume layout shows about the selected app.
pub enum VolumeLine<'a> {
	/// Volume, `1.0` = 100 %.
	Level { volume: f32, muted: bool },
	/// Free text instead of a level ("Not playing", an error…).
	Message(&'a str),
}

/// `max_percent`: highest volume the dial allows; above 100 the red boost bar is shown.
pub fn volume_screen(icon: &str, name: &str, line: VolumeLine<'_>, max_percent: u32) -> Screen {
	let boost = max_percent > 100;
	let (text, text_color, percent, fill) = match line {
		VolumeLine::Level { volume, muted } => {
			let percent = (volume * 100.0).round().max(0.0);
			// The 22 px line only fits ~11 characters: when muted, the grey bar shows the level.
			let text = if muted {
				t("muted").to_owned()
			} else {
				format!("{percent:.0}%")
			};
			let color = if muted {
				DIM
			} else if percent > 100.0 {
				"#ff6659"
			} else {
				WHITE
			};
			(text, color, percent, if muted { DIM } else { WHITE })
		}
		VolumeLine::Message(message) => (message.to_owned(), GREY, 0.0, DIM),
	};

	let mut feedback = Map::new();
	feedback.insert("app_icon".into(), json!(icon));
	feedback.insert("app_name".into(), json!({ "value": name }));
	feedback.insert(
		"volume_text".into(),
		json!({ "value": text, "color": text_color }),
	);
	feedback.insert(
		"volume_bar".into(),
		json!({ "value": percent.min(100.0), "bar_fill_c": fill }),
	);
	if boost {
		feedback.insert(
			"boost_bar".into(),
			json!({
				"value": percent.clamp(100.0, max_percent as f32),
				"range": { "min": 100, "max": max_percent },
			}),
		);
	}
	Screen {
		layout: if boost {
			LAYOUT_VOLUME
		} else {
			LAYOUT_VOLUME_CAPPED
		},
		feedback,
	}
}

pub struct OutputLine<'a> {
	pub name: &'a str,
	/// 0-based position in the output list.
	pub index: usize,
	pub count: usize,
	/// The app already plays to this output.
	pub is_current: bool,
}

pub fn output_screen(icon: &str, output: Option<OutputLine<'_>>) -> Screen {
	let (header, line1, line2, status, status_color) = match output {
		Some(o) => {
			let (line1, line2) = wrap_two_lines(o.name, LINE_CHARS);
			let (status, color) = if o.is_current {
				(t("current_output"), GREEN)
			} else {
				(t("press_to_apply"), GREY)
			};
			(
				format!("{} {}/{}", t("output"), o.index + 1, o.count),
				line1,
				line2,
				status,
				color,
			)
		}
		None => (
			t("output").to_owned(),
			t("no_output").to_owned(),
			String::new(),
			t("hold_to_go_back"),
			GREY,
		),
	};

	let mut feedback = Map::new();
	feedback.insert("app_icon".into(), json!(icon));
	feedback.insert("header".into(), json!({ "value": header }));
	feedback.insert("output_line1".into(), json!({ "value": line1 }));
	feedback.insert("output_line2".into(), json!({ "value": line2 }));
	feedback.insert(
		"status".into(),
		json!({ "value": status, "color": status_color }),
	);
	Screen {
		layout: LAYOUT_OUTPUT,
		feedback,
	}
}

/// "Output changed" confirmation: one frame of the check animation and the new output.
pub fn confirm_screen(frame: &str, output_name: &str) -> Screen {
	let (line1, line2) = wrap_two_lines(output_name, LINE_CHARS);
	let mut feedback = Map::new();
	feedback.insert("check".into(), json!(frame));
	feedback.insert("header".into(), json!({ "value": t("output_changed") }));
	feedback.insert("output_line1".into(), json!({ "value": line1 }));
	feedback.insert("output_line2".into(), json!({ "value": line2 }));
	Screen {
		layout: LAYOUT_CONFIRM,
		feedback,
	}
}

/// Split `text` into two lines at word boundaries; the renderer ellipsizes what still overflows.
fn wrap_two_lines(text: &str, max_chars: usize) -> (String, String) {
	let mut first = String::new();
	let mut words = text.split_whitespace().peekable();
	while let Some(word) = words.peek() {
		let needed = if first.is_empty() {
			word.chars().count()
		} else {
			first.chars().count() + 1 + word.chars().count()
		};
		if needed > max_chars && !first.is_empty() {
			break;
		}
		if !first.is_empty() {
			first.push(' ');
		}
		first.push_str(word);
		words.next();
	}
	let rest = words.collect::<Vec<_>>().join(" ");
	(first, rest)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn wraps_long_device_names() {
		let (a, b) = wrap_two_lines("Navi 21/23 HDMI/DP Audio Controller", 15);
		assert_eq!(a, "Navi 21/23");
		assert_eq!(b, "HDMI/DP Audio Controller");
		assert_eq!(
			wrap_two_lines("GoXLR Chat", 15),
			("GoXLR Chat".into(), String::new())
		);
		// A single word longer than a line stays on the first line.
		assert_eq!(
			wrap_two_lines("Supercalifragilistic x", 5).0,
			"Supercalifragilistic"
		);
	}

	#[test]
	fn boost_bar_only_fills_above_100() {
		let screen = volume_screen(
			"",
			"App",
			VolumeLine::Level {
				volume: 1.25,
				muted: false,
			},
			150,
		);
		assert_eq!(screen.feedback["volume_bar"]["value"], json!(100.0));
		assert_eq!(screen.feedback["boost_bar"]["value"], json!(125.0));
		assert_eq!(screen.feedback["boost_bar"]["range"]["max"], json!(150));
		let screen = volume_screen(
			"",
			"App",
			VolumeLine::Level {
				volume: 0.4,
				muted: false,
			},
			100,
		);
		assert_eq!(screen.layout, LAYOUT_VOLUME_CAPPED);
		assert!(!screen.feedback.contains_key("boost_bar"));
	}
}

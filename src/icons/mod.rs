//! Application icons, turned into PNG data URLs that the LCD layout can display.

use crate::audio::IconHint;

use base64::Engine;
use image::{Rgba, RgbaImage, imageops};
use resvg::{tiny_skia, usvg};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

/// Size of the `app_icon` pixmap in `assets/layouts/*.json`.
pub const ICON_SIZE: u32 = 56;

/// Shown when the app's icon cannot be found.
const FALLBACK_SVG: &[u8] = include_bytes!("../../assets/icons/app-fallback.svg");

/// Find and render the icon of an app. Blocking (file system access).
pub fn app_icon(hint: &IconHint) -> String {
	find_icon(hint).unwrap_or_else(fallback_icon)
}

/// Frames of the "output changed" animation: a green disc grows, then a check mark is drawn.
pub const CHECK_FRAME_COUNT: usize = 10;

pub fn check_frames() -> Vec<String> {
	(1..=CHECK_FRAME_COUNT)
		.map(|i| {
			let t = i as f32 / CHECK_FRAME_COUNT as f32;
			let svg = check_frame_svg(t);
			rasterize_svg(svg.as_bytes(), ICON_SIZE)
				.map(|img| to_data_url(&img))
				.unwrap_or_default()
		})
		.collect()
}

/// `t` from 0 to 1. The disc fills the first 40 %, the check mark the rest.
fn check_frame_svg(t: f32) -> String {
	// Ease-out so the disc pops in, then settles.
	let grow = 1.0 - (1.0 - (t / 0.4).min(1.0)).powi(3);
	let radius = 26.0 * grow;
	let draw = ((t - 0.4) / 0.6).clamp(0.0, 1.0);
	// Length of the check path below (≈ 12.7 + 26.9).
	const CHECK_LENGTH: f32 = 40.0;
	let offset = CHECK_LENGTH * (1.0 - draw);
	format!(
		r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64" width="64" height="64">
	<circle cx="32" cy="32" r="{radius:.2}" fill="#2e7d32"/>
	<path d="M19 33 l9 9 l19 -19" fill="none" stroke="#ffffff" stroke-width="6" stroke-linecap="round" stroke-linejoin="round" stroke-dasharray="{CHECK_LENGTH}" stroke-dashoffset="{offset:.2}" opacity="{opacity}"/>
</svg>"##,
		opacity = if draw > 0.0 { 1 } else { 0 },
	)
}

pub fn fallback_icon() -> String {
	rasterize_svg(FALLBACK_SVG, ICON_SIZE)
		.map(|img| to_data_url(&img))
		.unwrap_or_default()
}

/// Icon file chosen for an app, for diagnostics (`--list`).
#[cfg(target_os = "linux")]
pub fn icon_file(hint: &IconHint) -> Option<std::path::PathBuf> {
	linux::find_icon_file(&hint.names)
}

#[cfg(not(target_os = "linux"))]
pub fn icon_file(_hint: &IconHint) -> Option<std::path::PathBuf> {
	None
}

#[cfg(target_os = "linux")]
fn find_icon(hint: &IconHint) -> Option<String> {
	let path = linux::find_icon_file(&hint.names)?;
	log::debug!("Icon for {:?}: {}", hint.names, path.display());
	let bytes = std::fs::read(&path).ok()?;
	let is_svg = path
		.extension()
		.is_some_and(|e| e.eq_ignore_ascii_case("svg") || e.eq_ignore_ascii_case("svgz"));
	let img = if is_svg {
		rasterize_svg(&bytes, ICON_SIZE)
	} else {
		rasterize_bitmap(&bytes, ICON_SIZE)
	};
	if img.is_none() {
		log::debug!("Could not decode icon {}", path.display());
	}
	img.map(|img| to_data_url(&img))
}

#[cfg(target_os = "windows")]
fn find_icon(hint: &IconHint) -> Option<String> {
	let path = hint.exe_path.as_ref()?;
	let img = windows::extract_exe_icon(path)?;
	Some(to_data_url(&fit(img, ICON_SIZE)))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn find_icon(_hint: &IconHint) -> Option<String> {
	None
}

fn rasterize_svg(data: &[u8], size: u32) -> Option<RgbaImage> {
	// .svgz (used by some icon themes): usvg is built without its own gzip support.
	let unzipped;
	let data = if data.starts_with(&[0x1f, 0x8b]) {
		let mut out = Vec::new();
		std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(data), &mut out).ok()?;
		unzipped = out;
		&unzipped[..]
	} else {
		data
	};
	let tree = usvg::Tree::from_data(data, &usvg::Options::default()).ok()?;
	let svg_size = tree.size();
	let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
	let dx = (size as f32 - svg_size.width() * scale) / 2.0;
	let dy = (size as f32 - svg_size.height() * scale) / 2.0;
	let mut pixmap = tiny_skia::Pixmap::new(size, size)?;
	resvg::render(
		&tree,
		tiny_skia::Transform::from_scale(scale, scale).post_translate(dx, dy),
		&mut pixmap.as_mut(),
	);
	let pixels: Vec<u8> = pixmap
		.pixels()
		.iter()
		.flat_map(|p| {
			let c = p.demultiply();
			[c.red(), c.green(), c.blue(), c.alpha()]
		})
		.collect();
	RgbaImage::from_raw(size, size, pixels)
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn rasterize_bitmap(data: &[u8], size: u32) -> Option<RgbaImage> {
	let img = image::load_from_memory(data).ok()?.to_rgba8();
	Some(fit(img, size))
}

/// Scale `img` to fit a `size`×`size` square, centered on a transparent background.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn fit(img: RgbaImage, size: u32) -> RgbaImage {
	let (w, h) = img.dimensions();
	if w == size && h == size {
		return img;
	}
	let scale = (size as f32 / w as f32).min(size as f32 / h as f32);
	let (nw, nh) = (
		((w as f32 * scale).round() as u32).max(1),
		((h as f32 * scale).round() as u32).max(1),
	);
	let resized = imageops::resize(&img, nw, nh, imageops::FilterType::Lanczos3);
	let mut canvas = RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 0]));
	imageops::overlay(
		&mut canvas,
		&resized,
		((size - nw) / 2) as i64,
		((size - nh) / 2) as i64,
	);
	canvas
}

fn to_data_url(img: &RgbaImage) -> String {
	let mut png = Vec::new();
	if let Err(error) = img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
		log::warn!("Could not encode icon: {error}");
		return String::new();
	}
	format!(
		"data:image/png;base64,{}",
		base64::engine::general_purpose::STANDARD.encode(png)
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn gzipped_svg_is_rendered() {
		use std::io::Write;
		let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
		encoder.write_all(FALLBACK_SVG).unwrap();
		let svgz = encoder.finish().unwrap();
		let plain = rasterize_svg(FALLBACK_SVG, ICON_SIZE).unwrap();
		assert_eq!(rasterize_svg(&svgz, ICON_SIZE).unwrap(), plain);
	}

	#[test]
	fn check_frames_render() {
		let frames = check_frames();
		assert_eq!(frames.len(), CHECK_FRAME_COUNT);
		assert!(
			frames
				.iter()
				.all(|f| f.starts_with("data:image/png;base64,"))
		);
	}

	#[test]
	fn fallback_icon_renders() {
		assert!(fallback_icon().starts_with("data:image/png;base64,"));
	}

	#[test]
	fn fit_keeps_aspect_ratio() {
		let img = fit(RgbaImage::from_pixel(100, 50, Rgba([255, 0, 0, 255])), 56);
		assert_eq!(img.dimensions(), (56, 56));
		assert_eq!(img.get_pixel(28, 28)[0], 255);
		assert_eq!(img.get_pixel(28, 2)[3], 0);
	}
}

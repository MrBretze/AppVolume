//! Extract the icon embedded in an executable.

use std::path::Path;

use image::RgbaImage;
use windows::Win32::Graphics::Gdi::{
	BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
	GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::HSTRING;

/// Requested icon size (the shell picks the closest one in the file).
const EXTRACT_SIZE: u32 = 64;

pub fn extract_exe_icon(exe: &Path) -> Option<RgbaImage> {
	let path = HSTRING::from(exe.as_os_str());
	let mut icon = HICON::default();
	// SAFETY: `icon` receives a new icon handle that is destroyed below.
	unsafe {
		SHDefExtractIconW(&path, 0, 0, Some(&mut icon), None, EXTRACT_SIZE)
			.ok()
			.ok()?;
		if icon.is_invalid() {
			return None;
		}
		let image = icon_to_rgba(icon);
		let _ = DestroyIcon(icon);
		image
	}
}

/// # Safety
/// `icon` must be a valid icon handle.
unsafe fn icon_to_rgba(icon: HICON) -> Option<RgbaImage> {
	unsafe {
		let mut info = ICONINFO::default();
		GetIconInfo(icon, &mut info).ok()?;
		let color = bitmap_bgra(info.hbmColor);
		let mask = bitmap_bgra(info.hbmMask);
		let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
		let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));

		let (w, h, mut pixels) = color?;
		// BGRA → RGBA
		for px in pixels.as_chunks_mut::<4>().0 {
			px.swap(0, 2);
		}
		// Icons without an alpha channel use the AND mask for transparency.
		if pixels.as_chunks::<4>().0.iter().all(|px| px[3] == 0)
			&& let Some((mw, mh, mask)) = mask
			&& mw == w
			&& mh >= h
		{
			for (px, m) in pixels
				.as_chunks_mut::<4>()
				.0
				.iter_mut()
				.zip(mask.as_chunks::<4>().0)
			{
				px[3] = if m[0] == 0 { 255 } else { 0 };
			}
		}
		RgbaImage::from_raw(w, h, pixels)
	}
}

/// # Safety
/// `bitmap` must be a valid bitmap handle or null.
unsafe fn bitmap_bgra(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
	if bitmap.is_invalid() {
		return None;
	}
	unsafe {
		let mut bm = BITMAP::default();
		let got = GetObjectW(
			HGDIOBJ(bitmap.0),
			std::mem::size_of::<BITMAP>() as i32,
			Some((&mut bm as *mut BITMAP).cast()),
		);
		if got == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 {
			return None;
		}
		let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
		let mut bmi = BITMAPINFO {
			bmiHeader: BITMAPINFOHEADER {
				biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
				biWidth: w as i32,
				// Negative height: top-down rows.
				biHeight: -(h as i32),
				biPlanes: 1,
				biBitCount: 32,
				biCompression: BI_RGB.0,
				..Default::default()
			},
			..Default::default()
		};
		let mut pixels = vec![0u8; (w * h * 4) as usize];
		let dc = GetDC(None);
		let lines = GetDIBits(
			dc,
			bitmap,
			0,
			h,
			Some(pixels.as_mut_ptr().cast()),
			&mut bmi,
			DIB_RGB_COLORS,
		);
		ReleaseDC(None, dc);
		(lines > 0).then_some((w, h, pixels))
	}
}

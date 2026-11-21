use eframe::egui;
use image::imageops::FilterType;
use image::RgbaImage;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::OnceLock;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

use crate::icon_render;

const ICON_PNG_BYTES: &[u8] = include_bytes!("../assets/icon.png");
const _ICON_ICO_BYTES: &[u8] = include_bytes!("../assets/icon.ico");

static BASE_ICON: OnceLock<Option<RgbaImage>> = OnceLock::new();
static HEADER_OFF: OnceLock<Vec<u8>> = OnceLock::new();
static HEADER_ON: OnceLock<Vec<u8>> = OnceLock::new();
static VIEWPORT_OFF: OnceLock<Vec<u8>> = OnceLock::new();
static VIEWPORT_ON: OnceLock<Vec<u8>> = OnceLock::new();
static TRAY_OFF: OnceLock<Vec<u8>> = OnceLock::new();
static TRAY_ON: OnceLock<Vec<u8>> = OnceLock::new();

fn base_icon_image() -> Option<&'static RgbaImage> {
    BASE_ICON
        .get_or_init(|| {
            image::load_from_memory(ICON_PNG_BYTES)
                .ok()
                .map(|img| img.to_rgba8())
        })
        .as_ref()
}

fn render_sized_rgba(size: u32, active: bool) -> Vec<u8> {
    let mut rgba = if let Some(base) = base_icon_image() {
        let resized = if base.width() == size && base.height() == size {
            base.clone()
        } else {
            image::imageops::resize(base, size, size, FilterType::Lanczos3)
        };
        resized.into_raw()
    } else {
        icon_render::render_fallback_charcoal_rgba(size, false)
    };

    if active {
        icon_render::composite_active_dot(&mut rgba, size);
    }
    rgba
}

fn cached_rgba(slot: &'static OnceLock<Vec<u8>>, size: u32, active: bool) -> &'static [u8] {
    slot.get_or_init(|| render_sized_rgba(size, active))
        .as_slice()
}

pub fn app_viewport_icon(active: bool) -> egui::IconData {
    let size = 64u32;
    let rgba = if active {
        cached_rgba(&VIEWPORT_ON, size, true)
    } else {
        cached_rgba(&VIEWPORT_OFF, size, false)
    };
    egui::IconData {
        rgba: rgba.to_vec(),
        width: size,
        height: size,
    }
}

pub fn header_icon_rgba(active: bool) -> (u32, u32, &'static [u8]) {
    let size = 128u32;
    let rgba = if active {
        cached_rgba(&HEADER_ON, size, true)
    } else {
        cached_rgba(&HEADER_OFF, size, false)
    };
    (size, size, rgba)
}

pub fn tray_hicon(active: bool) -> windows::core::Result<HICON> {
    let size = 32u32;
    let rgba = if active {
        cached_rgba(&TRAY_ON, size, true)
    } else {
        cached_rgba(&TRAY_OFF, size, false)
    };
    rgba_to_hicon(rgba, size, size)
}

pub fn notification_hicon(active: bool) -> windows::core::Result<HICON> {
    let size = 64u32;
    let rgba = if active {
        cached_rgba(&VIEWPORT_ON, size, true)
    } else {
        cached_rgba(&VIEWPORT_OFF, size, false)
    };
    rgba_to_hicon(rgba, size, size)
}

pub fn thumb_power_hicon(is_on: bool) -> windows::core::Result<HICON> {
    let size = 24u32;
    let rgba = icon_render::render_thumb_power_rgba(size, is_on);
    rgba_to_hicon(&rgba, size, size)
}

pub fn thumb_quit_hicon() -> windows::core::Result<HICON> {
    let size = 24u32;
    let rgba = icon_render::render_thumb_quit_rgba(size);
    rgba_to_hicon(&rgba, size, size)
}

/// Converts a top-down RGBA8 pixel buffer into a Win32 HICON with per-pixel alpha.
pub fn rgba_to_hicon(rgba: &[u8], w: u32, h: u32) -> windows::core::Result<HICON> {
    assert_eq!(rgba.len(), (w * h * 4) as usize);

    unsafe {
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = w as i32;
        bmi.bmiHeader.biHeight = -(h as i32); // Negative height => top-down DIB
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let hdc = GetDC(None);
        let mut bits_ptr: *mut core::ffi::c_void = null_mut();
        let hbm_res = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0);
        if !hdc.is_invalid() {
            let _ = ReleaseDC(None, hdc);
        }
        let hbm_color = hbm_res?;

        if hbm_color.is_invalid() || bits_ptr.is_null() {
            if !hbm_color.is_invalid() {
                let _ = DeleteObject(hbm_color);
            }
            return Err(windows::core::Error::from_win32());
        }

        // Copy RGBA -> BGRA into DIB memory.
        let dst = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (w * h * 4) as usize);
        for (src_px, dst_px) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            dst_px[0] = src_px[2]; // B
            dst_px[1] = src_px[1]; // G
            dst_px[2] = src_px[0]; // R
            dst_px[3] = src_px[3]; // A
        }

        let stride = ((w + 15) / 16) * 2;
        let mask_bits = vec![0u8; (stride * h) as usize];
        let hbm_mask = CreateBitmap(w as i32, h as i32, 1, 1, Some(mask_bits.as_ptr() as _));
        if hbm_mask.is_invalid() {
            let _ = DeleteObject(hbm_color);
            return Err(windows::core::Error::from_win32());
        }

        let icon_info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        let hicon = CreateIconIndirect(&icon_info);
        let _ = DeleteObject(hbm_color);
        let _ = DeleteObject(hbm_mask);
        hicon
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_png_and_ico_assets_load_cleanly() {
        assert!(!ICON_PNG_BYTES.is_empty());
        assert!(!_ICON_ICO_BYTES.is_empty());

        let base = base_icon_image().expect("bundled icon.png should decode");
        assert_eq!(base.width(), 512);
        assert_eq!(base.height(), 512);

        let vp_off = app_viewport_icon(false);
        let vp_on = app_viewport_icon(true);
        assert_eq!(vp_off.width, 64);
        assert_eq!(vp_off.height, 64);
        assert_eq!(vp_off.rgba.len(), 64 * 64 * 4);
        assert_ne!(vp_off.rgba, vp_on.rgba);

        let (hw, hh, hbytes) = header_icon_rgba(false);
        assert_eq!((hw, hh), (128, 128));
        assert_eq!(hbytes.len(), 128 * 128 * 4);

        let hicon = tray_hicon(false).expect("tray_hicon should succeed");
        assert!(!hicon.is_invalid());
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyIcon(hicon);
        }
    }
}

use anyhow::{anyhow, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, ManuallyDrop};
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::mpsc::{self, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;
use windows::core::{w, IUnknown, Interface, PCWSTR};
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_POINT_2F, D2D_RECT_F,
    D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1Geometry, ID2D1RenderTarget, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_BITMAP_PROPERTIES,
    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_LAYER_OPTIONS_NONE, D2D1_LAYER_PARAMETERS,
    D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_FONT_WEIGHT_SEMI_BOLD,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TEXT_METRICS, DWRITE_TEXT_RANGE, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapCacheOnDemand, WICBitmapLockRead, WICRect,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, TranslateMessage, UpdateLayeredWindow,
    GWLP_USERDATA, GWL_EXSTYLE, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, HTTRANSPARENT, HWND_NOTOPMOST, HWND_TOPMOST, IDC_ARROW, MSG,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, ULW_ALPHA, WM_APP, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_EXITSIZEMOVE,
    WM_MOUSEMOVE, WM_MOVE, WM_NCHITTEST, WM_NCDESTROY, WM_SIZE, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const WM_NCMOUSEMOVE: u32 = 0x00A0;
const WM_NCMOUSELEAVE: u32 = 0x02A2;
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::config::{
    parse_hex_color, HideAnimation, LiveSubtitleStyle, OverlayConfig, SubtitleLineConfig,
    SubtitleLineSource, TextAlign, WindowState,
};
use crate::icon;
use crate::monitor::{
    apply_window_rect, enumerate_monitors, query_window_state, sanitize_window_rect,
    FallbackPlacement, ResolvedWindowRect,
};

const WM_APP_UPDATE_TEXT: u32 = WM_APP + 101;
const WM_APP_UPDATE_CFG: u32 = WM_APP + 102;
const WM_APP_SET_RECT: u32 = WM_APP + 103;
const WM_APP_QUIT: u32 = WM_APP + 105;

const TIMER_AUTOHIDE_ID: usize = 1;
const TIMER_ANIM_ID: usize = 2;
const RESIZE_BORDER_PX: i32 = 8;
const SCROLL_ANIM_DURATION_SEC: f32 = 0.36;

#[derive(Debug, Clone)]
pub enum OverlayEvent {
    GeometryChanged(WindowState),
    DisplayChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayLineItem {
    pub id: u64,
    pub original: String,
    pub translation: String,
    pub confirmed_tl_bytes: usize,
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OverlayPayload {
    pub items: Vec<OverlayLineItem>,
}

/// Visual crawl depth style indexed by item age (`0` = newest at bottom, `1` = middle, `2+` = top).
fn age_style(age: usize) -> (f32, f32) {
    match age {
        0 => (1.00, 1.00),
        1 => (0.94, 0.86),
        2 => (0.86, 0.60),
        _ => (0.80, 0.44),
    }
}

/// Returns clamped normalized time `t` and cubic ease-out factor `1 - (1 - t)^3`.
fn compute_scroll_easing(elapsed_sec: f32, duration_sec: f32) -> (f32, f32) {
    if duration_sec <= 0.0 {
        return (1.0, 1.0);
    }
    let t = (elapsed_sec / duration_sec).clamp(0.0, 1.0);
    let ease = 1.0 - (1.0 - t).powi(3);
    (t, ease)
}

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

#[derive(Debug, Clone)]
enum OverlayCmd {
    SetPayload(OverlayPayload),
    SetConfig(OverlayConfig),
    ApplyRect(ResolvedWindowRect),
    Shutdown,
}

pub struct OverlayHandle {
    hwnd_raw: isize,
    cmd_queue: Arc<std::sync::Mutex<Vec<OverlayCmd>>>,
    join: Option<JoinHandle<()>>,
}

impl OverlayHandle {
    pub fn spawn(
        initial_cfg: OverlayConfig,
        saved_window: Option<WindowState>,
        on_event: Arc<dyn Fn(OverlayEvent) + Send + Sync>,
    ) -> Result<Self> {
        let cmd_queue = Arc::new(std::sync::Mutex::new(Vec::<OverlayCmd>::new()));
        let queue_c = Arc::clone(&cmd_queue);
        let (hwnd_tx, hwnd_rx) = mpsc::channel::<Result<isize>>();

        let join = thread::Builder::new()
            .name("subtitle-overlay".to_string())
            .spawn(move || {
                overlay_thread_main(initial_cfg, saved_window, queue_c, hwnd_tx, on_event);
            })
            .map_err(|e| anyhow!("Failed to spawn overlay thread: {e}"))?;

        let hwnd_raw = hwnd_rx
            .recv()
            .map_err(|_| anyhow!("Overlay thread exited during startup"))??;

        Ok(Self {
            hwnd_raw,
            cmd_queue,
            join: Some(join),
        })
    }

    fn post(&self, msg_id: u32, cmd: OverlayCmd) {
        self.cmd_queue.lock().unwrap().push(cmd);
        if self.hwnd_raw != 0 {
            let hwnd = HWND(self.hwnd_raw as *mut _);
            unsafe {
                let _ = PostMessageW(hwnd, msg_id, WPARAM(0), LPARAM(0));
            }
        }
    }

    pub fn set_payload(&self, payload: OverlayPayload) {
        self.post(WM_APP_UPDATE_TEXT, OverlayCmd::SetPayload(payload));
    }

    pub fn set_config(&self, cfg: OverlayConfig) {
        self.post(WM_APP_UPDATE_CFG, OverlayCmd::SetConfig(cfg));
    }

    pub fn apply_rect(&self, rect: ResolvedWindowRect) {
        self.post(WM_APP_SET_RECT, OverlayCmd::ApplyRect(rect));
    }

    pub fn current_window_state(&self) -> Option<WindowState> {
        query_window_state(self.hwnd_raw)
    }

    pub fn shutdown(&mut self) {
        if self.hwnd_raw != 0 {
            self.post(WM_APP_QUIT, OverlayCmd::Shutdown);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for OverlayHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct CachedBgImage {
    path: String,
    width: u32,
    height: u32,
    pbgra: Vec<u8>,
}

#[derive(Clone)]
struct RenderedItemSnapshot {
    item: OverlayLineItem,
    y: f32,
    height: f32,
    scale: f32,
    alpha_mul: f32,
    age: usize,
}

#[derive(Clone, Copy)]
struct AnimTrack {
    start_y: f32,
    start_scale: f32,
    start_alpha_mul: f32,
}

#[derive(Clone)]
struct ExitingItem {
    item: OverlayLineItem,
    start_y: f32,
    end_y: f32,
    start_scale: f32,
    end_scale: f32,
    start_alpha_mul: f32,
    age_at_exit: usize,
}

struct ScrollAnimState {
    started_at: Instant,
    duration_sec: f32,
    tracks: HashMap<u64, AnimTrack>,
    exiting: Vec<ExitingItem>,
}

struct OverlayInner {
    hwnd: HWND,
    cfg: OverlayConfig,
    payload: OverlayPayload,
    scroll_anim: Option<ScrollAnimState>,
    last_snapshots: Vec<RenderedItemSnapshot>,
    anim_timer_active: bool,
    hovered: bool,
    tracking_leave: bool,
    last_activity: Instant,
    fade_alpha: u8,
    fading_started_at: Option<Instant>,
    hidden_by_autohide: bool,
    d2d_factory: ID2D1Factory,
    dw_factory: IDWriteFactory,
    wic_factory: IWICImagingFactory,
    cached_bg: Option<CachedBgImage>,
    cmd_queue: Arc<std::sync::Mutex<Vec<OverlayCmd>>>,
    on_event: Arc<dyn Fn(OverlayEvent) + Send + Sync>,
}

fn wide_null(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn d2d_color_from_hex(hex: &str, alpha: f32) -> D2D1_COLOR_F {
    let [r, g, b] = parse_hex_color(hex).unwrap_or([255, 255, 255]);
    D2D1_COLOR_F {
        r: (r as f32) / 255.0,
        g: (g as f32) / 255.0,
        b: (b as f32) / 255.0,
        a: alpha.clamp(0.0, 1.0),
    }
}

const IDENTITY_MATRIX: Matrix3x2 = Matrix3x2 {
    M11: 1.0,
    M12: 0.0,
    M21: 0.0,
    M22: 1.0,
    M31: 0.0,
    M32: 0.0,
};

struct PreparedSubLine {
    layout: windows::Win32::Graphics::DirectWrite::IDWriteTextLayout,
    local_y: f32,
    height: f32,
    color_hex: String,
    solid_alpha: f32,
    tentative_alpha: Option<f32>,
    confirmed_utf16: usize,
    total_utf16: usize,
    outline: bool,
    outline_r: f32,
    outline_alpha: f32,
}

struct PreparedBlock {
    sub_lines: Vec<PreparedSubLine>,
    raw_height: f32,
    align: TextAlign,
}

#[allow(clippy::too_many_arguments)]
unsafe fn build_sub_line(
    dw_factory: &IDWriteFactory,
    text: &str,
    font_family: &str,
    font_size: f32,
    weight: windows::Win32::Graphics::DirectWrite::DWRITE_FONT_WEIGHT,
    align: TextAlign,
    color_hex: &str,
    solid_alpha: f32,
    tentative_alpha: Option<f32>,
    confirmed_bytes: usize,
    outline: bool,
    outline_alpha: f32,
    max_w: f32,
    max_h: f32,
    dpi_scale: f32,
    local_y: f32,
) -> Option<PreparedSubLine> {
    if text.trim().is_empty() {
        return None;
    }
    let fam_w = wide_null(font_family);
    let loc_w = wide_null("en-us");
    let font_px = (font_size * dpi_scale).max(9.0);
    let format = dw_factory
        .CreateTextFormat(
            PCWSTR(fam_w.as_ptr()),
            None,
            weight,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            font_px,
            PCWSTR(loc_w.as_ptr()),
        )
        .ok()?;
    let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
    let dw_align = match align {
        TextAlign::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
        TextAlign::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
        TextAlign::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
    };
    let _ = format.SetTextAlignment(dw_align);

    let utf16: Vec<u16> = text.encode_utf16().collect();
    let layout = dw_factory
        .CreateTextLayout(&utf16, &format, max_w, max_h.max(40.0))
        .ok()?;
    let mut metrics = DWRITE_TEXT_METRICS::default();
    let _ = layout.GetMetrics(&mut metrics);
    let lh = metrics.height.max(font_px);

    let mut safe_bytes = confirmed_bytes.min(text.len());
    while safe_bytes > 0 && !text.is_char_boundary(safe_bytes) {
        safe_bytes -= 1;
    }
    let confirmed_utf16 = text[..safe_bytes].encode_utf16().count();
    let total_utf16 = utf16.len();

    let eff_tentative = tentative_alpha.filter(|_| confirmed_utf16 < total_utf16);

    Some(PreparedSubLine {
        layout,
        local_y,
        height: lh,
        color_hex: color_hex.to_string(),
        solid_alpha,
        tentative_alpha: eff_tentative,
        confirmed_utf16,
        total_utf16,
        outline,
        outline_r: (font_px * 0.075).clamp(1.2, 2.4),
        outline_alpha,
    })
}

#[allow(clippy::too_many_arguments)]
unsafe fn build_block(
    dw_factory: &IDWriteFactory,
    lines: &[SubtitleLineConfig],
    item: &OverlayLineItem,
    age: usize,
    max_w: f32,
    max_h: f32,
    dpi_scale: f32,
) -> Option<PreparedBlock> {
    let shows_tl = lines
        .iter()
        .any(|l| l.visible && l.source == SubtitleLineSource::Translation);
    let intra_gap = 2.0 * dpi_scale;
    let mut sub_lines = Vec::new();
    let mut raw_height = 0.0f32;
    let mut block_align: Option<TextAlign> = None;

    for line in lines {
        if !line.visible {
            continue;
        }
        match line.source {
            SubtitleLineSource::Original => {
                if (age != 0 && shows_tl) || item.original.trim().is_empty() {
                    continue;
                }
                let solid_alpha = if item.is_final { 1.0 } else { 0.78 };
                let outline_alpha = if item.is_final { 0.90 } else { 0.70 };
                let local_y = if sub_lines.is_empty() {
                    0.0
                } else {
                    raw_height + intra_gap
                };
                if let Some(sl) = build_sub_line(
                    dw_factory,
                    &item.original,
                    &line.font_family,
                    line.font_size,
                    DWRITE_FONT_WEIGHT_NORMAL,
                    line.align,
                    &line.color_hex,
                    solid_alpha,
                    None,
                    item.original.len(),
                    line.outline,
                    outline_alpha,
                    max_w,
                    max_h,
                    dpi_scale,
                    local_y,
                ) {
                    raw_height = local_y + sl.height;
                    block_align.get_or_insert(line.align);
                    sub_lines.push(sl);
                }
            }
            SubtitleLineSource::Translation => {
                if item.translation.trim().is_empty() {
                    continue;
                }
                let text = &item.translation;
                let conf_bytes = if item.is_final {
                    text.len()
                } else {
                    item.confirmed_tl_bytes.min(text.len())
                };
                let fully_confirmed = conf_bytes >= text.len();
                let tent_alpha = if item.is_final || fully_confirmed {
                    None
                } else {
                    Some(0.58)
                };
                let solid_alpha = if item.is_final { 1.0 } else { 0.96 };
                let outline_alpha = if item.is_final {
                    0.92
                } else if conf_bytes > 0 {
                    0.88
                } else {
                    0.65
                };
                let local_y = if sub_lines.is_empty() {
                    0.0
                } else {
                    raw_height + intra_gap
                };
                if let Some(sl) = build_sub_line(
                    dw_factory,
                    text,
                    &line.font_family,
                    line.font_size,
                    DWRITE_FONT_WEIGHT_SEMI_BOLD,
                    line.align,
                    &line.color_hex,
                    solid_alpha,
                    tent_alpha,
                    conf_bytes,
                    line.outline,
                    outline_alpha,
                    max_w,
                    max_h,
                    dpi_scale,
                    local_y,
                ) {
                    raw_height = local_y + sl.height;
                    block_align = Some(line.align);
                    sub_lines.push(sl);
                }
            }
        }
    }

    if sub_lines.is_empty() {
        None
    } else {
        Some(PreparedBlock {
            sub_lines,
            raw_height,
            align: block_align.unwrap_or(TextAlign::Center),
        })
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_block(
    rt: &ID2D1RenderTarget,
    block: &PreparedBlock,
    draw_y: f32,
    draw_scale: f32,
    alpha_mul: f32,
    wf: f32,
    pad_x: f32,
) {
    let alpha_mul = alpha_mul.clamp(0.0, 1.0);
    if alpha_mul <= 0.004 {
        return;
    }
    let anchor_x = match block.align {
        TextAlign::Left => pad_x,
        TextAlign::Center => wf * 0.5,
        TextAlign::Right => wf - pad_x,
    };
    let apply_xform = (draw_scale - 1.0).abs() > 0.0005;
    if apply_xform {
        let m = Matrix3x2 {
            M11: draw_scale,
            M12: 0.0,
            M21: 0.0,
            M22: draw_scale,
            M31: anchor_x * (1.0 - draw_scale),
            M32: draw_y * (1.0 - draw_scale),
        };
        rt.SetTransform(&m);
    }

    for sub in &block.sub_lines {
        let y = draw_y + sub.local_y;
        // Pass 1: Draw 8-direction black outline BEFORE calling SetDrawingEffect on ranges
        if sub.outline {
            let outline_col = D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: (sub.outline_alpha * alpha_mul).clamp(0.0, 1.0),
            };
            if let Ok(ob) = rt.CreateSolidColorBrush(&outline_col, None) {
                let r = sub.outline_r;
                let offsets = [
                    (-r, 0.0),
                    (r, 0.0),
                    (0.0, -r),
                    (0.0, r),
                    (-r * 0.72, -r * 0.72),
                    (r * 0.72, -r * 0.72),
                    (-r * 0.72, r * 0.72),
                    (r * 0.72, r * 0.72),
                ];
                for (dx, dy) in offsets {
                    rt.DrawTextLayout(
                        D2D_POINT_2F {
                            x: pad_x + dx,
                            y: y + dy,
                        },
                        &sub.layout,
                        &ob,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
        }
        // Pass 2: Apply dimmed tentative brush to trailing range [confirmed_utf16..total_utf16]
        if let Some(tent_alpha) = sub.tentative_alpha {
            if sub.confirmed_utf16 < sub.total_utf16 {
                let tent_col = d2d_color_from_hex(
                    &sub.color_hex,
                    (tent_alpha * alpha_mul).clamp(0.0, 1.0),
                );
                if let Ok(tent_brush) = rt.CreateSolidColorBrush(&tent_col, None) {
                    if let Ok(tent_unk) = tent_brush.cast::<IUnknown>() {
                        let _ = sub.layout.SetDrawingEffect(
                            &tent_unk,
                            DWRITE_TEXT_RANGE {
                                startPosition: sub.confirmed_utf16 as u32,
                                length: (sub.total_utf16 - sub.confirmed_utf16) as u32,
                            },
                        );
                    }
                }
            }
        }
        // Pass 3: Draw foreground text
        let fg_col = d2d_color_from_hex(
            &sub.color_hex,
            (sub.solid_alpha * alpha_mul).clamp(0.0, 1.0),
        );
        if let Ok(fg) = rt.CreateSolidColorBrush(&fg_col, None) {
            rt.DrawTextLayout(
                D2D_POINT_2F { x: pad_x, y },
                &sub.layout,
                &fg,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
    }

    if apply_xform {
        rt.SetTransform(&IDENTITY_MATRIX);
    }
}

impl OverlayInner {
    fn load_bg_image_cached(&mut self, path: &str) -> Option<&CachedBgImage> {
        let same = self
            .cached_bg
            .as_ref()
            .is_some_and(|c| c.path == path);
        if !same {
            let decoded = image::open(path).ok()?.to_rgba8();
            let w = decoded.width();
            let h = decoded.height();
            let mut pbgra = vec![0u8; (w * h * 4) as usize];
            for (src, dst) in decoded.as_raw().chunks_exact(4).zip(pbgra.chunks_exact_mut(4)) {
                let a = (src[3] as f32) / 255.0;
                dst[0] = ((src[2] as f32) * a).round() as u8;
                dst[1] = ((src[1] as f32) * a).round() as u8;
                dst[2] = ((src[0] as f32) * a).round() as u8;
                dst[3] = src[3];
            }
            self.cached_bg = Some(CachedBgImage {
                path: path.to_string(),
                width: w,
                height: h,
                pbgra,
            });
        }
        self.cached_bg.as_ref()
    }

    fn apply_window_styles(&self) {
        unsafe {
            let mut ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            ex |= (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE).0;
            if self.cfg.click_through {
                ex |= WS_EX_TRANSPARENT.0;
            } else {
                ex &= !WS_EX_TRANSPARENT.0;
            }
            if self.cfg.top_most {
                ex |= WS_EX_TOPMOST.0;
            } else {
                ex &= !WS_EX_TOPMOST.0;
            }
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex as isize);

            let z_order = if self.cfg.top_most {
                HWND_TOPMOST
            } else {
                HWND_NOTOPMOST
            };
            let _ = SetWindowPos(
                self.hwnd,
                z_order,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );

            let should_show = self.cfg.visible && !self.hidden_by_autohide;
            let _ = ShowWindow(
                self.hwnd,
                if should_show { SW_SHOWNOACTIVATE } else { SW_HIDE },
            );
        }
    }

    fn reset_autohide(&mut self) {
        self.last_activity = Instant::now();
        self.fade_alpha = 255;
        self.fading_started_at = None;
        let was_hidden = self.hidden_by_autohide;
        self.hidden_by_autohide = false;
        if was_hidden && self.cfg.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            }
        }
        self.sync_timer();
    }

    fn sync_timer(&self) {
        unsafe {
            if self.cfg.visible && self.cfg.auto_hide_sec > 0.05 && !self.hidden_by_autohide {
                let _ = SetTimer(self.hwnd, TIMER_AUTOHIDE_ID, 33, None);
            } else {
                let _ = KillTimer(self.hwnd, TIMER_AUTOHIDE_ID);
            }
        }
    }

    fn sync_anim_timer(&mut self) {
        let want = self.scroll_anim.is_some() && self.cfg.visible && !self.hidden_by_autohide;
        unsafe {
            if want && !self.anim_timer_active {
                let _ = SetTimer(self.hwnd, TIMER_ANIM_ID, 16, None);
                self.anim_timer_active = true;
            } else if !want && self.anim_timer_active {
                let _ = KillTimer(self.hwnd, TIMER_ANIM_ID);
                self.anim_timer_active = false;
            }
        }
    }

    fn apply_payload(&mut self, new_payload: OverlayPayload) {
        let same_ids = self.payload.items.len() == new_payload.items.len()
            && self
                .payload
                .items
                .iter()
                .zip(new_payload.items.iter())
                .all(|(a, b)| a.id == b.id);

        if !same_ids {
            if !self.cfg.smooth_scroll || new_payload.items.is_empty() {
                self.scroll_anim = None;
                if new_payload.items.is_empty() {
                    self.last_snapshots.clear();
                }
            } else {
                let mut tracks = HashMap::with_capacity(new_payload.items.len());
                let mut exiting = Vec::new();
                for snap in &self.last_snapshots {
                    if new_payload.items.iter().any(|it| it.id == snap.item.id) {
                        tracks.insert(
                            snap.item.id,
                            AnimTrack {
                                start_y: snap.y,
                                start_scale: snap.scale,
                                start_alpha_mul: snap.alpha_mul,
                            },
                        );
                    } else {
                        exiting.push(ExitingItem {
                            item: snap.item.clone(),
                            start_y: snap.y,
                            end_y: snap.y - (snap.height + 10.0),
                            start_scale: snap.scale,
                            end_scale: (snap.scale - 0.08).max(0.72),
                            start_alpha_mul: snap.alpha_mul,
                            age_at_exit: snap.age,
                        });
                    }
                }
                self.scroll_anim = Some(ScrollAnimState {
                    started_at: Instant::now(),
                    duration_sec: SCROLL_ANIM_DURATION_SEC,
                    tracks,
                    exiting,
                });
            }
        }

        self.payload = new_payload;
        self.reset_autohide();
        self.sync_anim_timer();
        self.render();
    }

    fn tick_autohide(&mut self) {
        if self.cfg.auto_hide_sec <= 0.05 || !self.cfg.visible {
            return;
        }
        if self.hovered && !self.cfg.click_through {
            self.last_activity = Instant::now();
            if self.fade_alpha < 255 {
                self.fade_alpha = 255;
                self.fading_started_at = None;
                self.render();
            }
            return;
        }

        let elapsed = self.last_activity.elapsed().as_secs_f32();
        if elapsed < self.cfg.auto_hide_sec {
            return;
        }

        match self.cfg.hide_animation {
            HideAnimation::None => {
                self.hidden_by_autohide = true;
                self.fade_alpha = 0;
                unsafe {
                    let _ = ShowWindow(self.hwnd, SW_HIDE);
                }
                self.sync_timer();
                self.sync_anim_timer();
            }
            HideAnimation::Fade => {
                let fade_start = *self.fading_started_at.get_or_insert_with(Instant::now);
                let dur = (self.cfg.hide_duration_ms.max(50) as f32) / 1000.0;
                let t = (fade_start.elapsed().as_secs_f32() / dur).clamp(0.0, 1.0);
                let next_alpha = ((1.0 - t) * 255.0).round() as u8;
                if next_alpha != self.fade_alpha {
                    self.fade_alpha = next_alpha;
                    self.render();
                }
                if t >= 1.0 {
                    self.hidden_by_autohide = true;
                    unsafe {
                        let _ = ShowWindow(self.hwnd, SW_HIDE);
                    }
                    self.sync_timer();
                    self.sync_anim_timer();
                }
            }
        }
    }

    fn render(&mut self) {
        if !self.cfg.visible || self.hidden_by_autohide {
            return;
        }
        let mut client = RECT::default();
        unsafe {
            if GetClientRect(self.hwnd, &mut client).is_err() {
                return;
            }
        }
        let w = (client.right - client.left).max(1) as u32;
        let h = (client.bottom - client.top).max(1) as u32;

        let dpi_scale = unsafe {
            let dpi = GetDpiForWindow(self.hwnd);
            if dpi == 0 {
                1.0
            } else {
                (dpi as f32) / 96.0
            }
        };

        let Ok(wic_bmp) = (unsafe {
            self.wic_factory.CreateBitmap(
                w,
                h,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapCacheOnDemand,
            )
        }) else {
            return;
        };

        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            ..Default::default()
        };

        let Ok(rt): Result<ID2D1RenderTarget, _> =
            (unsafe { self.d2d_factory.CreateWicBitmapRenderTarget(&wic_bmp, &rt_props) })
        else {
            return;
        };

        let bg_img_data = self
            .cfg
            .bg_image_path
            .clone()
            .and_then(|p| self.load_bg_image_cached(&p).map(|c| (c.width, c.height, c.pbgra.clone())));

        let no_speech_yet = self.payload.items.is_empty();
        let mut next_snapshots = Vec::new();
        let mut anim_finished = false;

        unsafe {
            rt.BeginDraw();
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let clear = D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
            rt.Clear(Some(&clear));

            let wf = w as f32;
            let hf = h as f32;
            let radius = (self.cfg.border_radius * dpi_scale).min(wf * 0.45).min(hf * 0.45);
            let rr = D2D1_ROUNDED_RECT {
                rect: D2D_RECT_F {
                    left: 0.5,
                    top: 0.5,
                    right: wf - 0.5,
                    bottom: hf - 0.5,
                },
                radiusX: radius,
                radiusY: radius,
            };

            let mut bg_alpha = (self.cfg.bg_opacity_pct / 100.0).clamp(0.0, 1.0);
            // Keep alpha >= 1/255 inside overlay rect when interactive so dragging works at 0% opacity
            if !self.cfg.click_through && bg_alpha < (1.0 / 255.0) {
                bg_alpha = 1.0 / 255.0;
            }

            if bg_alpha > 0.0 {
                let bg_col = d2d_color_from_hex(&self.cfg.bg_color_hex, bg_alpha);
                if let Ok(bg_brush) = rt.CreateSolidColorBrush(&bg_col, None) {
                    rt.FillRoundedRectangle(&rr, &bg_brush);
                }
            }

            // Optional background image clipped to rounded rectangle
            if let Some((iw, ih, pbgra)) = bg_img_data {
                let bmp_props = D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                };
                if let Ok(d2d_bmp) = rt.CreateBitmap(
                    D2D_SIZE_U {
                        width: iw,
                        height: ih,
                    },
                    Some(pbgra.as_ptr() as *const _),
                    iw * 4,
                    &bmp_props,
                ) {
                    if let (Ok(geom), Ok(layer)) = (
                        self.d2d_factory.CreateRoundedRectangleGeometry(&rr),
                        rt.CreateLayer(None),
                    ) {
                        let mask_geom: ID2D1Geometry = geom.into();
                        let params = D2D1_LAYER_PARAMETERS {
                            contentBounds: rr.rect,
                            geometricMask: ManuallyDrop::new(Some(mask_geom)),
                            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                            maskTransform: Matrix3x2 {
                                M11: 1.0,
                                M12: 0.0,
                                M21: 0.0,
                                M22: 1.0,
                                M31: 0.0,
                                M32: 0.0,
                            },
                            opacity: 1.0,
                            opacityBrush: ManuallyDrop::new(None),
                            layerOptions: D2D1_LAYER_OPTIONS_NONE,
                        };
                        rt.PushLayer(&params, &layer);
                        let img_alpha = (self.cfg.bg_opacity_pct / 100.0).clamp(0.1, 1.0);
                        rt.DrawBitmap(
                            &d2d_bmp,
                            Some(&rr.rect),
                            img_alpha,
                            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                            None,
                        );
                        rt.PopLayer();
                        let _ = ManuallyDrop::into_inner(params.geometricMask);
                    }
                }
            }

            // Render subtitle blocks with smooth upward scroll crawl
            let pad_x = 18.0 * dpi_scale;
            let pad_y = 12.0 * dpi_scale;
            let max_w = (wf - pad_x * 2.0).max(40.0);
            let max_block_h = (hf - pad_y * 2.0).max(40.0);

            // Subtle interactive border frame while not click-through
            if !self.cfg.click_through && (self.hovered || no_speech_yet) {
                let border_alpha = if self.hovered { 0.45 } else { 0.22 };
                let border_col = D2D1_COLOR_F {
                    r: 0.45,
                    g: 0.45,
                    b: 0.45,
                    a: border_alpha,
                };
                if let Ok(border_brush) = rt.CreateSolidColorBrush(&border_col, None) {
                    rt.DrawRoundedRectangle(&rr, &border_brush, 1.2, None);
                }
            }

            if no_speech_yet {
                if !self.cfg.click_through {
                    if let Some(first_vis) = self.cfg.lines.iter().find(|l| l.visible) {
                        if let Some(hint_sl) = build_sub_line(
                            &self.dw_factory,
                            "Translate Live — Drag to move · Edges to resize",
                            &first_vis.font_family,
                            first_vis.font_size,
                            DWRITE_FONT_WEIGHT_NORMAL,
                            first_vis.align,
                            &first_vis.color_hex,
                            0.65,
                            None,
                            usize::MAX,
                            first_vis.outline,
                            0.75,
                            max_w,
                            max_block_h,
                            dpi_scale,
                            0.0,
                        ) {
                            let h = hint_sl.height;
                            let hint_block = PreparedBlock {
                                sub_lines: vec![hint_sl],
                                raw_height: h,
                                align: first_vis.align,
                            };
                            let y = ((hf - h) * 0.5).max(pad_y);
                            draw_block(&rt, &hint_block, y, 1.0, 1.0, wf, pad_x);
                        }
                    }
                }
            } else {
                let count = self.payload.items.len();
                let item_gap = 8.0 * dpi_scale;
                let mut active_blocks: Vec<(&OverlayLineItem, usize, PreparedBlock, f32, f32)> =
                    Vec::with_capacity(count);
                let mut total_text_h = 0.0f32;

                for (idx, item) in self.payload.items.iter().enumerate() {
                    let age = count - 1 - idx;
                    if let Some(block) = build_block(
                        &self.dw_factory,
                        &self.cfg.lines,
                        item,
                        age,
                        max_w,
                        max_block_h,
                        dpi_scale,
                    ) {
                        let (target_scale, target_alpha) = age_style(age);
                        let scaled_h = block.raw_height * target_scale;
                        if !active_blocks.is_empty() {
                            total_text_h += item_gap;
                        }
                        total_text_h += scaled_h;
                        active_blocks.push((item, age, block, target_scale, target_alpha));
                    }
                }

                let avail_h = (hf - pad_y * 2.0).max(20.0);
                let stack_top = if total_text_h <= avail_h {
                    pad_y + (avail_h - total_text_h) * 0.5
                } else {
                    hf - pad_y - total_text_h
                };

                let mut target_ys = Vec::with_capacity(active_blocks.len());
                let mut cur_y = stack_top;
                for (_, _, block, target_scale, _) in &active_blocks {
                    target_ys.push(cur_y);
                    cur_y += block.raw_height * *target_scale + item_gap;
                }

                let anim_progress = self.scroll_anim.as_ref().map(|anim| {
                    compute_scroll_easing(anim.started_at.elapsed().as_secs_f32(), anim.duration_sec)
                });
                anim_finished = anim_progress.is_some_and(|(t, _)| t >= 1.0);

                let clip_rect = D2D_RECT_F {
                    left: (pad_x * 0.4).max(4.0),
                    top: (pad_y * 0.4).max(4.0),
                    right: (wf - pad_x * 0.4).max(8.0),
                    bottom: (hf - pad_y * 0.4).max(8.0),
                };
                rt.PushAxisAlignedClip(&clip_rect, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

                // Draw outgoing top items smoothly gliding out past the top clip edge
                if let (Some(anim), Some((anim_t, ease))) = (&self.scroll_anim, anim_progress) {
                    let exit_fade = 1.0 - anim_t.powf(0.85);
                    for ex in &anim.exiting {
                        if let Some(block) = build_block(
                            &self.dw_factory,
                            &self.cfg.lines,
                            &ex.item,
                            ex.age_at_exit,
                            max_w,
                            max_block_h,
                            dpi_scale,
                        ) {
                            let draw_y = lerp(ex.start_y, ex.end_y, ease);
                            let draw_scale = lerp(ex.start_scale, ex.end_scale, ease);
                            let draw_alpha = lerp(ex.start_alpha_mul, 0.0, 1.0 - exit_fade);
                            draw_block(&rt, &block, draw_y, draw_scale, draw_alpha, wf, pad_x);
                        }
                    }
                }

                // Draw active items (older receding gently at top, newest live at bottom)
                for (i, (item, age, block, target_scale, target_alpha)) in
                    active_blocks.into_iter().enumerate()
                {
                    let target_y = target_ys[i];
                    let scaled_h = block.raw_height * target_scale;
                    let (draw_y, draw_scale, draw_alpha) = if let (Some(anim), Some((_, ease))) =
                        (&self.scroll_anim, anim_progress)
                    {
                        if let Some(tr) = anim.tracks.get(&item.id) {
                            (
                                lerp(tr.start_y, target_y, ease),
                                lerp(tr.start_scale, target_scale, ease),
                                lerp(tr.start_alpha_mul, target_alpha, ease),
                            )
                        } else {
                            let enter_shift = (scaled_h + item_gap).min(46.0 * dpi_scale);
                            (
                                lerp(target_y + enter_shift, target_y, ease),
                                lerp((target_scale * 1.03).min(1.04), target_scale, ease),
                                lerp(0.0, target_alpha, ease),
                            )
                        }
                    } else {
                        (target_y, target_scale, target_alpha)
                    };

                    next_snapshots.push(RenderedItemSnapshot {
                        item: item.clone(),
                        y: draw_y,
                        height: block.raw_height * draw_scale,
                        scale: draw_scale,
                        alpha_mul: draw_alpha,
                        age,
                    });

                    draw_block(&rt, &block, draw_y, draw_scale, draw_alpha, wf, pad_x);
                }

                rt.PopAxisAlignedClip();
            }

            if rt.EndDraw(None, None).is_err() {
                return;
            }
        }

        // Copy WIC bitmap PBGRA bytes into 32-bit DIBSection for UpdateLayeredWindow
        unsafe {
            let lock_rect = WICRect {
                X: 0,
                Y: 0,
                Width: w as i32,
                Height: h as i32,
            };
            let Ok(lock) = wic_bmp.Lock(&lock_rect, WICBitmapLockRead.0 as u32) else {
                return;
            };
            let mut buf_sz = 0u32;
            let mut ptr: *mut u8 = null_mut();
            if lock.GetDataPointer(&mut buf_sz, &mut ptr).is_err() || ptr.is_null() {
                return;
            }
            let src_stride = lock.GetStride().unwrap_or(w * 4) as usize;

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w as i32;
            bmi.bmiHeader.biHeight = -(h as i32);
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            let mut bits_ptr: *mut core::ffi::c_void = null_mut();
            let Ok(hbm) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
            else {
                return;
            };
            if hbm.is_invalid() || bits_ptr.is_null() {
                if !hbm.is_invalid() {
                    let _ = DeleteObject(hbm);
                }
                return;
            }

            let dst_stride = (w * 4) as usize;
            let src_slice = std::slice::from_raw_parts(ptr, (h as usize) * src_stride);
            let dst_slice = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, (h as usize) * dst_stride);
            if src_stride == dst_stride {
                dst_slice.copy_from_slice(&src_slice[..dst_slice.len()]);
            } else {
                for row in 0..(h as usize) {
                    let s0 = row * src_stride;
                    let d0 = row * dst_stride;
                    dst_slice[d0..d0 + dst_stride]
                        .copy_from_slice(&src_slice[s0..s0 + dst_stride]);
                }
            }

            let mem_dc = CreateCompatibleDC(None);
            let old_obj = SelectObject(mem_dc, hbm);

            let mut win_rect = RECT::default();
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(self.hwnd, &mut win_rect);
            let pt_dst = POINT {
                x: win_rect.left,
                y: win_rect.top,
            };
            let sz = SIZE {
                cx: w as i32,
                cy: h as i32,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: self.fade_alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&pt_dst),
                Some(&sz),
                mem_dc,
                Some(&pt_src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );

            SelectObject(mem_dc, old_obj);
            let _ = DeleteDC(mem_dc);
            let _ = DeleteObject(hbm);
        }

        if !no_speech_yet {
            self.last_snapshots = next_snapshots;
            if anim_finished {
                self.scroll_anim = None;
                self.sync_anim_timer();
            }
        } else {
            self.last_snapshots.clear();
        }
    }
}

unsafe fn drain_commands(state_rc: &Rc<RefCell<OverlayInner>>) {
    let cmds: Vec<OverlayCmd> = {
        let state = state_rc.borrow();
        let mut q = state.cmd_queue.lock().unwrap();
        std::mem::take(&mut *q)
    };

    for cmd in cmds {
        match cmd {
            OverlayCmd::SetPayload(payload) => {
                state_rc.borrow_mut().apply_payload(payload);
            }
            OverlayCmd::SetConfig(cfg) => {
                let mut st = state_rc.borrow_mut();
                let vis_changed = st.cfg.visible != cfg.visible;
                st.cfg = cfg;
                if !st.cfg.smooth_scroll {
                    st.scroll_anim = None;
                }
                if vis_changed && st.cfg.visible {
                    st.hidden_by_autohide = false;
                    st.fade_alpha = 255;
                    st.last_activity = Instant::now();
                }
                st.apply_window_styles();
                st.sync_timer();
                st.sync_anim_timer();
                st.render();
            }
            OverlayCmd::ApplyRect(rect) => {
                let hwnd = state_rc.borrow().hwnd;
                apply_window_rect(hwnd.0 as isize, rect);
                let mut st = state_rc.borrow_mut();
                st.scroll_anim = None;
                st.sync_anim_timer();
                st.render();
            }
            OverlayCmd::Shutdown => {
                let mut st = state_rc.borrow_mut();
                st.scroll_anim = None;
                st.sync_anim_timer();
                let hwnd = st.hwnd;
                drop(st);
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const RefCell<OverlayInner>;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, w_param, l_param);
    }
    let state_rc = ManuallyDrop::new(Rc::from_raw(ptr));
    let _guard = Rc::clone(&state_rc);
    let res = handle_overlay_msg(hwnd, msg, w_param, l_param, &state_rc);
    if msg == WM_NCDESTROY {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Rc::from_raw(ptr));
        windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
    }
    res
}

unsafe fn handle_overlay_msg(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
    state_rc: &Rc<RefCell<OverlayInner>>,
) -> LRESULT {
    match msg {
        WM_APP_UPDATE_TEXT | WM_APP_UPDATE_CFG | WM_APP_SET_RECT | WM_APP_QUIT => {
            drain_commands(state_rc);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            let click_through = state_rc.borrow().cfg.click_through;
            if click_through {
                return LRESULT(HTTRANSPARENT as isize);
            }
            let x = (l_param.0 & 0xFFFF) as i16 as i32;
            let y = ((l_param.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            let b = RESIZE_BORDER_PX;

            let left = pt.x < b;
            let right = pt.x >= w - b;
            let top = pt.y < b;
            let bottom = pt.y >= h - b;

            let code = match (left, right, top, bottom) {
                (true, _, true, _) => HTTOPLEFT,
                (_, true, true, _) => HTTOPRIGHT,
                (true, _, _, true) => HTBOTTOMLEFT,
                (_, true, _, true) => HTBOTTOMRIGHT,
                (true, _, _, _) => HTLEFT,
                (_, true, _, _) => HTRIGHT,
                (_, _, true, _) => HTTOP,
                (_, _, _, true) => HTBOTTOM,
                _ => HTCAPTION,
            };
            LRESULT(code as isize)
        }
        WM_MOUSEMOVE | WM_NCMOUSEMOVE => {
            let mut st = state_rc.borrow_mut();
            if !st.tracking_leave {
                let flags = if msg == WM_NCMOUSEMOVE {
                    TME_LEAVE | TME_NONCLIENT
                } else {
                    TME_LEAVE
                };
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: flags,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                if TrackMouseEvent(&mut tme).is_ok() {
                    st.tracking_leave = true;
                }
            }
            if !st.hovered {
                st.hovered = true;
                st.render();
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE | WM_NCMOUSELEAVE => {
            let mut st = state_rc.borrow_mut();
            st.tracking_leave = false;
            if st.hovered {
                st.hovered = false;
                st.render();
            }
            LRESULT(0)
        }
        WM_SIZE => {
            {
                let mut st = state_rc.borrow_mut();
                st.scroll_anim = None;
                st.sync_anim_timer();
                st.render();
            }
            if let Some(ws) = query_window_state(hwnd.0 as isize) {
                let on_ev = Arc::clone(&state_rc.borrow().on_event);
                on_ev(OverlayEvent::GeometryChanged(ws));
            }
            LRESULT(0)
        }
        WM_MOVE | WM_EXITSIZEMOVE => {
            if let Some(ws) = query_window_state(hwnd.0 as isize) {
                let on_ev = Arc::clone(&state_rc.borrow().on_event);
                on_ev(OverlayEvent::GeometryChanged(ws));
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            let suggested = l_param.0 as *const RECT;
            if !suggested.is_null() {
                let r = *suggested;
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            let mut st = state_rc.borrow_mut();
            st.scroll_anim = None;
            st.sync_anim_timer();
            st.render();
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            let saved = query_window_state(hwnd.0 as isize);
            let monitors = enumerate_monitors();
            let resolved = sanitize_window_rect(
                saved,
                820,
                196,
                260,
                72,
                &monitors,
                FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
            );
            apply_window_rect(hwnd.0 as isize, resolved);
            {
                let mut st = state_rc.borrow_mut();
                st.scroll_anim = None;
                st.sync_anim_timer();
                st.render();
            }
            let on_ev = Arc::clone(&state_rc.borrow().on_event);
            on_ev(OverlayEvent::DisplayChanged);
            LRESULT(0)
        }
        WM_TIMER => {
            if w_param.0 == TIMER_AUTOHIDE_ID {
                state_rc.borrow_mut().tick_autohide();
            } else if w_param.0 == TIMER_ANIM_ID {
                state_rc.borrow_mut().render();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

fn overlay_thread_main(
    initial_cfg: OverlayConfig,
    saved_window: Option<WindowState>,
    cmd_queue: Arc<std::sync::Mutex<Vec<OverlayCmd>>>,
    hwnd_tx: Sender<Result<isize>>,
    on_event: Arc<dyn Fn(OverlayEvent) + Send + Sync>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        let d2d_factory: ID2D1Factory =
            match D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) {
                Ok(f) => f,
                Err(e) => {
                    let _ = hwnd_tx.send(Err(anyhow!("D2D1CreateFactory failed: {e}")));
                    CoUninitialize();
                    return;
                }
            };
        let dw_factory: IDWriteFactory =
            match DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) {
                Ok(f) => f,
                Err(e) => {
                    let _ = hwnd_tx.send(Err(anyhow!("DWriteCreateFactory failed: {e}")));
                    CoUninitialize();
                    return;
                }
            };
        let wic_factory: IWICImagingFactory =
            match CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) {
                Ok(f) => f,
                Err(e) => {
                    let _ = hwnd_tx.send(Err(anyhow!("WICImagingFactory failed: {e}")));
                    CoUninitialize();
                    return;
                }
            };

        let hinstance: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let class_name = w!("TranslateLiveOverlayWnd");
        let hicon = icon::tray_hicon(false).unwrap_or_default();

        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: hinstance,
            hIcon: hicon,
            hIconSm: hicon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let monitors = enumerate_monitors();
        let init_rect = sanitize_window_rect(
            saved_window,
            820,
            196,
            260,
            72,
            &monitors,
            FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
        );

        let mut ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        if initial_cfg.top_most {
            ex_style |= WS_EX_TOPMOST;
        }
        if initial_cfg.click_through {
            ex_style |= WS_EX_TRANSPARENT;
        }

        let Ok(hwnd) = CreateWindowExW(
            ex_style,
            class_name,
            w!("Translate Live Subtitle Overlay"),
            WS_POPUP,
            init_rect.x,
            init_rect.y,
            init_rect.width,
            init_rect.height,
            None,
            None,
            hinstance,
            None,
        ) else {
            if !hicon.is_invalid() {
                let _ = DestroyIcon(hicon);
            }
            let _ = hwnd_tx.send(Err(anyhow!("Failed to create subtitle overlay window")));
            CoUninitialize();
            return;
        };

        let inner = Rc::new(RefCell::new(OverlayInner {
            hwnd,
            cfg: initial_cfg.clone(),
            payload: OverlayPayload::default(),
            scroll_anim: None,
            last_snapshots: Vec::new(),
            anim_timer_active: false,
            hovered: false,
            tracking_leave: false,
            last_activity: Instant::now(),
            fade_alpha: 255,
            fading_started_at: None,
            hidden_by_autohide: false,
            d2d_factory,
            dw_factory,
            wic_factory,
            cached_bg: None,
            cmd_queue,
            on_event,
        }));

        let raw_rc = Rc::into_raw(Rc::clone(&inner));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw_rc as isize);

        {
            let mut st = inner.borrow_mut();
            st.apply_window_styles();
            st.sync_timer();
            st.render();
        }

        let _ = hwnd_tx.send(Ok(hwnd.0 as isize));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
        CoUninitialize();
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FinalizedLine<'a> {
    pub original: &'a str,
    pub translation: &'a str,
}

/// Coordinates subtitle display with smooth multi-line chronological history (`max_lines` 1..=4):
/// - **Multi-Line Smooth Scroll Crawl** (`max_lines >= 2` + `PrefixLockedFill`): assigns
///   monotonic item IDs (`1, 2, 3...`) so live interim lines confirm in-place to final
///   without shifting IDs or flashing, then smoothly scroll upward as newer speech enters
///   at the bottom. `min_display_sec` paces back-to-back finalized sentences so rapidly
///   completed lines remain readable before scrolling off.
/// - **Single-line / CleanSentencesOnly**: holds finalized lines for `min_display_sec`
///   and catches up cleanly during rapid bursts.
#[derive(Debug, Default)]
pub struct OverlayDisplayHold {
    displayed_finals: usize,
    final_shown_at: Option<Instant>,
}

impl OverlayDisplayHold {
    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &mut self,
        now: Instant,
        transcript: &[FinalizedLine<'_>],
        interim_original: &str,
        interim_translation: &str,
        interim_confirmed_bytes: usize,
        live_style: LiveSubtitleStyle,
        shows_tl: bool,
        max_lines: usize,
        min_display_sec: f32,
    ) -> (OverlayPayload, bool) {
        let max_lines = max_lines.clamp(1, 4);

        if transcript.len() < self.displayed_finals {
            self.displayed_finals = transcript.len();
            self.final_shown_at = if self.displayed_finals > 0 {
                Some(now)
            } else {
                None
            };
        }

        let pending = transcript.len().saturating_sub(self.displayed_finals);
        if pending > 0 {
            if self.displayed_finals == 0 || min_display_sec <= 0.01 {
                self.displayed_finals = transcript.len();
                self.final_shown_at = Some(now);
            } else {
                let elapsed = self
                    .final_shown_at
                    .map(|t| now.saturating_duration_since(t).as_secs_f32())
                    .unwrap_or(f32::MAX);
                let effective_dwell = if pending >= 2 {
                    (min_display_sec * 0.75).max(0.85).min(min_display_sec)
                } else {
                    min_display_sec
                };
                if elapsed >= effective_dwell {
                    if pending >= 3 {
                        self.displayed_finals = transcript.len() - 1;
                    } else {
                        self.displayed_finals += 1;
                    }
                    self.final_shown_at = Some(now);
                }
            }
        }

        let elapsed = self
            .final_shown_at
            .map(|t| now.saturating_duration_since(t).as_secs_f32())
            .unwrap_or(f32::MAX);
        let holding_final =
            min_display_sec > 0.01 && self.displayed_finals > 0 && elapsed < min_display_sec;
        let has_unshown_finals = self.displayed_finals < transcript.len();

        let has_active_interim = match live_style {
            LiveSubtitleStyle::CleanSentencesOnly => !shows_tl && !interim_original.is_empty(),
            LiveSubtitleStyle::PrefixLockedFill => {
                if shows_tl {
                    !interim_translation.is_empty()
                } else {
                    !interim_original.is_empty()
                }
            }
        };

        let make_fin = |idx: usize| -> OverlayLineItem {
            let row = transcript[idx];
            let translation = if shows_tl {
                row.translation.to_string()
            } else {
                String::new()
            };
            let confirmed_tl_bytes = translation.len();
            OverlayLineItem {
                id: (idx as u64) + 1,
                original: row.original.to_string(),
                translation,
                confirmed_tl_bytes,
                is_final: true,
            }
        };
        let make_interim = |id: u64| -> OverlayLineItem {
            OverlayLineItem {
                id,
                original: interim_original.to_string(),
                translation: if shows_tl {
                    interim_translation.to_string()
                } else {
                    String::new()
                },
                confirmed_tl_bytes: interim_confirmed_bytes,
                is_final: false,
            }
        };

        // Case 1: Multi-line Smooth Live Fill — bottom live interim confirms in-place (same ID)
        // and newest interims scroll older translations cleanly upward.
        if max_lines >= 2 && live_style == LiveSubtitleStyle::PrefixLockedFill {
            if has_unshown_finals {
                let end = (self.displayed_finals + 1).min(transcript.len());
                let start = end.saturating_sub(max_lines);
                let items = (start..end).map(make_fin).collect();
                return (OverlayPayload { items }, true);
            } else if has_active_interim {
                let fin_keep = self.displayed_finals.min(max_lines - 1);
                let start = self.displayed_finals.saturating_sub(fin_keep);
                let mut items: Vec<OverlayLineItem> =
                    (start..self.displayed_finals).map(make_fin).collect();
                items.push(make_interim((self.displayed_finals as u64) + 1));
                return (OverlayPayload { items }, false);
            } else {
                let start = self.displayed_finals.saturating_sub(max_lines);
                let items = (start..self.displayed_finals).map(make_fin).collect();
                return (OverlayPayload { items }, false);
            }
        }

        // Case 2: Multi-line CleanSentencesOnly — up to `max_lines` finalized sentences stack
        // chronologically (unless Original-only mode is showing live interim).
        if max_lines >= 2 {
            let items = if has_active_interim && !holding_final && !has_unshown_finals {
                let fin_keep = self.displayed_finals.min(max_lines - 1);
                let start = self.displayed_finals.saturating_sub(fin_keep);
                let mut v: Vec<OverlayLineItem> =
                    (start..self.displayed_finals).map(make_fin).collect();
                v.push(make_interim((self.displayed_finals as u64) + 1));
                v
            } else {
                let start = self.displayed_finals.saturating_sub(max_lines);
                (start..self.displayed_finals).map(make_fin).collect()
            };
            let pending_transition = has_unshown_finals || (holding_final && has_active_interim);
            return (OverlayPayload { items }, pending_transition);
        }

        // Case 3: Single-line mode (max_lines == 1)
        let items = if has_active_interim && !holding_final && !has_unshown_finals {
            vec![make_interim((self.displayed_finals as u64) + 1)]
        } else if self.displayed_finals > 0 {
            vec![make_fin(self.displayed_finals - 1)]
        } else if has_active_interim {
            vec![make_interim(1)]
        } else {
            Vec::new()
        };

        let pending_transition = has_unshown_finals || (holding_final && has_active_interim);
        (OverlayPayload { items }, pending_transition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fl<'a>(o: &'a str, t: &'a str) -> FinalizedLine<'a> {
        FinalizedLine {
            original: o,
            translation: t,
        }
    }

    #[test]
    fn three_line_crawl_keeps_stable_ids_and_scrolls_oldest_off_top() {
        let mut hold = OverlayDisplayHold::default();
        let t0 = Instant::now();

        // S1 streaming live at id=1
        let (p_s1_live, pend) = hold.resolve(
            t0,
            &[],
            "o1",
            "First live...",
            6,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend);
        assert_eq!(p_s1_live.items.len(), 1);
        assert_eq!(p_s1_live.items[0].id, 1);
        assert!(!p_s1_live.items[0].is_final);

        // S1 finishes -> SAME id=1 in-place!
        let tr1 = vec![fl("o1", "First finished!")];
        let (p_s1_fin, pend) = hold.resolve(
            t0 + Duration::from_millis(500),
            &tr1,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend);
        assert_eq!(p_s1_fin.items.len(), 1);
        assert_eq!(p_s1_fin.items[0].id, 1);
        assert!(p_s1_fin.items[0].is_final);

        // S2 starts immediately 50ms later -> [1 (final), 2 (live)]
        let (p_s2_live, pend) = hold.resolve(
            t0 + Duration::from_millis(550),
            &tr1,
            "o2",
            "Second starting...",
            7,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend);
        assert_eq!(
            p_s2_live.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(p_s2_live.items[0].is_final);
        assert!(!p_s2_live.items[1].is_final);

        // S2 finishes -> [1 (final), 2 (final)]
        let tr2 = vec![fl("o1", "First finished!"), fl("o2", "Second finished!")];
        let (p_s2_fin, pend) = hold.resolve(
            t0 + Duration::from_millis(2000),
            &tr2,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend);
        assert_eq!(
            p_s2_fin.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 2]
        );

        // S3 live -> [1, 2, 3 (live)]; then S3 finishes -> [1, 2, 3 (final)]
        let (p_s3_live, _) = hold.resolve(
            t0 + Duration::from_millis(2100),
            &tr2,
            "o3",
            "Third...",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert_eq!(
            p_s3_live.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        let tr3 = vec![
            fl("o1", "First finished!"),
            fl("o2", "Second finished!"),
            fl("o3", "Third finished!"),
        ];
        let (p_s3_fin, _) = hold.resolve(
            t0 + Duration::from_millis(3400),
            &tr3,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert_eq!(
            p_s3_fin.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(p_s3_fin.items.iter().all(|i| i.is_final));

        // S4 starts -> id=1 scrolls off top edge, window shows [2, 3, 4 (live)]!
        let (p_s4_live, pend) = hold.resolve(
            t0 + Duration::from_millis(3500),
            &tr3,
            "o4",
            "Fourth streaming...",
            6,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend);
        assert_eq!(
            p_s4_live.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert_eq!(p_s4_live.items[2].translation, "Fourth streaming...");
        assert!(!p_s4_live.items[2].is_final);
    }

    #[test]
    fn rapid_final_in_three_line_mode_turns_solid_in_place_without_vanishing() {
        let mut hold = OverlayDisplayHold::default();
        let t0 = Instant::now();
        let tr3 = vec![
            fl("o1", "One!"),
            fl("o2", "Two!"),
            fl("o3", "Three!"),
        ];
        let _ = hold.resolve(
            t0,
            &tr3,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );

        // S4 starts live -> [2, 3, 4(live)]
        let (p_live4, _) = hold.resolve(
            t0 + Duration::from_millis(100),
            &tr3,
            "o4",
            "Four starting...",
            5,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert_eq!(
            p_live4.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        // S4 finishes quickly at 400ms (before 1.2s min_display_sec):
        // S4 turns solid in-place ([2, 3, 4(final)]) and stays readable even if S5 starts at 600ms!
        let tr4 = vec![
            fl("o1", "One!"),
            fl("o2", "Two!"),
            fl("o3", "Three!"),
            fl("o4", "Four done!"),
        ];
        let (p_fin4, pend4) = hold.resolve(
            t0 + Duration::from_millis(400),
            &tr4,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(pend4);
        assert_eq!(
            p_fin4.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );
        assert!(p_fin4.items[2].is_final);
        assert_eq!(p_fin4.items[2].translation, "Four done!");

        let (p_held, pend_h) = hold.resolve(
            t0 + Duration::from_millis(600),
            &tr4,
            "o5",
            "Five starting...",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(pend_h);
        assert_eq!(
            p_held.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![2, 3, 4]
        );

        // After 1.2s, advances to [3, 4, 5(live)]
        let (p_adv, pend_a) = hold.resolve(
            t0 + Duration::from_millis(1250),
            &tr4,
            "o5",
            "Five starting...",
            5,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert!(!pend_a);
        assert_eq!(
            p_adv.items.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[test]
    fn single_line_mode_holds_final_before_allowing_next_interim() {
        let mut hold = OverlayDisplayHold::default();
        let t0 = Instant::now();
        let tr = vec![fl("src1", "First sentence!")];

        let (p0, pend0) = hold.resolve(
            t0,
            &tr,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            1,
            1.2,
        );
        assert_eq!(p0.items.len(), 1);
        assert!(p0.items[0].is_final);
        assert_eq!(p0.items[0].translation, "First sentence!");
        assert!(!pend0);

        // 200ms later, new interim starts: must HOLD Final sentence 1 in single-line mode
        let (p1, pend1) = hold.resolve(
            t0 + Duration::from_millis(200),
            &tr,
            "src2",
            "Second starting...",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            1,
            1.2,
        );
        assert_eq!(p1.items.len(), 1);
        assert!(p1.items[0].is_final);
        assert_eq!(p1.items[0].translation, "First sentence!");
        assert!(pend1);

        // At 1250ms, dwell expired: switches to interim 2
        let (p2, pend2) = hold.resolve(
            t0 + Duration::from_millis(1250),
            &tr,
            "src2",
            "Second starting...",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            1,
            1.2,
        );
        assert_eq!(p2.items.len(), 1);
        assert!(!p2.items[0].is_final);
        assert_eq!(p2.items[0].translation, "Second starting...");
        assert!(!pend2);
    }

    #[test]
    fn clean_sentences_three_line_paces_and_catches_up_bursts() {
        let mut hold = OverlayDisplayHold::default();
        let t0 = Instant::now();
        let tr1 = vec![fl("s1", "One!")];
        let _ = hold.resolve(
            t0,
            &tr1,
            "",
            "",
            0,
            LiveSubtitleStyle::CleanSentencesOnly,
            true,
            3,
            1.2,
        );

        // 100ms later, second final arrives: keep showing [One!] until dwell expires
        let tr2 = vec![fl("s1", "One!"), fl("s2", "Two!")];
        let (p_hold, pend) = hold.resolve(
            t0 + Duration::from_millis(100),
            &tr2,
            "",
            "",
            0,
            LiveSubtitleStyle::CleanSentencesOnly,
            true,
            3,
            1.2,
        );
        assert_eq!(p_hold.items.len(), 1);
        assert_eq!(p_hold.items[0].translation, "One!");
        assert!(pend);

        // At 1250ms, advances to [One!, Two!]
        let (p_two, pend2) = hold.resolve(
            t0 + Duration::from_millis(1250),
            &tr2,
            "",
            "",
            0,
            LiveSubtitleStyle::CleanSentencesOnly,
            true,
            3,
            1.2,
        );
        assert_eq!(
            p_two.items.iter().map(|i| i.translation.as_str()).collect::<Vec<_>>(),
            vec!["One!", "Two!"]
        );
        assert!(!pend2);

        // Burst of 5 finals catches up cleanly ([Two!, Three!, Four!], keeping Five! next)
        let tr5 = vec![
            fl("s1", "One!"),
            fl("s2", "Two!"),
            fl("s3", "Three!"),
            fl("s4", "Four!"),
            fl("s5", "Five!"),
        ];
        let (p_burst, pend_burst) = hold.resolve(
            t0 + Duration::from_millis(2300),
            &tr5,
            "",
            "",
            0,
            LiveSubtitleStyle::CleanSentencesOnly,
            true,
            3,
            1.2,
        );
        assert_eq!(
            p_burst
                .items
                .iter()
                .map(|i| i.translation.as_str())
                .collect::<Vec<_>>(),
            vec!["Two!", "Three!", "Four!"]
        );
        assert!(pend_burst);
    }

    #[test]
    fn zero_min_display_and_clear_reset_work_cleanly() {
        let mut hold = OverlayDisplayHold::default();
        let t0 = Instant::now();
        let mut tr = vec![fl("s1", "One!")];
        let _ = hold.resolve(
            t0,
            &tr,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            1,
            0.0,
        );
        let (p, pend) = hold.resolve(
            t0 + Duration::from_millis(50),
            &tr,
            "s2",
            "Two...",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            1,
            0.0,
        );
        assert_eq!(p.items.len(), 1);
        assert!(!p.items[0].is_final);
        assert_eq!(p.items[0].translation, "Two...");
        assert!(!pend);

        tr.clear();
        let (empty, pend_e) = hold.resolve(
            t0 + Duration::from_millis(100),
            &tr,
            "",
            "",
            0,
            LiveSubtitleStyle::PrefixLockedFill,
            true,
            3,
            1.2,
        );
        assert_eq!(empty, OverlayPayload::default());
        assert!(!pend_e);
    }

    #[test]
    fn scroll_easing_and_age_depth_behave_smoothly() {
        assert_eq!(compute_scroll_easing(0.0, 0.36), (0.0, 0.0));
        let (t_mid, e_mid) = compute_scroll_easing(0.18, 0.36);
        assert!((t_mid - 0.5).abs() < 1e-6);
        assert!(e_mid > 0.8 && e_mid < 1.0); // Ease-out is fast early, soft landing
        assert_eq!(compute_scroll_easing(1.0, 0.36), (1.0, 1.0));

        let (s0, a0) = age_style(0);
        let (s1, a1) = age_style(1);
        let (s2, a2) = age_style(2);
        assert!(s0 > s1 && s1 > s2);
        assert!(a0 > a1 && a1 > a2);
    }
}

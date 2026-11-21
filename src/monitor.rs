use crate::config::WindowState;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, IsIconic, IsZoomed, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, MONITORINFOF_PRIMARY, SWP_NOACTIVATE, SWP_NOZORDER, SW_MAXIMIZE,
    SW_RESTORE, WINDOWPLACEMENT, WS_EX_TOOLWINDOW,
};

pub const MIN_VISIBLE_W: i32 = 80;
pub const MIN_VISIBLE_H: i32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorWorkArea {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub is_primary: bool,
}

impl MonitorWorkArea {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(1)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPlacement {
    CenterPrimary,
    BottomCenterPrimary { bottom_margin: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedWindowRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
}

unsafe extern "system" fn enum_monitor_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _lprc: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<MonitorWorkArea>);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(hmon, &mut mi).as_bool() {
        let w = mi.rcWork;
        list.push(MonitorWorkArea {
            left: w.left,
            top: w.top,
            right: w.right,
            bottom: w.bottom,
            is_primary: (mi.dwFlags & MONITORINFOF_PRIMARY) != 0,
        });
    }
    BOOL(1)
}

pub fn enumerate_monitors() -> Vec<MonitorWorkArea> {
    let mut monitors = Vec::new();
    unsafe {
        let ptr = &mut monitors as *mut Vec<MonitorWorkArea>;
        let _ = EnumDisplayMonitors(HDC::default(), None, Some(enum_monitor_cb), LPARAM(ptr as isize));
    }
    monitors
}

fn overlap_dims(
    ax0: i64,
    ay0: i64,
    ax1: i64,
    ay1: i64,
    m: &MonitorWorkArea,
) -> (i64, i64) {
    let ow = (ax1.min(m.right as i64) - ax0.max(m.left as i64)).max(0);
    let oh = (ay1.min(m.bottom as i64) - ay0.max(m.top as i64)).max(0);
    (ow, oh)
}

fn dist_sq_to_monitor(cx: i64, cy: i64, m: &MonitorWorkArea) -> i128 {
    let left = m.left as i64;
    let right = m.right as i64;
    let top = m.top as i64;
    let bottom = m.bottom as i64;
    let dx = if cx < left {
        left - cx
    } else if cx > right {
        cx - right
    } else {
        0
    } as i128;
    let dy = if cy < top {
        top - cy
    } else if cy > bottom {
        cy - bottom
    } else {
        0
    } as i128;
    dx * dx + dy * dy
}

fn clamp_dims_to_monitor(
    w: i32,
    h: i32,
    min_w: i32,
    min_h: i32,
    m: &MonitorWorkArea,
) -> (i32, i32) {
    let mw = m.width();
    let mh = m.height();
    let eff_min_w = min_w.min(mw).max(1);
    let eff_min_h = min_h.min(mh).max(1);
    (w.clamp(eff_min_w, mw), h.clamp(eff_min_h, mh))
}

fn place_fallback_on_monitor(
    w: i32,
    h: i32,
    min_w: i32,
    min_h: i32,
    m: &MonitorWorkArea,
    fallback: FallbackPlacement,
    maximized: bool,
) -> ResolvedWindowRect {
    let (cw, ch) = clamp_dims_to_monitor(w, h, min_w, min_h, m);
    let (x, y) = match fallback {
        FallbackPlacement::CenterPrimary => (
            m.left + (m.width() - cw) / 2,
            m.top + (m.height() - ch) / 2,
        ),
        FallbackPlacement::BottomCenterPrimary { bottom_margin } => {
            let max_y = (m.bottom - ch).max(m.top);
            let target_y = (m.bottom - ch - bottom_margin).clamp(m.top, max_y);
            (m.left + (m.width() - cw) / 2, target_y)
        }
    };
    ResolvedWindowRect {
        x,
        y,
        width: cw,
        height: ch,
        maximized,
    }
}

/// Validates a saved window rectangle against active monitors' work areas.
/// - Preserves valid multi-monitor / negative virtual coordinates when at least 80x40 px is visible.
/// - Clamps dimensions so the window never exceeds its target monitor's work area.
/// - Recovers off-screen or disconnected-monitor windows safely onto the nearest monitor or primary fallback.
pub fn sanitize_window_rect(
    saved: Option<WindowState>,
    default_w: i32,
    default_h: i32,
    min_w: i32,
    min_h: i32,
    monitors: &[MonitorWorkArea],
    fallback: FallbackPlacement,
) -> ResolvedWindowRect {
    if monitors.is_empty() {
        let fallback_mon = MonitorWorkArea {
            left: 0,
            top: 0,
            right: default_w.max(1280),
            bottom: default_h.max(720),
            is_primary: true,
        };
        return place_fallback_on_monitor(
            default_w,
            default_h,
            min_w,
            min_h,
            &fallback_mon,
            fallback,
            false,
        );
    }

    let primary = monitors
        .iter()
        .find(|m| m.is_primary)
        .copied()
        .unwrap_or(monitors[0]);

    let Some(ws) = saved else {
        return place_fallback_on_monitor(
            default_w,
            default_h,
            min_w,
            min_h,
            &primary,
            fallback,
            false,
        );
    };

    if !ws.width.is_finite() || !ws.height.is_finite() {
        return place_fallback_on_monitor(
            default_w,
            default_h,
            min_w,
            min_h,
            &primary,
            fallback,
            ws.maximized,
        );
    }

    let raw_w = (ws.width.round() as f64).clamp(min_w as f64, i32::MAX as f64) as i32;
    let raw_h = (ws.height.round() as f64).clamp(min_h as f64, i32::MAX as f64) as i32;
    let ws_x = ws.x as i64;
    let ws_y = ws.y as i64;
    let ws_x1 = ws_x + (raw_w as i64);
    let ws_y1 = ws_y + (raw_h as i64);

    let mut best_mon: Option<(i128, MonitorWorkArea)> = None;
    for &m in monitors {
        let (ow, oh) = overlap_dims(ws_x, ws_y, ws_x1, ws_y1, &m);
        if ow >= MIN_VISIBLE_W as i64 && oh >= MIN_VISIBLE_H as i64 {
            let area = (ow as i128) * (oh as i128);
            if best_mon.as_ref().map_or(true, |(best_a, _)| area > *best_a) {
                best_mon = Some((area, m));
            }
        }
    }

    if let Some((_, m)) = best_mon {
        let (cw, ch) = clamp_dims_to_monitor(raw_w, raw_h, min_w, min_h, &m);
        let (ow, oh) = overlap_dims(ws_x, ws_y, ws_x + (cw as i64), ws_y + (ch as i64), &m);
        let (tx, ty) = if ow >= MIN_VISIBLE_W as i64 && oh >= MIN_VISIBLE_H as i64 && ws.y >= m.top {
            (ws.x, ws.y)
        } else {
            let min_x = m.left.saturating_sub(cw).saturating_add(MIN_VISIBLE_W.min(cw));
            let max_x = m.right.saturating_sub(MIN_VISIBLE_W.min(cw));
            let max_y = m.bottom.saturating_sub(MIN_VISIBLE_H.min(ch)).max(m.top);
            (
                ws.x.clamp(min_x, max_x.max(min_x)),
                ws.y.clamp(m.top, max_y),
            )
        };
        return ResolvedWindowRect {
            x: tx,
            y: ty,
            width: cw,
            height: ch,
            maximized: ws.maximized,
        };
    }

    // Saved rect is off-screen or its monitor was disconnected.
    match fallback {
        FallbackPlacement::BottomCenterPrimary { .. } => {
            place_fallback_on_monitor(raw_w, raw_h, min_w, min_h, &primary, fallback, false)
        }
        FallbackPlacement::CenterPrimary => {
            let cx = ws_x + (raw_w as i64) / 2;
            let cy = ws_y + (raw_h as i64) / 2;
            let nearest = monitors
                .iter()
                .min_by_key(|m| dist_sq_to_monitor(cx, cy, m))
                .copied()
                .unwrap_or(primary);
            let (cw, ch) = clamp_dims_to_monitor(raw_w, raw_h, min_w, min_h, &nearest);
            let dist_sq = dist_sq_to_monitor(cx, cy, &nearest);
            let threshold = (nearest.width().max(nearest.height()) as i128).pow(2);
            if dist_sq > threshold {
                place_fallback_on_monitor(
                    cw,
                    ch,
                    min_w,
                    min_h,
                    &primary,
                    FallbackPlacement::CenterPrimary,
                    ws.maximized,
                )
            } else {
                let max_x = (nearest.right - cw).max(nearest.left);
                let max_y = (nearest.bottom - ch).max(nearest.top);
                ResolvedWindowRect {
                    x: ws.x.clamp(nearest.left, max_x),
                    y: ws.y.clamp(nearest.top, max_y),
                    width: cw,
                    height: ch,
                    maximized: ws.maximized,
                }
            }
        }
    }
}

pub fn default_reset_rect(
    default_w: i32,
    default_h: i32,
    min_w: i32,
    min_h: i32,
    monitors: &[MonitorWorkArea],
    fallback: FallbackPlacement,
) -> ResolvedWindowRect {
    sanitize_window_rect(None, default_w, default_h, min_w, min_h, monitors, fallback)
}

/// Applies a resolved physical rectangle `(x, y, width, height)` to a Win32 HWND.
pub fn apply_window_rect(hwnd_raw: isize, rect: ResolvedWindowRect) {
    if hwnd_raw == 0 {
        return;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        if IsZoomed(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetWindowPos(
            hwnd,
            None,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        if rect.maximized {
            let _ = ShowWindow(hwnd, SW_MAXIMIZE);
        }
    }
}

/// Queries the window's persistent geometry in physical virtual-screen coordinates.
/// Uses `rcNormalPosition` when maximized (converted from workspace coords when applicable)
/// so restored size/monitor survives restarts.
pub fn query_window_state(hwnd_raw: isize) -> Option<WindowState> {
    if hwnd_raw == 0 {
        return None;
    }
    let hwnd = HWND(hwnd_raw as *mut _);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            return None;
        }
        let maximized = IsZoomed(hwnd).as_bool();
        if maximized {
            let mut wp = WINDOWPLACEMENT {
                length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                ..Default::default()
            };
            if GetWindowPlacement(hwnd, &mut wp).is_ok() {
                let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
                let (dx, dy) = if (ex_style & WS_EX_TOOLWINDOW.0) == 0 {
                    let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                    let mut mi = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    if !hmon.is_invalid() && GetMonitorInfoW(hmon, &mut mi).as_bool() {
                        (
                            mi.rcWork.left - mi.rcMonitor.left,
                            mi.rcWork.top - mi.rcMonitor.top,
                        )
                    } else {
                        (0, 0)
                    }
                } else {
                    (0, 0)
                };
                let r = wp.rcNormalPosition;
                let w = (r.right - r.left).max(100) as f32;
                let h = (r.bottom - r.top).max(60) as f32;
                return Some(WindowState {
                    x: r.left.saturating_add(dx),
                    y: r.top.saturating_add(dy),
                    width: w,
                    height: h,
                    maximized: true,
                });
            }
        }
        let mut r = RECT::default();
        GetWindowRect(hwnd, &mut r).ok()?;
        if r.left < -30000 || r.top < -30000 {
            return None;
        }
        let w = (r.right - r.left).max(100) as f32;
        let h = (r.bottom - r.top).max(60) as f32;
        Some(WindowState {
            x: r.left,
            y: r.top,
            width: w,
            height: h,
            maximized,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multi_monitors() -> Vec<MonitorWorkArea> {
        vec![
            MonitorWorkArea {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1040,
                is_primary: false,
            },
            MonitorWorkArea {
                left: 0,
                top: 0,
                right: 2560,
                bottom: 1400,
                is_primary: true,
            },
        ]
    }

    #[test]
    fn preserves_negative_left_monitor_coords() {
        let mons = multi_monitors();
        let saved = WindowState {
            x: -1400,
            y: 120,
            width: 560.0,
            height: 760.0,
            maximized: false,
        };
        let r = sanitize_window_rect(
            Some(saved),
            560,
            760,
            480,
            560,
            &mons,
            FallbackPlacement::CenterPrimary,
        );
        assert_eq!(r.x, -1400);
        assert_eq!(r.y, 120);
        assert_eq!(r.width, 560);
        assert_eq!(r.height, 760);
    }

    #[test]
    fn recovers_from_disconnected_left_monitor() {
        let primary_only = vec![MonitorWorkArea {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
            is_primary: true,
        }];
        let saved = WindowState {
            x: -1500,
            y: 150,
            width: 560.0,
            height: 760.0,
            maximized: false,
        };
        let main_r = sanitize_window_rect(
            Some(saved),
            560,
            760,
            480,
            560,
            &primary_only,
            FallbackPlacement::CenterPrimary,
        );
        assert!(main_r.x >= 0 && main_r.x + main_r.width <= 1920);
        assert!(main_r.y >= 0 && main_r.y + main_r.height <= 1040);

        let overlay_r = sanitize_window_rect(
            Some(WindowState {
                x: -1400,
                y: 800,
                width: 820.0,
                height: 150.0,
                maximized: false,
            }),
            820,
            150,
            260,
            72,
            &primary_only,
            FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
        );
        // Bottom-center of 1920x1040 primary work area:
        assert_eq!(overlay_r.x, (1920 - 820) / 2);
        assert_eq!(overlay_r.y, 1040 - 150 - 48);
    }

    #[test]
    fn enforces_80x40_minimum_visible_area_and_clamps_oversized_window() {
        let mons = vec![MonitorWorkArea {
            left: 0,
            top: 0,
            right: 1280,
            bottom: 720,
            is_primary: true,
        }];
        // Only 50x30 px intersects the monitor (< 80x40) -> triggers safeguard recovery
        let barely_visible = WindowState {
            x: 1230,
            y: 690,
            width: 560.0,
            height: 760.0,
            maximized: false,
        };
        let r = sanitize_window_rect(
            Some(barely_visible),
            560,
            760,
            480,
            560,
            &mons,
            FallbackPlacement::CenterPrimary,
        );
        // Width/height clamped to work area (height 760 -> 720) and moved cleanly onto monitor
        assert!(r.width <= 1280);
        assert!(r.height <= 720);
        assert!(r.x >= 0 && r.x + r.width <= 1280);
        assert!(r.y >= 0 && r.y + r.height <= 720);

        // Oversized window on valid position gets clamped to monitor work area
        let oversized = WindowState {
            x: 50,
            y: 40,
            width: 2400.0,
            height: 1600.0,
            maximized: false,
        };
        let r2 = sanitize_window_rect(
            Some(oversized),
            560,
            760,
            480,
            560,
            &mons,
            FallbackPlacement::CenterPrimary,
        );
        assert_eq!(r2.width, 1280);
        assert_eq!(r2.height, 720);
    }

    #[test]
    fn default_overlay_falls_back_to_bottom_center_primary() {
        let mons = multi_monitors();
        let r = sanitize_window_rect(
            None,
            820,
            150,
            260,
            72,
            &mons,
            FallbackPlacement::BottomCenterPrimary { bottom_margin: 48 },
        );
        assert_eq!(r.x, (2560 - 820) / 2);
        assert_eq!(r.y, 1400 - 150 - 48);
    }

    #[test]
    fn sanitize_window_rect_handles_extreme_coords_and_nan_without_overflow() {
        let mons = multi_monitors();
        for extreme in [
            WindowState {
                x: i32::MAX,
                y: i32::MAX,
                width: 1e20,
                height: 1e20,
                maximized: false,
            },
            WindowState {
                x: i32::MIN,
                y: i32::MIN,
                width: 560.0,
                height: 760.0,
                maximized: false,
            },
            WindowState {
                x: 100,
                y: 100,
                width: f32::NAN,
                height: f32::INFINITY,
                maximized: false,
            },
        ] {
            let r = sanitize_window_rect(
                Some(extreme),
                560,
                760,
                480,
                560,
                &mons,
                FallbackPlacement::CenterPrimary,
            );
            assert!(r.width >= 480 && r.width <= 2560);
            assert!(r.height >= 560 && r.height <= 1400);
            assert!(r.x >= 0 && r.x + r.width <= 2560);
            assert!(r.y >= 0 && r.y + r.height <= 1400);
        }
    }
}

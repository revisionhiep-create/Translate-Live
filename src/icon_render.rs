#![allow(dead_code)]

/// Pure-Rust procedural icon helpers for taskbar thumbnail buttons and fallback emblems.

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn mix_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = clamp01(t);
    ((a as f32) * (1.0 - t) + (b as f32) * t).round().clamp(0.0, 255.0) as u8
}

pub fn blend_over(dst: &mut [u8], r: u8, g: u8, b: u8, alpha: f32) {
    let a = clamp01(alpha);
    if a <= 0.001 {
        return;
    }
    let da = (dst[3] as f32) / 255.0;
    let out_a = a + da * (1.0 - a);
    if out_a <= 0.0001 {
        dst[0] = 0;
        dst[1] = 0;
        dst[2] = 0;
        dst[3] = 0;
        return;
    }
    let sr = r as f32;
    let sg = g as f32;
    let sb = b as f32;
    let dr = dst[0] as f32;
    let dg = dst[1] as f32;
    let db = dst[2] as f32;

    dst[0] = ((sr * a + dr * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[1] = ((sg * a + dg * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[2] = ((sb * a + db * da * (1.0 - a)) / out_a).round().clamp(0.0, 255.0) as u8;
    dst[3] = (out_a * 255.0).round().clamp(0.0, 255.0) as u8;
}

fn sd_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let denom = (bax * bax + bay * bay).max(1e-6);
    let h = clamp01((pax * bax + pay * bay) / denom);
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt()
}

pub fn cov(dist: f32, aa: f32) -> f32 {
    clamp01(0.5 - dist / aa)
}

/// Composites a soft emerald green (#34A853) active status dot with dark ring in the bottom-right corner.
pub fn composite_active_dot(rgba: &mut [u8], size: u32) {
    let aa = 1.25 / (size as f32);
    let cx = 0.80;
    let cy = 0.80;
    let dot_r = 0.145;
    let ring_r = 0.195;

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        if (py - cy).abs() > ring_r + aa * 2.0 {
            continue;
        }
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let dist = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
            if dist > ring_r + aa * 2.0 {
                continue;
            }
            let idx = ((py_i * size + px_i) * 4) as usize;
            let px_slice = &mut rgba[idx..idx + 4];
            let ring_a = cov(dist - ring_r, aa);
            blend_over(px_slice, 24, 24, 24, ring_a * 0.92);
            let dot_a = cov(dist - dot_r, aa);
            blend_over(px_slice, 52, 168, 83, dot_a); // #34A853
        }
    }
}

/// Safe fallback icon if bundled PNG decoding ever fails.
pub fn render_fallback_charcoal_rgba(size: u32, active: bool) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    for px in buf.chunks_exact_mut(4) {
        px[0] = 30;
        px[1] = 30;
        px[2] = 30;
        px[3] = 255;
    }
    if active {
        composite_active_dot(&mut buf, size);
    }
    buf
}

/// Render a 24x24 (or arbitrary size) power button icon for the Windows taskbar thumbnail toolbar.
pub fn render_thumb_power_rgba(size: u32, is_on: bool) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let aa = 1.25 / (size as f32);

    let (bg_r, bg_g, bg_b, rim_r, rim_g, rim_b, fg_r, fg_g, fg_b) = if is_on {
        (52u8, 168u8, 83u8, 84u8, 194u8, 112u8, 255u8, 255u8, 255u8)
    } else {
        (56u8, 53u8, 50u8, 96u8, 90u8, 84u8, 240u8, 238u8, 235u8)
    };

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let idx = ((py_i * size + px_i) * 4) as usize;
            let pixel = &mut buf[idx..idx + 4];

            let badge_d = ((px - 0.5).powi(2) + (py - 0.5).powi(2)).sqrt() - 0.44;
            let badge_a = cov(badge_d, aa);
            if badge_a > 0.0 {
                let rim = cov(badge_d.abs() - 0.02, aa);
                let r = mix_u8(bg_r, rim_r, rim * 0.7);
                let g = mix_u8(bg_g, rim_g, rim * 0.7);
                let b = mix_u8(bg_b, rim_b, rim * 0.7);
                blend_over(pixel, r, g, b, badge_a);
            }

            let ring_cx = 0.5;
            let ring_cy = 0.52;
            let dx = px - ring_cx;
            let dy = py - ring_cy;
            let angle_from_top = dx.abs().atan2(-dy);
            let ring_dist = if angle_from_top < 0.52 {
                let gx = 0.22 * 0.52_f32.sin();
                let gy = ring_cy - 0.22 * 0.52_f32.cos();
                let cap_l = ((px - (ring_cx - gx)).powi(2) + (py - gy).powi(2)).sqrt() - 0.048;
                let cap_r = ((px - (ring_cx + gx)).powi(2) + (py - gy).powi(2)).sqrt() - 0.048;
                cap_l.min(cap_r)
            } else {
                ((dx * dx + dy * dy).sqrt() - 0.22).abs() - 0.048
            };
            let bar_dist = sd_segment(px, py, 0.5, 0.23, 0.5, 0.48) - 0.048;
            let glyph_d = ring_dist.min(bar_dist);
            let glyph_a = cov(glyph_d, aa);
            blend_over(pixel, fg_r, fg_g, fg_b, glyph_a);
        }
    }

    buf
}

/// Render a quit ('x') icon for the Windows taskbar thumbnail toolbar.
pub fn render_thumb_quit_rgba(size: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let aa = 1.25 / (size as f32);

    for py_i in 0..size {
        let py = (py_i as f32 + 0.5) / (size as f32);
        for px_i in 0..size {
            let px = (px_i as f32 + 0.5) / (size as f32);
            let idx = ((py_i * size + px_i) * 4) as usize;
            let pixel = &mut buf[idx..idx + 4];

            let badge_d = ((px - 0.5).powi(2) + (py - 0.5).powi(2)).sqrt() - 0.44;
            let badge_a = cov(badge_d, aa);
            if badge_a > 0.0 {
                let rim = cov(badge_d.abs() - 0.02, aa);
                let r = mix_u8(56, 96, rim * 0.7);
                let g = mix_u8(53, 90, rim * 0.7);
                let b = mix_u8(50, 84, rim * 0.7);
                blend_over(pixel, r, g, b, badge_a);
            }

            let d1 = sd_segment(px, py, 0.34, 0.34, 0.66, 0.66) - 0.048;
            let d2 = sd_segment(px, py, 0.66, 0.34, 0.34, 0.66) - 0.048;
            let glyph_a = cov(d1.min(d2), aa);
            blend_over(pixel, 232, 229, 225, glyph_a);
        }
    }

    buf
}

// On Linux dev VMs we don't link the tray subsystem (no GTK), so these
// helpers go unused there. They remain compiled and unit-tested for parity.
#![cfg_attr(
    not(any(target_os = "windows", target_os = "macos")),
    allow(dead_code)
)]

//! Programmatically-generated tray icons.
//!
//! Each [`TrayState`] gets a 16x16 RGBA buffer with a transparent background
//! and a filled circle in the state color. The colors come from the spec:
//! - `DiscordOffline` -> `#71808a` (grey)
//! - `Idle`           -> `#5865f2` (Discord blurple)
//! - `InVoice`        -> `#57f287` (Discord green)
//! - `NeedsSetup`     -> `#faa61a` (amber)
//!
//! Icons are produced at runtime — no disk or network reads — and embedded
//! into the binary only by virtue of being constants in source.

use crate::state::TrayState;

const ICON_SIZE: u32 = 16;
/// Filled circle radius. ~5px on a 16x16 canvas keeps the glyph readable
/// while leaving an alpha-clean border.
const RADIUS: f32 = 5.0;

fn color_for(state: TrayState) -> (u8, u8, u8) {
    match state {
        TrayState::DiscordOffline => (0x71, 0x80, 0x8a),
        TrayState::Idle => (0x58, 0x65, 0xf2),
        TrayState::InVoice => (0x57, 0xf2, 0x87),
        TrayState::NeedsSetup => (0xfa, 0xa6, 0x1a),
    }
}

/// Returns `(rgba_bytes, width, height)` for the given tray state.
///
/// The buffer length is `width * height * 4`. Suitable for
/// `tray_icon::Icon::from_rgba`.
pub fn rgba_for(state: TrayState) -> (Vec<u8>, u32, u32) {
    let (r, g, b) = color_for(state);
    let mut buf = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let cx = (ICON_SIZE as f32 - 1.0) / 2.0;
    let cy = (ICON_SIZE as f32 - 1.0) / 2.0;
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            // Smooth 1px edge for a less aliased circle.
            let alpha = if dist <= RADIUS - 0.5 {
                255u8
            } else if dist <= RADIUS + 0.5 {
                let t = (RADIUS + 0.5 - dist).clamp(0.0, 1.0);
                (t * 255.0).round() as u8
            } else {
                0u8
            };
            let i = ((y * ICON_SIZE + x) * 4) as usize;
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = alpha;
        }
    }
    (buf, ICON_SIZE, ICON_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgba_buffer_is_correct_size() {
        for state in [
            TrayState::DiscordOffline,
            TrayState::Idle,
            TrayState::InVoice,
            TrayState::NeedsSetup,
        ] {
            let (buf, w, h) = rgba_for(state);
            assert_eq!(w, 16);
            assert_eq!(h, 16);
            // 16 * 16 * 4 = 1024 bytes RGBA.
            assert_eq!(buf.len(), 1024);
        }
    }

    #[test]
    fn rgba_center_pixel_is_state_color() {
        // The very center of the canvas should be fully opaque and match
        // the state color — sanity check that we're rendering at all.
        let (buf, w, _) = rgba_for(TrayState::Idle);
        let center = ((w / 2) * w + (w / 2)) as usize * 4;
        assert_eq!(buf[center], 0x58);
        assert_eq!(buf[center + 1], 0x65);
        assert_eq!(buf[center + 2], 0xf2);
        assert_eq!(buf[center + 3], 255);
    }

    #[test]
    fn rgba_corner_pixel_is_transparent() {
        // Corners should be fully transparent — circle must not fill the
        // entire canvas.
        let (buf, _, _) = rgba_for(TrayState::InVoice);
        assert_eq!(buf[3], 0, "top-left corner alpha must be 0");
    }
}

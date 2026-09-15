//! The app icon.
//!
//! The artwork lives at `assets/icon-source.jpg`; `scripts/make-icon.ps1`
//! converts it into `assets/icon.rgba`, which is embedded here.
//!
//! # Why a raw blob rather than a PNG
//!
//! One icon does not justify pulling a JPEG/PNG decoder into a Bluetooth audio
//! daemon. Decoding happens once, offline, in the script; the crate ships
//! tightly packed RGBA and hands it straight to eframe.
//!
//! Re-run the script if the artwork changes. It also handles the two things
//! that matter for a dark title bar: the background is clamped to **true**
//! black (the source is a JPEG, so its "black" is 0-2 plus compression noise
//! and an ambient glow, which renders as a visible tile), and the artwork is
//! autocropped so the fang fills the frame instead of floating in an empty
//! canvas.

use eframe::egui::IconData;

/// Edge length of the embedded image. Must match `scripts/make-icon.ps1`.
const SIZE: u32 = 128;

/// Tightly packed RGBA, row-major, fully opaque.
const PIXELS: &[u8] = include_bytes!("../../assets/icon.rgba");

pub fn icon_data() -> IconData {
    IconData {
        rgba: PIXELS.to_vec(),
        width: SIZE,
        height: SIZE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_blob_matches_the_declared_size() {
        // The blob and SIZE come from different tools - the script and this
        // constant - so nothing but this check keeps them in step. A mismatch
        // would have eframe read past the end or render garbage.
        assert_eq!(
            PIXELS.len(),
            (SIZE * SIZE * 4) as usize,
            "icon.rgba is {} bytes; {SIZE}x{SIZE} RGBA needs {}",
            PIXELS.len(),
            SIZE * SIZE * 4
        );
    }

    #[test]
    fn it_is_fully_opaque() {
        // A transparent background would let the taskbar show through the
        // fang. The art is composited on black, not cut out.
        assert!(PIXELS.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn the_background_is_true_black() {
        // The explicit requirement. The source JPEG's background is 0-2 with
        // noise plus an ambient glow; anything above zero shows as a lighter
        // square around the icon on a dark title bar.
        let corner = |x: u32, y: u32| {
            let i = ((y * SIZE + x) * 4) as usize;
            (PIXELS[i], PIXELS[i + 1], PIXELS[i + 2])
        };
        for (x, y) in [(0, 0), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(corner(x, y), (0, 0, 0), "corner ({x},{y}) is not black");
        }
    }

    #[test]
    fn the_artwork_actually_fills_the_frame() {
        // Guards a regression in the script's autocrop: a correctly sized blob
        // of entirely black pixels would pass every other test here.
        let lit = PIXELS.chunks(4).filter(|p| p[0] > 24 || p[2] > 24).count();
        let total = (SIZE * SIZE) as usize;
        assert!(
            lit * 100 / total > 4,
            "only {lit}/{total} pixels carry the artwork - autocrop likely broke"
        );
    }
}

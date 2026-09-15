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
const SIZE: u32 = 256;

/// Tightly packed RGBA, row-major. Opaque except the rounded corners.
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

    fn px(x: u32, y: u32) -> (u8, u8, u8, u8) {
        let i = ((y * SIZE + x) * 4) as usize;
        (PIXELS[i], PIXELS[i + 1], PIXELS[i + 2], PIXELS[i + 3])
    }

    #[test]
    fn the_corners_are_transparent() {
        // Rounded corners only read as rounded if what is behind the icon
        // shows through. Drawing black corners would leave it looking exactly
        // as square as before.
        for (x, y) in [(0, 0), (SIZE - 1, 0), (0, SIZE - 1), (SIZE - 1, SIZE - 1)] {
            assert_eq!(px(x, y).3, 0, "corner ({x},{y}) is not transparent");
        }
    }

    #[test]
    fn the_body_is_opaque_and_the_background_is_true_black() {
        // Everything inside the rounded rectangle keeps the original contract:
        // fully opaque, on a background of exactly #000000. The source JPEG's
        // "black" is 0-2 with noise plus an ambient glow, and anything above
        // zero shows as a lighter tile on a dark title bar.
        let mid = SIZE / 2;
        assert_eq!(px(mid, mid).3, 255, "the middle must be opaque");

        // A point on the straight part of the top edge - past the corner
        // radius, so unaffected by the mask, and above the artwork.
        let edge = px(mid, 1);
        assert_eq!(edge.3, 255, "the straight edge must stay opaque");
        assert_eq!(
            (edge.0, edge.1, edge.2),
            (0, 0, 0),
            "background is not true black"
        );
    }

    #[test]
    fn the_rounding_is_visible_but_restrained() {
        // Guards both ways: a mask that did nothing, and one that ate the
        // artwork. Counts transparent pixels, which for an 18% radius on a
        // square should be a few percent of the image.
        let clear = PIXELS.chunks(4).filter(|p| p[3] == 0).count();
        let total = (SIZE * SIZE) as usize;
        let percent = clear * 100 / total;
        assert!(percent >= 1, "no rounding applied ({percent}%)");
        assert!(percent <= 10, "corners are eating the icon ({percent}%)");
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

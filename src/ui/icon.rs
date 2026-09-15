//! The app icon, rasterised at runtime.
//!
//! Drawn in code rather than shipped as a PNG so the crate needs no image
//! decoder for one small asset, and so the same shape can be re-rendered at
//! whatever size a future tray icon wants.
//!
//! A stylised sabertooth fang on black: root nodules across the crown, a long
//! scimitar curve bowing right, and a bead of liquid coming off the point.
//!
//! The curve is asymmetric on purpose. A symmetric taper reads as a dagger no
//! matter how long it is; a saber gets its character from a **concave inner
//! edge and a convex outer edge**, so the two are defined separately rather
//! than as a centre line with a half-width.

use eframe::egui::IconData;

/// Deep purple. Lifted off true violet enough to stay legible against black at
/// 16px, where a darker shade turns into an indistinct smudge.
const PURPLE: [u8; 3] = [0x7A, 0x2B, 0xC4];
/// A lighter inner edge. Just enough to read as a solid object rather than a
/// flat silhouette; deliberately subtle, per CLAUDE.md's "no decorative chrome".
const PURPLE_LIT: [u8; 3] = [0x9A, 0x4B, 0xE4];

/// Supersampling factor per axis. The shape is all diagonals and curves, and
/// without this the taper looks like a staircase at small sizes.
const SS: u32 = 4;

// --- geometry, all normalised 0..1 with y running downward -------------------
//
// The blade is a *swept disk*: a curved spine with a radius that shrinks toward
// the point. Thickness is therefore measured perpendicular to the curve, which
// is what makes it read as a curved tooth.
//
// An earlier attempt offset a half-width horizontally from a tilted centre
// line. That shears the shape rather than bending it - the edges stop being
// parallel to the spine and the result looks like a leaning spike.

/// Spine control points for a quadratic Bezier.
///
/// The control point sits well to the right of the chord, which is what bows
/// the belly of the curve outward to the right rather than merely leaning the
/// tip.
const P0: (f32, f32) = (0.26, 0.22);
const P1: (f32, f32) = (0.72, 0.38);
const P2: (f32, f32) = (0.60, 0.80);

/// Radius of the sweep at the crown.
const CROWN_R: f32 = 0.150;

/// Root nodules, sitting on the top arc of the crown so they read as bumps.
/// Placed above the spine point rather than on it - centred on it they would
/// sit entirely inside the crown disk and be invisible.
const NODULE_Y: f32 = 0.125;
const NODULE_R: f32 = 0.075;
const NODULE_DX: f32 = 0.095;

fn spine_at(t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    (
        u * u * P0.0 + 2.0 * u * t * P1.0 + t * t * P2.0,
        u * u * P0.1 + 2.0 * u * t * P1.1 + t * t * P2.1,
    )
}

/// Sweep radius at `t`.
///
/// Near-linear. The reference silhouette tapers steadily from a broad root to
/// a needle point; a much lower exponent keeps the blade fat and blunt, which
/// is what made earlier attempts read as a boot rather than a saber.
fn radius_at(t: f32) -> f32 {
    CROWN_R * (1.0 - t).powf(0.95)
}

fn tip_x() -> f32 {
    spine_at(1.0).0
}

/// How many spine samples the swept-disk test walks.
///
/// Coarser than this and the outline beads visibly along the curve.
const SPINE_STEPS: usize = 160;

fn blade_inside(x: f32, y: f32) -> bool {
    for i in 0..=SPINE_STEPS {
        let t = i as f32 / SPINE_STEPS as f32;
        let (sx, sy) = spine_at(t);
        let r = radius_at(t);
        let (dx, dy) = (x - sx, y - sy);
        if dx * dx + dy * dy <= r * r {
            return true;
        }
    }
    false
}

/// A bead of liquid coming off the point.
///
/// A teardrop that overlaps the point slightly, so it reads as connected
/// liquid rather than a stray dot.
fn drip_inside(x: f32, y: f32) -> bool {
    const NECK_TOP: f32 = 0.775;
    const BEAD_Y: f32 = 0.930;
    const BEAD_R: f32 = 0.048;

    let cx = tip_x();

    let (dx, dy) = (x - cx, y - BEAD_Y);
    if dx * dx + dy * dy <= BEAD_R * BEAD_R {
        return true;
    }

    if (NECK_TOP..=BEAD_Y).contains(&y) {
        let t = (y - NECK_TOP) / (BEAD_Y - NECK_TOP);
        // Concave: the neck stays thin and only flares into the bead near the
        // bottom, so the point of the tooth stays visible above it.
        let half = BEAD_R * t.powf(1.9);
        return (x - cx).abs() <= half;
    }

    false
}

fn inside(x: f32, y: f32) -> bool {
    for cx in [P0.0 - NODULE_DX, P0.0, P0.0 + NODULE_DX] {
        let (dx, dy) = (x - cx, y - NODULE_Y);
        if dx * dx + dy * dy <= NODULE_R * NODULE_R {
            return true;
        }
    }
    blade_inside(x, y) || drip_inside(x, y)
}

/// Whether a point sits on the lit (outer) side of the blade.
///
/// Measured against the nearest spine point so the highlight follows the bow
/// all the way round, which is what gives the arc its sense of volume.
fn is_lit(x: f32, y: f32) -> bool {
    let mut best = (f32::MAX, 0.0f32);
    for i in 0..=SPINE_STEPS {
        let t = i as f32 / SPINE_STEPS as f32;
        let (sx, sy) = spine_at(t);
        let d = (x - sx) * (x - sx) + (y - sy) * (y - sy);
        if d < best.0 {
            best = (d, sx);
        }
    }
    x > best.1
}

pub fn icon_data() -> IconData {
    const SIZE: u32 = 64;
    IconData {
        rgba: rasterise(SIZE),
        width: SIZE,
        height: SIZE,
    }
}

fn rasterise(size: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    let samples = (SS * SS) as f32;

    for py in 0..size {
        for px in 0..size {
            let mut hits = 0u32;
            let mut lit = 0u32;

            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if inside(x, y) {
                        hits += 1;
                        if is_lit(x, y) {
                            lit += 1;
                        }
                    }
                }
            }

            if hits == 0 {
                // Opaque black background, as asked for - not transparency,
                // which would show the taskbar through the fang.
                rgba.extend_from_slice(&[0, 0, 0, 255]);
                continue;
            }

            let coverage = hits as f32 / samples;
            let base = if lit * 2 > hits { PURPLE_LIT } else { PURPLE };
            // Composite over black, so partial coverage darkens toward the
            // background rather than going transparent.
            rgba.extend_from_slice(&[
                (base[0] as f32 * coverage) as u8,
                (base[1] as f32 * coverage) as u8,
                (base[2] as f32 * coverage) as u8,
                255,
            ]);
        }
    }

    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width_at(y: f32) -> usize {
        (0..2000).filter(|i| inside(*i as f32 / 2000.0, y)).count()
    }

    #[test]
    fn the_buffer_is_the_size_eframe_expects() {
        let icon = icon_data();
        assert_eq!(
            icon.rgba.len(),
            (icon.width * icon.height * 4) as usize,
            "eframe indexes this as tightly packed RGBA"
        );
    }

    #[test]
    fn it_is_fully_opaque() {
        // A transparent background would let the taskbar show through the
        // fang, which is not what was asked for.
        assert!(icon_data().rgba.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn the_point_is_narrower_than_the_crown() {
        assert!(
            width_at(0.74) < width_at(0.18),
            "the blade must taper toward the point"
        );
    }

    #[test]
    fn the_blade_keeps_its_body_halfway_down() {
        // Guards the hairline-spike failure, where the blade collapses
        // immediately and reads as a thin dagger.
        //
        // The floor is 0.45 rather than something higher because the reference
        // silhouette tapers steadily from root to needle; insisting the blade
        // still be over half its width at the midpoint forces the blunt,
        // boot-like shape this went through earlier. Low enough to catch a
        // collapse, loose enough to allow a real taper.
        assert!(
            radius_at(0.5) > CROWN_R * 0.45,
            "radius at midpoint was {}, too thin",
            radius_at(0.5)
        );
        assert!(
            radius_at(0.95) < CROWN_R * 0.12,
            "and it must actually come to a point: {}",
            radius_at(0.95)
        );
    }

    #[test]
    fn the_spine_bows_out_to_the_right() {
        // THE defining property. The belly of the curve must sit right of the
        // straight line between crown and point - a spine that merely leans
        // has its midpoint ON that line, and reads as a slanted spike.
        let (mx, _) = spine_at(0.5);
        let chord_mid_x = (P0.0 + P2.0) / 2.0;
        assert!(
            mx > chord_mid_x + 0.08,
            "midpoint {mx} should bow well right of the chord {chord_mid_x}"
        );
    }

    #[test]
    fn the_curve_is_monotonic_downward() {
        // A spine that doubles back would render as a hook, not a tooth.
        let mut prev = spine_at(0.0).1;
        for i in 1..=100 {
            let y = spine_at(i as f32 / 100.0).1;
            assert!(y > prev, "spine must descend monotonically");
            prev = y;
        }
    }

    #[test]
    fn the_drip_hangs_under_the_point() {
        assert!(inside(tip_x(), 0.92), "the bead must be painted there");
        assert!(
            !inside(P0.0 - NODULE_DX, 0.92),
            "and not under the crown, or it fell off the wrong end"
        );
    }

    #[test]
    fn the_drip_is_connected_to_the_tooth() {
        // A gap makes it read as a stray dot at 16px.
        assert!(
            inside(tip_x(), 0.80),
            "neck must meet the blade rather than float below it"
        );
    }

    #[test]
    fn everything_stays_inside_the_canvas() {
        // The sweep and the drip both push right; a shape clipped by the icon
        // edge looks like a rendering bug rather than a design.
        for i in 0..1000 {
            let y = i as f32 / 1000.0;
            for j in 0..1000 {
                let x = j as f32 / 1000.0;
                if inside(x, y) {
                    assert!(
                        (0.01..=0.99).contains(&x) && (0.01..=0.99).contains(&y),
                        "shape touches the edge at ({x}, {y})"
                    );
                }
            }
        }
    }
}

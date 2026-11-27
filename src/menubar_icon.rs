// The macOS menu-bar icon: a thin outlined microphone in the style of Apple's SF Symbols, plus a slashed
// variant for "muted". Drawn from distance functions with 4x4 supersampling, so edges are anti-aliased at any
// size. Output is black with alpha; macOS treats it as a template image and recolours it for the menu bar.
//
// Self-contained (no crate imports) so it can be rendered to a PNG outside the app for previewing.

/// Distance from p to the segment a-b.
fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (ax, ay) = (p.0 - a.0, p.1 - a.1);
    let (bx, by) = (b.0 - a.0, b.1 - a.1);
    let len2 = bx * bx + by * by;
    let t = if len2 > 0.0 { ((ax * bx + ay * by) / len2).clamp(0.0, 1.0) } else { 0.0 };
    ((ax - bx * t).powi(2) + (ay - by * t).powi(2)).sqrt()
}

/// True if the point (in 18x18 "points", the menu-bar icon grid) is ink.
fn ink(x: f32, y: f32, muted: bool) -> bool {
    let w = 1.45; // stroke width in points, close to the system symbols at regular weight
    let half = w / 2.0;
    let p = (x, y);
    let cx = 9.0;

    // Capsule outline: a rounded rectangle 6 pt wide, from y=1.6 to y=11.4.
    let r = 3.0;
    let capsule = (seg_dist(p, (cx, 1.6 + r), (cx, 11.4 - r)) - r).abs() <= half;

    // The cradle: lower half of a circle around the capsule, with round ends.
    let (acy, ar) = (8.0, 5.6);
    let arc = if y >= acy {
        ((((x - cx).powi(2) + (y - acy).powi(2)).sqrt()) - ar).abs() <= half
    } else {
        seg_dist(p, (cx - ar, acy), (cx - ar, acy)) <= half || seg_dist(p, (cx + ar, acy), (cx + ar, acy)) <= half
    };

    // Stem down to the bottom edge.
    let stem = seg_dist(p, (cx, acy + ar), (cx, 16.6)) <= half;

    let shape = capsule || arc || stem;
    if !muted {
        return shape;
    }
    // Muted: a diagonal slash, with a transparent gap on one side so it reads clearly against the outline.
    let (a, b) = ((3.2, 1.8), (14.8, 16.2));
    let d = seg_dist(p, a, b);
    let side = (b.0 - a.0) * (y - a.1) - (b.1 - a.1) * (x - a.0); // >0 below-left of the slash
    if d <= half {
        true
    } else if d <= half + 1.1 && side > 0.0 {
        false
    } else {
        shape
    }
}

/// RGBA pixels (size x size) of the icon; 36 px is the 18 pt menu-bar height on a Retina screen.
pub fn rgba(size: u32, muted: bool) -> Vec<u8> {
    const SS: u32 = 4; // supersamples per axis
    let scale = 18.0 / size as f32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) * scale;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) * scale;
                    hits += ink(x, y, muted) as u32;
                }
            }
            let a = (hits * 255 / (SS * SS)) as u8;
            out.extend_from_slice(&[0, 0, 0, a]);
        }
    }
    out
}

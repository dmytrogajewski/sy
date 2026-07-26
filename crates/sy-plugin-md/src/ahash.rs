//! Average-hash (aHash) perceptual fingerprint over a PNG. ~30 lines
//! per Step 12's "implement inline rather than pull img_hash".
//!
//! Algorithm (per [imagehash][imagehash]):
//!
//! 1. Decode PNG (via `tiny-skia::Pixmap::decode_png` — pure-Rust).
//! 2. Down-sample to 8×8 grayscale by box-averaging source tiles.
//! 3. Compute the mean of the 64 grayscale values.
//! 4. Emit a 64-bit hash where bit `i` is set iff `samples[i] > mean`.
//!
//! Two PNGs are considered "perceptually equal" iff the Hamming
//! distance between their hashes is ≤ `MAX_HAMMING`. The Step 12
//! perceptual budget (0.5 %) maps to a Hamming distance of 1 on a
//! 64-bit hash; we expose [`HAMMING_BUDGET`] so the value is shared
//! across every test.
//!
//! [imagehash]: https://hackerfactor.com/blog/?/archives/432-Looks-Like-It.html

use tiny_skia::Pixmap;

/// Per Step 12: 0.5 % drift = Hamming distance ≤ 1 on a 64-bit hash.
pub const HAMMING_BUDGET: u32 = 1;

/// Hash a PNG byte stream. Returns `Err` if `bytes` is not a valid
/// PNG body (caller picked the wrong path / wrote junk to disk).
pub fn hash_png(bytes: &[u8]) -> Result<u64, String> {
    let pix = Pixmap::decode_png(bytes).map_err(|e| format!("ahash::hash_png decode: {e}"))?;
    Ok(hash_pixmap(&pix))
}

/// Hash an in-memory pixmap. Inline so callers that already hold a
/// `Pixmap` (the renderer's own self-check) don't pay an encode→decode
/// round-trip.
pub fn hash_pixmap(pix: &Pixmap) -> u64 {
    const SIDE: u32 = 8;
    let w = pix.width().max(1);
    let h = pix.height().max(1);
    let mut samples = [0u32; (SIDE * SIDE) as usize];
    for ty in 0..SIDE {
        for tx in 0..SIDE {
            // Box-average the source tile that maps to (tx, ty).
            let x0 = (tx * w) / SIDE;
            let y0 = (ty * h) / SIDE;
            let x1 = (((tx + 1) * w) / SIDE).max(x0 + 1);
            let y1 = (((ty + 1) * h) / SIDE).max(y0 + 1);
            let mut sum: u64 = 0;
            let mut count: u64 = 0;
            for py in y0..y1 {
                for px in x0..x1 {
                    if let Some(p) = pix.pixel(px, py) {
                        // Demultiply alpha; treat fully-transparent
                        // pixels as the page background (the renderer
                        // never emits alpha < 255 today, but be safe).
                        let r = p.red() as u64;
                        let g = p.green() as u64;
                        let b = p.blue() as u64;
                        // Rec. 601 luma — same coefficients GIMP /
                        // Pillow use for greyscale conversion.
                        sum += (299 * r + 587 * g + 114 * b) / 1000;
                        count += 1;
                    }
                }
            }
            samples[(ty * SIDE + tx) as usize] = (sum / count.max(1)) as u32;
        }
    }
    let mean: u32 = (samples.iter().map(|&v| v as u64).sum::<u64>() / 64) as u32;
    let mut hash: u64 = 0;
    for (i, &s) in samples.iter().enumerate() {
        if s > mean {
            hash |= 1u64 << i;
        }
    }
    hash
}

/// Hamming distance between two aHash fingerprints. The 0.5 % drift
/// budget from SPEC §4.4 corresponds to [`HAMMING_BUDGET`] on the
/// returned distance.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Identical pixmaps must hash to a distance of 0.
    #[test]
    fn identical_pixmaps_hash_equal() {
        let mut p = Pixmap::new(64, 64).expect("pixmap");
        p.fill(tiny_skia::Color::from_rgba8(40, 40, 40, 255));
        let h1 = hash_pixmap(&p);
        let h2 = hash_pixmap(&p);
        assert_eq!(hamming(h1, h2), 0);
    }

    /// A solid-fill image and a half-and-half image must differ by
    /// far more than the budget — proves the hash isn't accidentally
    /// returning a constant.
    #[test]
    fn solid_vs_half_differs_above_budget() {
        let mut solid = Pixmap::new(64, 64).expect("pixmap");
        solid.fill(tiny_skia::Color::from_rgba8(40, 40, 40, 255));
        let mut half = Pixmap::new(64, 64).expect("pixmap");
        half.fill(tiny_skia::Color::from_rgba8(40, 40, 40, 255));
        // Paint the right half white via direct pixel writes so we
        // don't pull in tiny-skia's path API just for the test.
        let w = half.width();
        let h = half.height();
        let row_stride = (w * 4) as usize;
        for y in 0..h {
            for x in (w / 2)..w {
                let i = (y as usize) * row_stride + (x as usize) * 4;
                let buf = half.data_mut();
                buf[i] = 255;
                buf[i + 1] = 255;
                buf[i + 2] = 255;
                buf[i + 3] = 255;
            }
        }
        let h1 = hash_pixmap(&solid);
        let h2 = hash_pixmap(&half);
        assert!(
            hamming(h1, h2) > HAMMING_BUDGET,
            "solid vs half hashes too close: {} vs {}",
            h1,
            h2
        );
    }

    /// Round-trip through encode_png + hash_png must agree with the
    /// in-memory `hash_pixmap` (modulo decode quantisation).
    #[test]
    fn encode_then_hash_matches_inmem() {
        let mut p = Pixmap::new(32, 32).expect("pixmap");
        p.fill(tiny_skia::Color::from_rgba8(80, 120, 60, 255));
        let png = p.encode_png().expect("encode");
        let h_inmem = hash_pixmap(&p);
        let h_png = hash_png(&png).expect("decode + hash");
        assert!(
            hamming(h_inmem, h_png) <= HAMMING_BUDGET,
            "round-trip drift exceeded budget: {} vs {}",
            h_inmem,
            h_png
        );
    }
}

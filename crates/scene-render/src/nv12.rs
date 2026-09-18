//! RGBA8 → NV12 (4:2:0 semi-planar) conversion, BT.709 limited range —
//! the range/colorspace the mux tags the stream with.
//!
//! NV12 plane layout: `w*h` Y bytes, then `w*h/2` interleaved UV bytes.
//! Chroma is computed per 2×2 block from the block's mean RGB — a box
//! filter, which is what swscale's default does at this quality level.

/// BT.709 limited-range luma from 8-bit R,G,B.
fn y_of(r: f64, g: f64, b: f64) -> u8 {
    (16.0 + (0.2126 * r + 0.7152 * g + 0.0722 * b) * (219.0 / 255.0))
        .round()
        .clamp(0.0, 255.0) as u8
}

/// BT.709 limited-range chroma (Cb or Cr) from the 2×2 block mean.
fn c_of(u: bool, r: f64, g: f64, b: f64) -> u8 {
    let v = if u {
        -0.1146 * r - 0.3854 * g + 0.5 * b
    } else {
        0.5 * r - 0.4542 * g - 0.0458 * b
    };
    (128.0 + v * (224.0 / 255.0)).round().clamp(0.0, 255.0) as u8
}

/// Convert one **premultiplied** RGBA8 frame (tiny-skia's native layout —
/// color already composited over black) to NV12. `rgba.len()` must be
/// `w*h*4`; `w` and `h` must be even (a canvas is always even — enforced
/// by the renderer).
pub fn rgba_to_nv12(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    debug_assert_eq!(rgba.len(), w * h * 4);
    debug_assert!(w % 2 == 0 && h % 2 == 0);
    let mut out = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for row in 0..h {
        for col in 0..w {
            let px = (row * w + col) * 4;
            let (r, g, b) = (
                f64::from(rgba[px]),
                f64::from(rgba[px + 1]),
                f64::from(rgba[px + 2]),
            );
            y_plane[row * w + col] = y_of(r, g, b);
            if row % 2 == 0 && col % 2 == 0 {
                // 2×2 box-filter mean for the shared chroma sample.
                let mut rs = 0.0;
                let mut gs = 0.0;
                let mut bs = 0.0;
                for dr in 0..2usize {
                    for dc in 0..2usize {
                        let p = ((row + dr) * w + col + dc) * 4;
                        rs += f64::from(rgba[p]);
                        gs += f64::from(rgba[p + 1]);
                        bs += f64::from(rgba[p + 2]);
                    }
                }
                let (r, g, b) = (rs / 4.0, gs / 4.0, bs / 4.0);
                let idx = (row / 2) * w + col;
                uv_plane[idx] = c_of(true, r, g, b);
                uv_plane[idx + 1] = c_of(false, r, g, b);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size() {
        let rgba = vec![0u8; 4 * 2 * 4];
        assert_eq!(rgba_to_nv12(&rgba, 4, 2).len(), 4 * 2 * 3 / 2);
    }

    #[test]
    fn black_is_black() {
        let rgba = vec![0u8; 4 * 2 * 4]; // a=0 → premultiplied black
        let nv12 = rgba_to_nv12(&rgba, 4, 2);
        assert!(nv12[..8].iter().all(|&y| y == 16));
        assert!(nv12[8..].iter().all(|&c| c == 128));
    }

    #[test]
    fn white_is_white() {
        let mut rgba = Vec::new();
        for _ in 0..8 {
            rgba.extend_from_slice(&[255, 255, 255, 255]);
        }
        let nv12 = rgba_to_nv12(&rgba, 4, 2);
        assert!(nv12[..8].iter().all(|&y| y == 235));
        assert!(nv12[8..].iter().all(|&c| c == 128));
    }

    #[test]
    fn red_is_bt709_red() {
        let mut rgba = Vec::new();
        for _ in 0..8 {
            rgba.extend_from_slice(&[255, 0, 0, 255]);
        }
        let nv12 = rgba_to_nv12(&rgba, 4, 2);
        // BT.709 limited: Y≈63, Cr≈240, Cb≈102 — not BT.601's values
        assert_eq!(nv12[0], 63);
        assert_eq!(nv12[8], 102); // Cb
        assert_eq!(nv12[9], 240); // Cr
    }

    #[test]
    fn deterministic() {
        let rgba: Vec<u8> = (0..4 * 4 * 4).map(|i| (i * 37 % 256) as u8).collect();
        assert_eq!(rgba_to_nv12(&rgba, 4, 4), rgba_to_nv12(&rgba, 4, 4));
    }
}

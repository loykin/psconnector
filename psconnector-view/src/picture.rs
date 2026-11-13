//! Picture adjustment for the window: lift crushed shadows, roll off blown highlights, contrast,
//! saturation and warmth. Only the picture shown on screen is changed; macros, OCR and screenshots see
//! the console's original frames.
//!
//! The brightness curve works on luminance and scales each pixel's colour with it, so hues stay put.
//! Very dark pixels get their lift as neutral grey instead of a large multiplier, which would otherwise
//! blow up compression noise into coloured blotches.

use eframe::egui::Color32;
use rayon::prelude::*;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Picture {
    /// -0.5 … 0.5 exposure stops
    pub brightness: f32,
    /// 0 … 1: lift dark areas
    pub shadows: f32,
    /// 0 … 1: pull bright areas down
    pub highlights: f32,
    /// -0.5 … 0.5
    pub contrast: f32,
    /// 0 … 1.5 (1 = unchanged)
    pub saturation: f32,
    /// -1 (cooler) … 1 (warmer)
    pub warmth: f32,
    /// 0 … 1: take the colour cast (often blue) out of dark areas
    pub shadow_tint: f32,
}

impl Default for Picture {
    fn default() -> Self {
        Self {
            brightness: 0.0,
            shadows: 0.0,
            highlights: 0.0,
            contrast: 0.0,
            saturation: 1.0,
            warmth: 0.0,
            shadow_tint: 0.0,
        }
    }
}

impl Picture {
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    pub fn to_json(self) -> Value {
        json!({
            "brightness": self.brightness, "shadows": self.shadows, "highlights": self.highlights, "contrast": self.contrast,
            "saturation": self.saturation, "warmth": self.warmth, "shadow_tint": self.shadow_tint,
        })
    }

    /// Takes the fields present in `v`, keeping the others; values are clamped to their ranges.
    pub fn merged(self, v: &Value) -> Self {
        let get = |k: &str, cur: f32| v[k].as_f64().map(|x| x as f32).unwrap_or(cur);
        Self {
            brightness: get("brightness", self.brightness).clamp(-0.5, 0.5),
            shadows: get("shadows", self.shadows).clamp(0.0, 1.0),
            highlights: get("highlights", self.highlights).clamp(0.0, 1.0),
            contrast: get("contrast", self.contrast).clamp(-0.5, 0.5),
            saturation: get("saturation", self.saturation).clamp(0.0, 1.5),
            warmth: get("warmth", self.warmth).clamp(-1.0, 1.0),
            shadow_tint: get("shadow_tint", self.shadow_tint).clamp(0.0, 1.0),
        }
    }

    /// Brightness curve on 0…1 luminance.
    fn curve(&self, x: f32) -> f32 {
        let x = (x * 2.0f32.powf(self.brightness)).clamp(0.0, 1.0);
        // shadows: a gamma lift that fades out towards the bright end, so mid tones and highlights stay put
        let lifted = x.powf(1.0 / (1.0 + self.shadows * 1.5));
        let w = (1.0 - x) * (1.0 - x);
        let x = lifted * w + x * (1.0 - w);
        // highlights: a soft shoulder from mid grey up (Reinhard-style, always rising); at full strength
        // white comes down to about 2/3
        const KNEE: f32 = 0.45;
        let x = if x > KNEE {
            let over = x - KNEE;
            KNEE + over / (1.0 + 1.5 * self.highlights * over / (1.0 - KNEE))
        } else {
            x
        };
        (0.5 + (x - 0.5) * (1.0 + self.contrast)).clamp(0.0, 1.0)
    }
}

/// Precomputed tables for one setting.
pub struct Lut {
    /// colour multiplier per luminance, ×256
    gain: [i32; 256],
    /// neutral grey added per luminance
    offset: [i32; 256],
    /// target luminance per luminance
    target: [i32; 256],
    /// colour kept per luminance after saturation and shadow tint, ×256
    chroma: [i32; 256],
    red: i32,
    blue: i32,
}

/// Largest colour multiplier; the rest of a dark pixel's lift is added as grey.
const MAX_GAIN: f32 = 2.0;

impl Lut {
    pub fn new(p: &Picture) -> Self {
        let mut gain = [256; 256];
        let mut offset = [0; 256];
        let mut target = [0; 256];
        let mut chroma = [0; 256];
        for y in 0..256 {
            let t = p.curve(y as f32 / 255.0) * 255.0;
            target[y] = t.round() as i32;
            let k = if y == 0 { 1.0 } else { (t / y as f32).min(MAX_GAIN) };
            gain[y] = (k * 256.0).round() as i32;
            offset[y] = (t - y as f32 * k).round() as i32;
            // shadow tint: full effect on near-black, fading out by about 40 % brightness
            let dark = (1.0 - y as f32 / 100.0).clamp(0.0, 1.0);
            chroma[y] = (p.saturation * (1.0 - p.shadow_tint * dark) * 256.0).round() as i32;
        }
        Self {
            gain,
            offset,
            target,
            chroma,
            red: ((1.0 + 0.08 * p.warmth) * 256.0).round() as i32,
            blue: ((1.0 - 0.08 * p.warmth) * 256.0).round() as i32,
        }
    }

    fn pixel(&self, px: &[u8]) -> [u8; 3] {
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        let y = ((54 * r + 183 * g + 19 * b) >> 8) as usize;
        let (k, off, t) = (self.gain[y], self.offset[y], self.target[y]);
        let mut c = [((r * k) >> 8) + off, ((g * k) >> 8) + off, ((b * k) >> 8) + off];
        let s = self.chroma[y];
        if s != 256 {
            for v in &mut c {
                *v = t + (((*v - t) * s) >> 8);
            }
        }
        c[0] = (c[0] * self.red) >> 8;
        c[2] = (c[2] * self.blue) >> 8;
        c.map(|v| v.clamp(0, 255) as u8)
    }

    /// Adjusts an RGB frame into `out` (resized as needed). Rayon keeps its worker threads alive between
    /// frames; creating scoped OS threads here cost a surprising amount at 60 fps.
    pub fn apply(&self, src: &[u8], out: &mut Vec<u8>) {
        out.resize(src.len(), 0);
        let threads = std::thread::available_parallelism().map(|n| n.get().min(8)).unwrap_or(2);
        let chunk = (src.len() / 3).div_ceil(threads) * 3;
        src.par_chunks(chunk.max(3)).zip(out.par_chunks_mut(chunk.max(3))).for_each(|(sc, oc)| {
            for (p, o) in sc.as_chunks::<3>().0.iter().zip(oc.as_chunks_mut::<3>().0.iter_mut()) {
                *o = self.pixel(p);
            }
        });
    }

    /// Applies the adjustment directly into egui's RGBA pixel format, avoiding a second
    /// full-frame RGB→RGBA conversion and the intermediate adjusted RGB buffer.
    pub fn apply_color32(&self, src: &[u8]) -> Vec<Color32> {
        src.par_chunks_exact(3)
            .map(|p| {
                let rgb = self.pixel(p);
                Color32::from_rgb(rgb[0], rgb[1], rgb[2])
            })
            .collect()
    }

    /// Integer lookup table consumed by the GPU picture shader. Keeping the exact
    /// CPU table makes switching renderers preserve every saved preset.
    pub(crate) fn gpu_table(&self) -> ([[i32; 4]; 256], [i32; 2]) {
        let table = std::array::from_fn(|i| [self.gain[i], self.offset[i], self.target[i], self.chroma[i]]);
        (table, [self.red, self.blue])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_changes_nothing() {
        let lut = Lut::new(&Picture::default());
        let src: Vec<u8> = (0..=255u8).flat_map(|v| [v, v / 2, 255 - v]).collect();
        let mut out = Vec::new();
        lut.apply(&src, &mut out);
        let worst = src.iter().zip(&out).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
        assert!(worst <= 2, "off by {worst}");
    }

    #[test]
    fn direct_color32_output_matches_rgb_output() {
        let p = Picture::default().merged(&json!({
            "brightness": -0.25, "shadows": 0.05, "highlights": 0.55,
            "contrast": -0.02, "saturation": 0.92, "shadow_tint": 0.05
        }));
        let lut = Lut::new(&p);
        let src: Vec<u8> = (0..=255u8).flat_map(|v| [v, 255 - v, v / 2]).collect();
        let mut rgb = Vec::new();
        lut.apply(&src, &mut rgb);
        let rgba = lut.apply_color32(&src);
        let flattened: Vec<u8> = rgba.into_iter().flat_map(|p| [p.r(), p.g(), p.b()]).collect();
        assert_eq!(flattened, rgb);
    }

    #[test]
    fn adjustment_lifts_shadows_keeps_black_neutral_and_curve_rising() {
        let p = Picture {
            brightness: -0.2,
            shadows: 0.6,
            highlights: 0.8,
            contrast: -0.05,
            saturation: 0.85,
            warmth: 0.3,
            shadow_tint: 0.7,
        };
        let lut = Lut::new(&p);
        let mut out = Vec::new();
        // a near-black, slightly blue pixel: lifted, but not turned purple
        lut.apply(&[4, 4, 9], &mut out);
        assert!(out[0] > 20, "shadows lifted: {out:?}");
        assert!((out[2] as i32 - out[0] as i32) < 12, "stays close to neutral: {out:?}");
        // bright pixels come down
        lut.apply(&[250, 250, 250], &mut out);
        assert!(out[0] < 240, "{out:?}");
        let ys: Vec<f32> = (0..=255).map(|y| p.curve(y as f32 / 255.0)).collect();
        assert!(ys.windows(2).all(|w| w[1] >= w[0] - 1e-6), "curve must not fold back");
    }

    #[test]
    fn json_merge_clamps() {
        let p = Picture::default().merged(&json!({"shadows": 3.0, "warmth": -0.5}));
        assert_eq!((p.shadows, p.warmth, p.saturation), (1.0, -0.5, 1.0));
    }
}

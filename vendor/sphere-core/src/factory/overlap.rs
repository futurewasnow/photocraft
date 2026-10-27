//! Bounded local correspondences on a continuous strip around the dual-lens seam.
//! This measures correspondence; it does not warp delivery pixels or solve occlusions.
use super::{Input, MeiLens, sample};
use crate::parallel::*;
use crate::{Error, Result, pixel_count};
use serde::Serialize;

const WIDTH: usize = 2048;
const HEIGHT: usize = 128;
const HALF: i32 = 4;
const SEARCH: i32 = 8;

pub struct Plan {
    lenses: [MeiLens; 2],
    xy: Vec<Option<[[f64; 2]; 2]>>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Match {
    pub first: [usize; 2],
    pub second: [f64; 2],
    pub correlation: f64,
    pub cycle_error_pixels: f64,
    pub first_rgb: Option<[f64; 3]>,
    pub second_rgb: Option<[f64; 3]>,
}
#[derive(Debug, Serialize)]
pub struct Alignment {
    pub width: usize,
    pub height: usize,
    pub vertical_span_degrees: f64,
    pub candidates: usize,
    pub matches: Vec<Match>,
}
struct Atlas {
    width: usize,
    height: usize,
    gray: Vec<f32>,
    rgb: Vec<[f32; 4]>,
}
impl Atlas {
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        if y < 0 || y >= self.height as i32 {
            None
        } else {
            Some(y as usize * self.width + x.rem_euclid(self.width as i32) as usize)
        }
    }
    fn patch(&self, x: i32, y: i32) -> Option<[f64; 81]> {
        let mut values = [0.; 81];
        for dy in -HALF..=HALF {
            for dx in -HALF..=HALF {
                let v = self.gray[self.index(x + dx, y + dy)?];
                if !v.is_finite() {
                    return None;
                }
                values[((dy + HALF) * 9 + dx + HALF) as usize] = f64::from(v);
            }
        }
        normalize(values)
    }
    fn fractional_patch(&self, x: f64, y: f64) -> Option<[f64; 81]> {
        let mut values = [0.; 81];
        for dy in -HALF..=HALF {
            for dx in -HALF..=HALF {
                let sx = x + f64::from(dx);
                let sy = y + f64::from(dy);
                let ix = sx.floor() as i32;
                let iy = sy.floor() as i32;
                let fx = sx - f64::from(ix);
                let fy = sy - f64::from(iy);
                let mut value = 0.;
                for (ox, oy, weight) in [
                    (0, 0, (1. - fx) * (1. - fy)),
                    (1, 0, fx * (1. - fy)),
                    (0, 1, (1. - fx) * fy),
                    (1, 1, fx * fy),
                ] {
                    if weight > 0. {
                        let v = self.gray[self.index(ix + ox, iy + oy)?];
                        if !v.is_finite() {
                            return None;
                        }
                        value += f64::from(v) * weight;
                    }
                }
                values[((dy + HALF) * 9 + dx + HALF) as usize] = value;
            }
        }
        normalize(values)
    }
    fn color(&self, x: f64, y: f64) -> Option<[f64; 3]> {
        let mut mean = [0.; 3];
        for dy in -1..=1 {
            for dx in -1..=1 {
                let sx = x + f64::from(dx);
                let sy = y + f64::from(dy);
                let ix = sx.floor() as i32;
                let iy = sy.floor() as i32;
                let fx = sx - f64::from(ix);
                let fy = sy - f64::from(iy);
                for (ox, oy, weight) in [
                    (0, 0, (1. - fx) * (1. - fy)),
                    (1, 0, fx * (1. - fy)),
                    (0, 1, (1. - fx) * fy),
                    (1, 1, fx * fy),
                ] {
                    if weight <= 0. {
                        continue;
                    }
                    let p = self.rgb[self.index(ix + ox, iy + oy)?];
                    if !p[3].is_finite()
                        || p[3] < 0.99
                        || p[..3]
                            .iter()
                            .any(|v| !v.is_finite() || !(0.005..=0.95).contains(v))
                    {
                        return None;
                    }
                    for c in 0..3 {
                        mean[c] += f64::from(p[c]) * weight / 9.;
                    }
                }
            }
        }
        Some(mean)
    }
}
fn normalize(mut values: [f64; 81]) -> Option<[f64; 81]> {
    let mean = values.iter().sum::<f64>() / 81.;
    for v in &mut values {
        *v -= mean;
    }
    let norm = values.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm < 0.045 {
        return None; // Less than 0.005 RMS contrast in sqrt-luminance.
    }
    for v in &mut values {
        *v /= norm;
    }
    Some(values)
}
fn locate(template: &[f64; 81], target: &Atlas, x: i32, y: i32) -> Option<(f64, f64, f64)> {
    let mut scores = [[-2.; 17]; 17];
    let mut best = (-2., 0, 0);
    for dy in -SEARCH..=SEARCH {
        for dx in -SEARCH..=SEARCH {
            if let Some(patch) = target.patch(x + dx, y + dy) {
                let score = template.iter().zip(patch).map(|(a, b)| a * b).sum::<f64>();
                scores[(dy + SEARCH) as usize][(dx + SEARCH) as usize] = score;
                if score > best.0 {
                    best = (score, dx, dy);
                }
            }
        }
    }
    if best.0 < 0.7 || best.1.abs() == SEARCH || best.2.abs() == SEARCH {
        return None;
    }
    let bx = (best.1 + SEARCH) as usize;
    let by = (best.2 + SEARCH) as usize;
    let peak = |a: f64, b: f64, c: f64| {
        let curvature = a - 2. * b + c;
        if a > -1. && c > -1. && curvature < -1e-9 {
            (0.5 * (a - c) / curvature).clamp(-0.5, 0.5)
        } else {
            0.
        }
    };
    let mut position = [f64::from(x + best.1), f64::from(y + best.2)];
    let refined = [
        position[0] + peak(scores[by][bx - 1], best.0, scores[by][bx + 1]),
        position[1] + peak(scores[by - 1][bx], best.0, scores[by + 1][bx]),
    ];
    if let Some(patch) = target.fractional_patch(refined[0], refined[1]) {
        let score = template.iter().zip(patch).map(|(a, b)| a * b).sum::<f64>();
        if score > best.0 {
            best.0 = score;
            position = refined;
        }
    }
    if best.0 < 0.9 {
        return None;
    }
    let mut runner = -2f64;
    for dy in -SEARCH..=SEARCH {
        for dx in -SEARCH..=SEARCH {
            if (dx - best.1).abs() > 2 || (dy - best.2).abs() > 2 {
                runner = runner.max(scores[(dy + SEARCH) as usize][(dx + SEARCH) as usize]);
            }
        }
    }
    if best.0 - runner < 0.04 {
        return None;
    }
    Some((position[0], position[1], best.0))
}
fn correspond(a: &Atlas, b: &Atlas, x: i32, y: i32) -> Option<Match> {
    let template = a.patch(x, y)?;
    let (qx, qy, score) = locate(&template, b, x, y)?;
    let (px, py, _) = locate(
        &b.fractional_patch(qx, qy)?,
        a,
        qx.round() as i32,
        qy.round() as i32,
    )?;
    let cycle = ((px - f64::from(x)).powi(2) + (py - f64::from(y)).powi(2)).sqrt();
    if cycle > 1. {
        return None;
    }
    Some(Match {
        first: [x.rem_euclid(a.width as i32) as usize, y as usize],
        second: [qx.rem_euclid(a.width as f64), qy],
        correlation: score,
        cycle_error_pixels: cycle,
        first_rgb: a.color(f64::from(x), f64::from(y)),
        second_rgb: b.color(qx, qy),
    })
}
impl Plan {
    pub fn new(lenses: [MeiLens; 2]) -> Result<Self> {
        for lens in &lenses {
            lens.validate()?;
        }
        let mut xy = Vec::new();
        xy.try_reserve_exact(WIDTH * HEIGHT)
            .map_err(|_| Error::Allocation)?;
        for y in 0..HEIGHT {
            let beta = (0.5 - (y as f64 + 0.5) / HEIGHT as f64) * 24f64.to_radians();
            for x in 0..WIDTH {
                let phi = (x as f64 + 0.5) / WIDTH as f64 * std::f64::consts::TAU;
                let local = [beta.cos() * phi.cos(), beta.cos() * phi.sin(), beta.sin()];
                let world = std::array::from_fn(|i| {
                    (0..3)
                        .map(|j| lenses[0].world_to_lens[j][i] * local[j])
                        .sum()
                });
                let p = lenses.each_ref().map(|l| l.project(world));
                xy.push(match p {
                    [Some(a), Some(b)]
                        if [a, b].iter().zip(&lenses).all(|(&(u, v, t), l)| {
                            t < 99f64.to_radians()
                                && u >= 3.
                                && v >= 3.
                                && u < f64::from(l.width) - 4.
                                && v < f64::from(l.height) - 4.
                        }) =>
                    {
                        Some([[a.0, a.1], [b.0, b.1]])
                    }
                    _ => None,
                });
            }
        }
        Ok(Self { lenses, xy })
    }
    /// Analyze linear BT.709 RGBA. Opaque finite pixels provide geometric texture;
    /// dark/clipped colour patches can still match but carry no photometric sample.
    pub fn analyze(&self, sources: [&[[f32; 4]]; 2]) -> Result<Alignment> {
        let mut atlases = Vec::new();
        for (j, source) in sources.iter().enumerate() {
            if source.len() != pixel_count(self.lenses[j].width, self.lenses[j].height)? {
                return Err(Error::Invalid("overlap source dimensions mismatch"));
            }
            let mut atlas = Atlas {
                width: WIDTH,
                height: HEIGHT,
                gray: Vec::new(),
                rgb: Vec::new(),
            };
            atlas
                .gray
                .try_reserve_exact(self.xy.len())
                .map_err(|_| Error::Allocation)?;
            atlas
                .rgb
                .try_reserve_exact(self.xy.len())
                .map_err(|_| Error::Allocation)?;
            let input = Input {
                lens: &self.lenses[j],
                pixels: source,
            };
            for point in &self.xy {
                let p = point
                    .map(|p| sample(&input, p[j][0], p[j][1]))
                    .unwrap_or([0.; 4]);
                let gray = if p[3] >= 0.99 && p.iter().all(|v| v.is_finite()) {
                    (0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2])
                        .max(0.)
                        .sqrt()
                } else {
                    f32::NAN
                };
                atlas.rgb.push(p);
                atlas.gray.push(gray);
            }
            atlases.push(atlas);
        }
        // Fixed density and bounded searches; longitude wraps, vertical edges do not.
        let points: Vec<_> = (16..HEIGHT - 16)
            .step_by(8)
            .flat_map(|y| (0..WIDTH).step_by(8).map(move |x| (x, y)))
            .collect();
        let matches = points
            .par_iter()
            .filter_map(|&(x, y)| correspond(&atlases[0], &atlases[1], x as i32, y as i32))
            .collect();
        Ok(Alignment {
            width: WIDTH,
            height: HEIGHT,
            vertical_span_degrees: 24.,
            candidates: points.len(),
            matches,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn atlas(shift: [i32; 2], gain: f32) -> Atlas {
        let (width, height) = (64, 48);
        let mut out = Atlas {
            width,
            height,
            gray: Vec::new(),
            rgb: Vec::new(),
        };
        for y in 0..height as i32 {
            for x in 0..width as i32 {
                let xx = (x - shift[0]).rem_euclid(width as i32) as u32;
                let yy = (y - shift[1]).rem_euclid(height as i32) as u32;
                let mut hash = xx.wrapping_mul(1664525) ^ yy.wrapping_mul(1013904223);
                hash ^= hash >> 16;
                hash = hash.wrapping_mul(0x7feb352d);
                hash ^= hash >> 15;
                hash = hash.wrapping_mul(0x846ca68b);
                hash ^= hash >> 16;
                let v = 0.1 + (hash >> 24) as f32 / 1024.;
                out.gray.push(v * gain);
                out.rgb.push([v * gain, v * gain, v * gain, 1.]);
            }
        }
        out
    }
    #[test]
    fn exposure_invariant_translation_wrap_and_invalid_regions() -> Result<()> {
        let a = atlas([0, 0], 1.);
        let mut b = atlas([3, -2], 1.3);
        let m = correspond(&a, &b, 62, 24).ok_or(Error::Invalid("textured translated patch"))?;
        assert_eq!(m.second, [1., 22.]);
        assert!(m.correlation > 0.99999);
        assert_eq!(m.cycle_error_pixels, 0.);
        b.rgb.fill([0.; 4]);
        let geometry =
            correspond(&a, &b, 62, 24).ok_or(Error::Invalid("geometry without usable colour"))?;
        assert!(geometry.second_rgb.is_none());
        b.gray.fill(f32::NAN);
        assert!(correspond(&a, &b, 62, 24).is_none());
        b.gray.fill(0.2);
        assert!(correspond(&a, &b, 62, 24).is_none());
        let outside = atlas([8, 0], 1.);
        assert!(correspond(&a, &outside, 24, 24).is_none());
        assert!(correspond(&a, &a, 24, 1).is_none());
        Ok(())
    }
    #[test]
    fn repeated_texture_is_ambiguous() {
        let mut a = atlas([0, 0], 1.);
        for y in 0..a.height {
            for x in 0..a.width {
                a.gray[y * a.width + x] = if x % 4 < 2 { 0.2 } else { 0.4 };
            }
        }
        assert!(correspond(&a, &a, 32, 24).is_none());
    }
    #[test]
    fn subpixel_peak_reduces_fractional_translation_error() -> Result<()> {
        let mut a = atlas([0, 0], 1.);
        let mut b = atlas([0, 0], 1.);
        let field = |x: f64, y: f64| {
            0.2 + 0.05 * (0.4 * x + 0.13 * y).sin()
                + 0.05 * (0.17 * x - 0.37 * y).cos()
                + 0.025 * (0.9 * x).sin() * (0.7 * y).cos()
        };
        for y in 0..a.height {
            for x in 0..a.width {
                a.gray[y * a.width + x] = field(x as f64, y as f64) as f32;
                b.gray[y * a.width + x] = (1.2 * field(x as f64 - 2.4, y as f64 + 1.3)) as f32;
            }
        }
        let m = correspond(&a, &b, 32, 24).ok_or(Error::Invalid("fractional translated patch"))?;
        assert!((m.second[0] - 34.4).abs() < 0.25, "{m:?}");
        assert!((m.second[1] - 22.7).abs() < 0.25, "{m:?}");
        Ok(())
    }
}

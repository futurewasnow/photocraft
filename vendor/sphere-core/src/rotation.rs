//! Validated unit quaternions. Matrices map column vectors; composition applies RHS first.
use crate::{Error, Result};

#[derive(Clone, Copy, Debug)]
pub struct Rotation([f64; 4]);
impl Rotation {
    pub const IDENTITY: Self = Self([1., 0., 0., 0.]);
    pub fn from_wxyz(q: [f64; 4]) -> Result<Self> {
        if q.iter().any(|v| !v.is_finite() || v.abs() > 1e100) {
            return Err(Error::Invalid("invalid quaternion"));
        }
        let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
        if n < 1e-12 {
            return Err(Error::Invalid("zero quaternion"));
        }
        Ok(Self(q.map(|v| v / n)))
    }
    pub fn wxyz(self) -> [f64; 4] {
        self.0
    }
    pub fn inverse(self) -> Self {
        let [w, x, y, z] = self.0;
        Self([w, -x, -y, -z])
    }
    pub fn compose(self, rhs: Self) -> Self {
        let [a, b, c, d] = self.0;
        let [e, f, g, h] = rhs.0;
        let q = [
            a * e - b * f - c * g - d * h,
            a * f + b * e + c * h - d * g,
            a * g - b * h + c * e + d * f,
            a * h + b * g - c * f + d * e,
        ];
        // Product of two validated unit quaternions: norm is bounded away from zero.
        let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
        Self(q.map(|v| v / n))
    }
    pub fn from_vector(v: [f64; 3]) -> Result<Self> {
        if v.iter().any(|v| !v.is_finite() || v.abs() > 1e6) {
            return Err(Error::Invalid("invalid rotation vector"));
        }
        let angle = dot(v, v).sqrt();
        if angle < 1e-10 {
            return Self::from_wxyz([1., v[0] * 0.5, v[1] * 0.5, v[2] * 0.5]);
        }
        let (s, c) = (angle * 0.5).sin_cos();
        Self::from_wxyz([c, v[0] * s / angle, v[1] * s / angle, v[2] * s / angle])
    }
    pub fn between(a: [f64; 3], b: [f64; 3]) -> Result<Self> {
        let a = unit(a)?;
        let b = unit(b)?;
        let d = dot(a, b).clamp(-1., 1.);
        if d < -0.999999999 {
            let basis = if a[0].abs() < a[1].abs() && a[0].abs() < a[2].abs() {
                [1., 0., 0.]
            } else if a[1].abs() < a[2].abs() {
                [0., 1., 0.]
            } else {
                [0., 0., 1.]
            };
            let v = unit(cross(a, basis))?;
            return Self::from_wxyz([0., v[0], v[1], v[2]]);
        }
        let v = cross(a, b);
        Self::from_wxyz([1. + d, v[0], v[1], v[2]])
    }
    pub fn interpolate(self, other: Self, t: f64) -> Result<Self> {
        if !t.is_finite() || !(0.0..=1.0).contains(&t) {
            return Err(Error::Invalid("rotation interpolation must be 0..1"));
        }
        let a = self.0;
        let mut b = other.0;
        let mut d: f64 = (0..4).map(|i| a[i] * b[i]).sum();
        if d < 0. {
            b = b.map(|v| -v);
            d = -d;
        }
        if d > 0.9995 {
            return Self::from_wxyz(std::array::from_fn(|i| a[i] * (1. - t) + b[i] * t));
        }
        let angle = d.clamp(0., 1.).acos();
        let inv = 1. / angle.sin();
        let wa = ((1. - t) * angle).sin() * inv;
        let wb = (t * angle).sin() * inv;
        Self::from_wxyz(std::array::from_fn(|i| a[i] * wa + b[i] * wb))
    }
    pub fn matrix(self) -> [[f64; 3]; 3] {
        let [w, x, y, z] = self.0;
        [
            [
                1. - 2. * (y * y + z * z),
                2. * (x * y - z * w),
                2. * (x * z + y * w),
            ],
            [
                2. * (x * y + z * w),
                1. - 2. * (x * x + z * z),
                2. * (y * z - x * w),
            ],
            [
                2. * (x * z - y * w),
                2. * (y * z + x * w),
                1. - 2. * (x * x + y * y),
            ],
        ]
    }
    pub fn apply(self, v: [f64; 3]) -> [f64; 3] {
        crate::rotate(self.matrix(), v)
    }
}
pub fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|i| a[i] * b[i]).sum()
}
pub fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
pub fn unit(v: [f64; 3]) -> Result<[f64; 3]> {
    if v.iter().any(|v| !v.is_finite() || v.abs() > 1e100) {
        return Err(Error::Invalid("invalid direction"));
    }
    let n = dot(v, v).sqrt();
    if n < 1e-12 {
        return Err(Error::Invalid("zero direction"));
    }
    Ok(v.map(|v| v / n))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn composition_interpolation_and_antipodes() -> Result<()> {
        let q = Rotation::from_vector([0., std::f64::consts::FRAC_PI_2, 0.])?;
        let x = q.apply([0., 0., 1.]);
        assert!((x[0] - 1.).abs() < 1e-12 && x[2].abs() < 1e-12);
        let unchanged = q.compose(q.inverse()).apply([0.2, 0.3, 0.4]);
        for (a, b) in unchanged.into_iter().zip([0.2, 0.3, 0.4]) {
            assert!((a - b).abs() < 1e-12);
        }
        let half = Rotation::IDENTITY.interpolate(q, 0.5)?.apply([0., 0., 1.]);
        assert!((half[0] - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-12);
        let anti = Rotation::between([0., 1., 0.], [0., -1., 0.])?.apply([0., 1., 0.]);
        assert!((anti[1] + 1.).abs() < 1e-12);
        assert!(Rotation::from_wxyz([0.; 4]).is_err());
        assert!(Rotation::from_vector([f64::NAN, 0., 0.]).is_err());
        assert!(q.interpolate(q, 2.).is_err());
        Ok(())
    }
}

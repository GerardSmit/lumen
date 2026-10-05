//! Allocation-free pointer orientation conversion, independent of event hosts.

use core::f64::consts::{FRAC_PI_2, PI, TAU};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Orientation {
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub altitude: f64,
    pub azimuth: f64,
}

impl Orientation {
    /// Complete either representation without replacing explicitly supplied
    /// values. A partially supplied pair uses the other member's default.
    pub fn from_components(
        x: Option<i32>,
        y: Option<i32>,
        altitude: Option<f64>,
        azimuth: Option<f64>,
    ) -> Self {
        let has_tilt = x.is_some() || y.is_some();
        let has_angles = altitude.is_some() || azimuth.is_some();
        let mut result = Self {
            tilt_x: x.unwrap_or(0),
            tilt_y: y.unwrap_or(0),
            altitude: altitude.unwrap_or(FRAC_PI_2),
            azimuth: azimuth.unwrap_or(0.0),
        };
        if has_tilt && !has_angles {
            let x = result.tilt_x;
            let y = result.tilt_y;
            let xr = f64::from(x).to_radians();
            let yr = f64::from(y).to_radians();
            result.azimuth = if x == 0 {
                if y > 0 {
                    FRAC_PI_2
                } else if y < 0 {
                    3.0 * FRAC_PI_2
                } else {
                    0.0
                }
            } else if y == 0 {
                if x < 0 { PI } else { 0.0 }
            } else if x == 90 || x == -90 || y == 90 || y == -90 {
                0.0
            } else {
                let angle = yr.tan().atan2(xr.tan());
                if angle < 0.0 { angle + TAU } else { angle }
            };
            result.altitude = if x == 90 || x == -90 || y == 90 || y == -90 {
                0.0
            } else if x == 0 {
                FRAC_PI_2 - yr.abs()
            } else if y == 0 {
                FRAC_PI_2 - xr.abs()
            } else {
                (1.0 / (xr.tan().powi(2) + yr.tan().powi(2)).sqrt()).atan()
            };
        } else if has_angles && !has_tilt {
            let a = result.azimuth;
            let (x, y) = if result.altitude == 0.0 {
                if a == 0.0 || a == TAU {
                    (90.0, 0.0)
                } else if a == FRAC_PI_2 {
                    (0.0, 90.0)
                } else if a == PI {
                    (-90.0, 0.0)
                } else if a == 3.0 * FRAC_PI_2 {
                    (0.0, -90.0)
                } else if a > 0.0 && a < FRAC_PI_2 {
                    (90.0, 90.0)
                } else if a > FRAC_PI_2 && a < PI {
                    (-90.0, 90.0)
                } else if a > PI && a < 3.0 * FRAC_PI_2 {
                    (-90.0, -90.0)
                } else if a > 3.0 * FRAC_PI_2 && a < TAU {
                    (90.0, -90.0)
                } else {
                    (0.0, 0.0)
                }
            } else {
                let tangent = result.altitude.tan();
                (
                    (a.cos() / tangent).atan().to_degrees(),
                    (a.sin() / tangent).atan().to_degrees(),
                )
            };
            result.tilt_x = (x + 0.5).floor() as i32;
            result.tilt_y = (y + 0.5).floor() as i32;
        }
        result
    }
}

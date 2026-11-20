//! How the pixels of a screenshot or a recording line up with the points of
//! a reading. A recording is only evidence when it shows the display that
//! was read; a phone with two displays can hand over a picture of the other
//! one.

use crate::reply::{Fail, Res};

/// Largest difference allowed between the horizontal and the vertical
/// pixels-per-point ratio.
const TOLERANCE: f64 = 0.01;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Geometry {
    pub width: u32,
    pub height: u32,
    /// Pixels per point.
    pub scale: f64,
}

impl Geometry {
    /// `points`: the screen size a reading reports. `pixels`: the image's.
    /// Fails with `display_mismatch` when the two are not the same shape.
    pub fn fit(points: (f64, f64), pixels: (u32, u32)) -> Res<Self> {
        let (pw, ph) = points;
        let (w, h) = pixels;
        let mismatch = || {
            Fail::not_sent(
                "display_mismatch",
                format!("the image is {w}x{h} pixels but the screen reads {pw}x{ph} points"),
            )
            .hint(crate::i18n::t(
                "화면과 다른 디스플레이를 받았습니다. 폰을 펼치거나 접은 직후라면 잠시 뒤 다시 시도하고, 녹화는 `--display`로 다른 디스플레이를 고르세요.",
                "The picture is of another display. Right after folding or unfolding the phone try again in a moment; for a recording pick another with `--display`.",
            ))
        };
        if pw <= 0.0 || ph <= 0.0 || w == 0 || h == 0 {
            return Err(mismatch());
        }
        let sx = w as f64 / pw;
        let sy = h as f64 / ph;
        if (sx - sy).abs() > TOLERANCE * sx.max(sy) {
            return Err(mismatch());
        }
        Ok(Self { width: w, height: h, scale: (sx + sy) / 2.0 })
    }

    /// A length or coordinate in points, in whole pixels.
    pub fn px(&self, points: f64) -> i64 {
        (points * self.scale).round() as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_shape_gives_one_scale() {
        let g = Geometry::fit((402.0, 874.0), (1206, 2622)).unwrap();
        assert_eq!((g.width, g.height), (1206, 2622));
        assert!((g.scale - 3.0).abs() < 1e-9);
        assert_eq!(g.px(10.5), 32);
    }

    #[test]
    fn ratios_that_differ_by_more_than_one_percent_are_another_display() {
        // A portrait reading against a landscape picture.
        let err = Geometry::fit((402.0, 874.0), (2622, 1206)).unwrap_err();
        assert_eq!(err.code, "display_mismatch");
        assert_eq!(err.outcome.as_str(), "not_sent");
        // Off by 0.9% still fits; off by 1.5% does not.
        assert!(Geometry::fit((400.0, 800.0), (1200, 2422)).is_ok());
        assert!(Geometry::fit((400.0, 800.0), (1200, 2436)).is_err());
        assert!(Geometry::fit((0.0, 800.0), (1200, 2400)).is_err());
        assert!(Geometry::fit((400.0, 800.0), (0, 0)).is_err());
    }
}

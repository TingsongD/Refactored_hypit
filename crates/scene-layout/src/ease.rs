//! Easing curves for the named entrance animations. Deterministic pure
//! math — same t in, same curve out, on every worker.

/// Decelerating approach: fast start, settles into place.
pub fn ease_out_cubic(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// Gentle ramp in both directions — the fade curve.
pub fn ease_in_out(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Overshoot-and-settle: pops past 1.0 then back. Standard `c1 = 1.70158`.
/// Endpoints are exact — callers rely on `ease(1)` being precisely the
/// settled value.
pub fn ease_out_back(t: f64) -> f64 {
    const C1: f64 = 1.70158;
    const C3: f64 = C1 + 1.0;
    let t = t.clamp(0.0, 1.0);
    if t == 0.0 || t == 1.0 {
        return t;
    }
    1.0 + C3 * (t - 1.0).powi(3) + C1 * (t - 1.0).powi(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        for f in [ease_out_cubic, ease_in_out] {
            assert_eq!(f(0.0), 0.0);
            assert_eq!(f(1.0), 1.0);
        }
        assert_eq!(ease_out_back(0.0), 0.0);
        assert!((ease_out_back(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn back_overshoots() {
        // The signature of an ease-out-back: somewhere mid-curve it
        // passes 1.0 before settling back.
        assert!((6..9).any(|i| ease_out_back(f64::from(i) / 10.0) > 1.0));
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(ease_out_cubic(-1.0), 0.0);
        assert_eq!(ease_out_cubic(2.0), 1.0);
    }
}

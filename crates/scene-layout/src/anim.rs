//! Named entrance animations: `anim="rise|fade|pop"`.
//!
//! Every anim is an *entrance* — it plays over the first `entrance`
//! frames of the element's resolved span and then holds at identity for
//! the rest. Progress is `local_frame / entrance`, clamped.

use scene_ir::AnimKind;

use crate::ease;
use crate::geom::Transform;

/// An animation never takes more than this many frames (~0.5s at 30fps).
const ENTRANCE_CAP_FRAMES: u32 = 15;
/// Pixels a `rise` travels while coming in.
const RISE_TRAVEL_PX: f64 = 64.0;
/// `pop` starts at this scale.
const POP_FROM_SCALE: f64 = 0.7;

/// Frames the entrance occupies: the shorter of the cap and the
/// element's own span (a 5-frame clip still gets a whole entrance).
fn entrance_frames(span_len: u32) -> u32 {
    span_len.clamp(1, ENTRANCE_CAP_FRAMES)
}

/// Evaluate `anim` at `local_frame` within a span of `len` frames.
/// Returns `(opacity, transform)` — opacity multiplies the element's.
pub fn evaluate(kind: AnimKind, local_frame: u32, len: u32) -> (f64, Transform) {
    let entrance = entrance_frames(len);
    let t = (local_frame as f64 / f64::from(entrance)).clamp(0.0, 1.0);
    match kind {
        AnimKind::Fade => (ease::ease_in_out(t), Transform::IDENTITY),
        AnimKind::Rise => {
            let ease = ease::ease_out_cubic(t);
            (
                // The fade leads the travel so the first frame isn't
                // invisible-but-already-half-risen.
                (t * 1.6).min(1.0),
                Transform {
                    dx: 0.0,
                    dy: (1.0 - ease) * RISE_TRAVEL_PX,
                    scale: 1.0,
                },
            )
        }
        AnimKind::Pop => (
            (t * 2.0).min(1.0),
            Transform {
                dx: 0.0,
                dy: 0.0,
                scale: POP_FROM_SCALE + (1.0 - POP_FROM_SCALE) * ease::ease_out_back(t),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entrance_is_capped() {
        assert_eq!(entrance_frames(90), 15);
        assert_eq!(entrance_frames(3), 3);
        assert_eq!(entrance_frames(0), 1); // degenerate span still safe
    }

    #[test]
    fn ends_at_identity() {
        for kind in [AnimKind::Rise, AnimKind::Fade, AnimKind::Pop] {
            let (opacity, transform) = evaluate(kind, 15, 90);
            assert_eq!(opacity, 1.0);
            assert!(transform.is_identity(), "{kind:?} not settled");
        }
        // and stays there for the rest of the span
        let (_, transform) = evaluate(AnimKind::Rise, 89, 90);
        assert!(transform.is_identity());
    }

    #[test]
    fn rise_comes_from_below() {
        let (opacity, transform) = evaluate(AnimKind::Rise, 0, 90);
        assert_eq!(opacity, 0.0);
        assert_eq!(transform.dy, RISE_TRAVEL_PX);
        let (_, mid) = evaluate(AnimKind::Rise, 7, 90);
        assert!(mid.dy > 0.0 && mid.dy < RISE_TRAVEL_PX);
    }

    #[test]
    fn fade_only_fades() {
        let (opacity, transform) = evaluate(AnimKind::Fade, 0, 90);
        assert_eq!(opacity, 0.0);
        assert!(transform.is_identity());
        let (mid, _) = evaluate(AnimKind::Fade, 7, 90);
        assert!(mid > 0.0 && mid < 1.0);
    }

    #[test]
    fn pop_scales_up() {
        let (opacity, transform) = evaluate(AnimKind::Pop, 0, 90);
        assert_eq!(opacity, 0.0);
        assert_eq!(transform.scale, POP_FROM_SCALE);
    }
}

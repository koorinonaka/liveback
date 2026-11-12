//! `Motion.easing-enter` is `cubic-bezier(0, 0, 0, 1)` (`ui/tokens.slint`):
//! x(s) = s^3, y(s) = 3s^2 - 2s^3. With both x control points at 0, slint's
//! Newton solve for s overshoots far past 1 on a small x and, out of
//! iterations, returned s clamped to 1 -- y = 1, the end value -- on the
//! first frame of every entrance, then the true value on the next
//! (t261010-4367: each history tile flashed in at full opacity and dropped
//! back). Fixed in the vendored `vendor/i-slint-core/animations.rs` (search
//! "Vendored fix").

use i_slint_core::animations::{easing_curve, EasingCurve};

fn exact(x: f32) -> f32 {
    let s = x.cbrt();
    3.0 * s * s - 2.0 * s * s * s
}

#[test]
fn easing_enter_follows_its_curve_from_the_first_frame() {
    let curve = EasingCurve::CubicBezier([0.0, 0.0, 0.0, 1.0]);
    let mut last = 0.0f32;
    for i in 0..=1000 {
        let x = i as f32 / 1000.0;
        let y = easing_curve(&curve, x);
        assert!(
            (y - exact(x)).abs() < 0.02,
            "x={x}: y={y}, curve says {}",
            exact(x)
        );
        assert!(y + 1e-4 >= last, "x={x}: y={y} fell below {last}");
        last = y;
    }
}

#[test]
fn a_curve_newton_handles_is_unchanged() {
    // `easing-reveal` (0.2, 0.75, 0.25, 1): dx/ds = 0.6 at 0, so Newton
    // converges -- the control that the fix only takes over where it failed.
    let curve = EasingCurve::CubicBezier([0.2, 0.75, 0.25, 1.0]);
    for (x, want) in [(0.0156f32, 0.0586f32), (0.1, 0.3633), (0.5, 0.9295)] {
        let y = easing_curve(&curve, x);
        assert!((y - want).abs() < 0.01, "x={x}: y={y}, want {want}");
    }
}

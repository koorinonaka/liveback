//! A hold button's `hold-complete` waits for the check (`HoldGesture` in
//! `ui/controls.slint`). Every caller removes the button's host on it -- the
//! menu closes, the marker row goes, the selection empties -- so firing it as
//! the fill filled left the confirmation check unseen.
//!
//! One `#[test]`: the testing backend is a process-wide platform, set once.

use std::time::Duration;

slint::slint! {
    import { Btn } from "ui/controls.slint";

    export component Harness inherits Window {
        width: 200px;
        height: 80px;
        out property <int> completions;
        out property <int> phase: b.hold-phase;
        public function press() {
            b.hold-start();
        }
        public function release() {
            b.hold-cancel();
        }
        b := Btn {
            label: "x";
            hold: true;
            hold-complete => {
                root.completions += 1;
            }
        }
    }
}

/// Advances the mocked clock in frame-sized steps, so each Timer starts on
/// the step its `running` turned true.
fn advance(ms: u64) {
    for _ in 0..ms / 10 {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(10));
    }
}

#[test]
fn hold_complete_fires_once_after_the_check() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = Harness::new().unwrap();

    ui.invoke_press();
    advance(1100);
    assert_eq!(ui.get_phase(), 2, "the fill is full");
    assert_eq!(ui.get_completions(), 0, "not before the check has drawn");

    // Letting go once the fill is full does not take the action back.
    ui.invoke_release();
    advance(400);
    assert_eq!(ui.get_completions(), 1);

    advance(1500);
    assert_eq!(ui.get_completions(), 1, "once, not every 450ms of `done`");
    assert_eq!(ui.get_phase(), 0);
}

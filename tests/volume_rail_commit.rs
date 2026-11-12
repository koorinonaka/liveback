//! `VolumeRail` (`ui/stage_parts.slint`) tells its caller about a gesture twice:
//! `ratio-changed` on every step, so the knob follows the pointer, and
//! `ratio-committed` once at the end, which is where the review screen saves
//! the level to settings.json (t261025-ea25). Saving on `ratio-changed` read
//! and rewrote the file on every pointer move of a drag.
//!
//! One `#[test]`: the testing backend is a process-wide platform, set once.

use slint::platform::{PointerEventButton, WindowEvent};
use slint::LogicalPosition;

slint::slint! {
    import { VolumeRail } from "ui/stage_parts.slint";

    export component Harness inherits Window {
        width: 200px;
        height: 40px;
        in-out property <float> r: 0.5;
        out property <int> changes;
        out property <int> commits;
        out property <float> committed: -1;
        VolumeRail {
            x: 0;
            y: 0;
            width: 200px;
            height: 40px;
            ratio: root.r;
            ratio-changed(v) => {
                root.r = v;
                root.changes += 1;
            }
            ratio-committed(v) => {
                root.committed = v;
                root.commits += 1;
            }
        }
    }
}

fn pointer(ui: &Harness, event: WindowEvent) {
    ui.window().dispatch_event(event);
}

#[test]
fn a_drag_commits_once_on_release() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = Harness::new().unwrap();
    ui.show().unwrap();
    let at = |x: f32| LogicalPosition::new(x, 20.0);
    let button = PointerEventButton::Left;

    pointer(
        &ui,
        WindowEvent::PointerMoved {
            position: at(100.0),
        },
    );
    pointer(
        &ui,
        WindowEvent::PointerPressed {
            position: at(100.0),
            button,
        },
    );
    const MOVES: i32 = 50;
    for step in 0..MOVES {
        pointer(
            &ui,
            WindowEvent::PointerMoved {
                position: at(101.0 + step as f32),
            },
        );
    }
    assert_eq!(
        ui.get_changes(),
        MOVES + 1,
        "down and every move follow the pointer"
    );
    assert_eq!(
        ui.get_commits(),
        0,
        "nothing is committed while the button is held"
    );

    pointer(
        &ui,
        WindowEvent::PointerReleased {
            position: at(150.0),
            button,
        },
    );
    assert_eq!(
        ui.get_changes(),
        MOVES + 1,
        "the release itself is not a step"
    );
    assert_eq!(ui.get_commits(), 1);
    assert!(
        (ui.get_committed() - ui.get_r()).abs() < 1e-6,
        "commits where the drag left it"
    );

    // One wheel notch is a whole gesture: one step and its commit.
    pointer(
        &ui,
        WindowEvent::PointerScrolled {
            position: at(150.0),
            delta_x: 0.0,
            delta_y: 10.0,
        },
    );
    assert_eq!(ui.get_changes(), MOVES + 2);
    assert_eq!(ui.get_commits(), 2);
}

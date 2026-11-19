//! A selected line break draws a sliver at the end of its line (2026-10-11,
//! the user), so an empty line inside a selection is not blank. slint lays out
//! each `\n`-separated paragraph on its own and the `\n` itself never reached
//! the selection geometry; the fix is in the vendored `i-slint-core`
//! (`textlayout/sharedparley/selection.rs`, search "Vendored fix").
//!
//! One `#[test]`: the testing backend is a process-wide platform, set once.

use slint::ComponentHandle;

slint::slint! {
    export component Harness inherits Window {
        width: 200px;
        height: 100px;
        background: white;
        public function select(a: int, c: int) {
            input.focus();
            input.set-selection-offsets(a, c);
        }
        input := TextInput {
            x: 0;
            y: 0;
            width: 200px;
            height: 100px;
            text: "a\n\nb";
            single-line: false;
            font-size: 20px;
            color: black;
            text-cursor-width: 0;
            selection-background-color: #ff0000;
            selection-foreground-color: black;
        }
    }
}

fn red_in_band(h: &Harness, y0: u32, y1: u32) -> bool {
    let shot = h.window().take_snapshot().unwrap();
    let w = shot.width();
    let px = shot.as_slice();
    (y0..y1).any(|y| {
        (0..20).any(|x| {
            let p = px[(y * w + x) as usize];
            p.r > 200 && p.g < 60 && p.b < 60
        })
    })
}

#[test]
fn selected_empty_line_shows_a_sliver() {
    slint::platform::set_platform(Box::new(i_slint_backend_testing::TestingBackend::new(
        i_slint_backend_testing::TestingBackendOptions {
            mock_time: true,
            renderer_name: Some("software".into()),
            ..Default::default()
        },
    )))
    .unwrap();
    let h = Harness::new().unwrap();
    h.show().unwrap();

    // Line 2 (the empty one) spans roughly y 25..45 at 20px.
    h.invoke_select(0, 0);
    assert!(!red_in_band(&h, 28, 42), "nothing selected yet");
    h.invoke_select(0, 5);
    assert!(
        red_in_band(&h, 28, 42),
        "the empty line's break is selected but drew nothing"
    );
}

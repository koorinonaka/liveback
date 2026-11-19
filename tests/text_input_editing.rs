//! How slint's `TextInput` edits, measured against the Windows standard edit
//! (t261010-a15f). Every text field in `ui/controls.slint` is a bare
//! `TextInput` that only handles Esc / Ctrl+Return itself, so the editing keys
//! below are slint's own behaviour -- which lives in the vendored
//! `vendor/i-slint-core` (search "Vendored fix"). The reference numbers come
//! from RichEdit, the control Win11 Notepad is built on, measured by
//! `.agents/tasks/evidence/t261010-a15f-text-editing/probe-reference-editors.ps1`.
//!
//! One `#[test]`: the testing backend is a process-wide platform, set once.

use i_slint_core::input::{InternalKeyEvent, KeyEventType};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString};

slint::slint! {
    export component Harness inherits Window {
        width: 600px;
        height: 200px;
        in-out property <string> text <=> input.text;
        in property <bool> single: false;
        out property <int> cursor: input.cursor-position-byte-offset;
        out property <int> anchor: input.anchor-position-byte-offset;
        out property <string> preedit: input.preedit-text;
        out property <length> caret-x;
        out property <length> caret-y;
        public function select(a: int, c: int) {
            input.focus();
            input.set-selection-offsets(a, c);
        }
        input := TextInput {
            x: 0;
            y: 0;
            width: 600px;
            height: root.single ? 32px : 120px;
            single-line: root.single;
            vertical-alignment: root.single ? center : top;
            font-size: 13px;
            cursor-position-changed(p) => {
                root.caret-x = p.x;
                root.caret-y = p.y;
            }
        }
    }
}

const MULTI: &str = "alpha beta gamma\ndelta epsilon\nzeta eta";
const SINGLE: &str = "alpha beta gamma";

fn key(ui: &Harness, k: impl Into<SharedString> + Clone) {
    let text: SharedString = k.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

/// `mods` held around `k` (Control / Shift), the way a keyboard sends them.
fn chord(ui: &Harness, mods: &[Key], k: impl Into<SharedString> + Clone) {
    for m in mods {
        ui.window()
            .dispatch_event(WindowEvent::KeyPressed { text: (*m).into() });
    }
    key(ui, k);
    for m in mods.iter().rev() {
        ui.window()
            .dispatch_event(WindowEvent::KeyReleased { text: (*m).into() });
    }
}

fn ime(ui: &Harness, event_type: KeyEventType, s: &str) {
    let mut ev = InternalKeyEvent {
        event_type,
        ..Default::default()
    };
    if event_type == KeyEventType::UpdateComposition {
        ev.preedit_text = s.into();
    } else {
        ev.key_event.text = s.into();
    }
    ui.window().dispatch_event(WindowEvent::internal(ev));
}

/// (line, col) of a byte offset, ASCII text.
fn line_col(text: &str, off: usize) -> (usize, usize) {
    let before = &text[..off];
    let line = before.matches('\n').count();
    (line, off - before.rfind('\n').map_or(0, |i| i + 1))
}

fn offset(text: &str, line: usize, col: usize) -> i32 {
    (text
        .split('\n')
        .take(line)
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + col) as i32
}

type Edit<'a> = (
    &'a str,
    (usize, usize),
    usize,
    &'a [Key],
    Key,
    (usize, usize),
    &'a str,
);

struct Case {
    name: &'static str,
    at: (usize, usize),
    sel: usize,
    mods: &'static [Key],
    key: Key,
    /// Expected caret (line, col), selection length, and resulting text when changed.
    want: ((usize, usize), usize, Option<&'static str>),
}

fn run(ui: &Harness, text: &str, c: &Case) -> String {
    ui.set_text(text.into());
    let a = offset(text, c.at.0, c.at.1);
    ui.invoke_select(a, a + c.sel as i32);
    chord(ui, c.mods, c.key);
    let t = ui.get_text().to_string();
    let cur = ui.get_cursor() as usize;
    let anc = ui.get_anchor() as usize;
    let got = (
        line_col(&t, cur),
        cur.abs_diff(anc),
        (t != text).then_some(t.as_str()),
    );
    let (wl, ws, wt) = c.want;
    let ok = got.0 == wl && got.1 == ws && got.2 == wt;
    format!(
        "{} {:<22} caret=L{}:{} sel={}{}",
        if ok { "ok  " } else { "DIFF" },
        c.name,
        got.0 .0,
        got.0 .1,
        got.1,
        got.2
            .map(|t| format!(" text={}", t.replace('\n', "\\n")))
            .unwrap_or_default()
    )
}

fn click(ui: &Harness, x: f32, y: f32, count: usize) {
    let position = LogicalPosition::new(x, y);
    ui.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    for _ in 0..count {
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }
}

fn selected(ui: &Harness) -> String {
    let t = ui.get_text();
    let (a, c) = (ui.get_anchor() as usize, ui.get_cursor() as usize);
    t[a.min(c)..a.max(c)].to_string()
}

#[test]
fn text_input_edits_like_the_windows_standard_edit() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = Harness::new().unwrap();
    ui.show().unwrap();
    use Key::*;
    let (ctl, sh): (&[Key], &[Key]) = (&[Control], &[Shift]);
    let mut lines = Vec::new();
    let mut diffs = 0;

    for single in [false, true] {
        ui.set_single(single);
        let (text, last) = if single { (SINGLE, 0) } else { (MULTI, 2) };
        let end = text.rsplit('\n').next().unwrap().len();
        let mut cases = vec![
            // The user's spec (2026-10-10), not RichEdit's: RichEdit leaves a
            // plain Up on the first line / Down on the last line where it is.
            Case {
                name: "up-first-line",
                at: (0, 7),
                sel: 0,
                mods: &[],
                key: UpArrow,
                want: ((0, 0), 0, None),
            },
            Case {
                name: "down-last-line",
                at: (last, 3),
                sel: 0,
                mods: &[],
                key: DownArrow,
                want: ((last, end), 0, None),
            },
            Case {
                name: "shift-up-first",
                at: (0, 7),
                sel: 0,
                mods: sh,
                key: UpArrow,
                want: ((0, 0), 7, None),
            },
            Case {
                name: "shift-down-last",
                at: (last, 3),
                sel: 0,
                mods: sh,
                key: DownArrow,
                want: ((last, end), end - 3, None),
            },
            Case {
                name: "home",
                at: (0, 8),
                sel: 0,
                mods: &[],
                key: Home,
                want: ((0, 0), 0, None),
            },
            Case {
                name: "end",
                at: (0, 8),
                sel: 0,
                mods: &[],
                key: End,
                want: ((0, 16), 0, None),
            },
            Case {
                name: "ctrl-home",
                at: (last, 3),
                sel: 0,
                mods: ctl,
                key: Home,
                want: ((0, 0), 0, None),
            },
            Case {
                name: "ctrl-end",
                at: (0, 3),
                sel: 0,
                mods: ctl,
                key: End,
                want: ((last, end), 0, None),
            },
            Case {
                name: "ctrl-right-midword",
                at: (0, 2),
                sel: 0,
                mods: ctl,
                key: RightArrow,
                want: ((0, 6), 0, None),
            },
            Case {
                name: "ctrl-right-wordend",
                at: (0, 5),
                sel: 0,
                mods: ctl,
                key: RightArrow,
                want: ((0, 6), 0, None),
            },
            Case {
                name: "ctrl-left-midword",
                at: (0, 8),
                sel: 0,
                mods: ctl,
                key: LeftArrow,
                want: ((0, 6), 0, None),
            },
            Case {
                name: "ctrl-left-wordstart",
                at: (0, 6),
                sel: 0,
                mods: ctl,
                key: LeftArrow,
                want: ((0, 0), 0, None),
            },
            Case {
                name: "shift-right-x1",
                at: (0, 2),
                sel: 0,
                mods: sh,
                key: RightArrow,
                want: ((0, 3), 1, None),
            },
            Case {
                name: "shift-left-x1",
                at: (0, 8),
                sel: 0,
                mods: sh,
                key: LeftArrow,
                want: ((0, 7), 1, None),
            },
        ];
        if !single {
            cases.push(Case {
                name: "down-middle-line",
                at: (1, 3),
                sel: 0,
                mods: &[],
                key: DownArrow,
                want: ((2, 3), 0, None),
            });
            cases.push(Case {
                name: "up-middle-line",
                at: (1, 3),
                sel: 0,
                mods: &[],
                key: UpArrow,
                want: ((0, 3), 0, None),
            });
        }
        // (name, caret, selection, modifiers, key, caret after, line 0 after)
        let edits: [Edit; 5] = [
            (
                "ctrl-bs-midword",
                (0, 8),
                0,
                ctl,
                Backspace,
                (0, 6),
                "alpha ta gamma",
            ),
            (
                "ctrl-del-midword",
                (0, 8),
                0,
                ctl,
                Delete,
                (0, 8),
                "alpha begamma",
            ),
            (
                "ctrl-del-wordend",
                (0, 5),
                0,
                ctl,
                Delete,
                (0, 5),
                "alphabeta gamma",
            ),
            (
                "bs-selection",
                (0, 6),
                4,
                &[],
                Backspace,
                (0, 6),
                "alpha  gamma",
            ),
            (
                "del-selection",
                (0, 6),
                4,
                &[],
                Delete,
                (0, 6),
                "alpha  gamma",
            ),
        ];
        let rest = if single {
            ""
        } else {
            "\ndelta epsilon\nzeta eta"
        };
        let edited: Vec<&'static str> = edits
            .iter()
            .map(|e| &*Box::leak(format!("{}{rest}", e.6).into_boxed_str()))
            .collect();
        for (e, t) in edits.iter().zip(edited) {
            cases.push(Case {
                name: e.0,
                at: e.1,
                sel: e.2,
                mods: e.3,
                key: e.4,
                want: (e.5, 0, Some(t)),
            });
        }

        let tag = if single { "single" } else { "multi " };
        for c in &cases {
            let l = run(&ui, text, c);
            diffs += l.starts_with("DIFF") as usize;
            lines.push(format!("{tag} {l}"));
        }

        // Ctrl+A
        ui.set_text(text.into());
        ui.invoke_select(3, 3);
        chord(&ui, ctl, "a");
        let ok = selected(&ui) == text;
        diffs += !ok as usize;
        lines.push(format!(
            "{tag} {} ctrl-a sel={}",
            if ok { "ok  " } else { "DIFF" },
            selected(&ui).len()
        ));

        // Typing over a selection.
        ui.set_text(text.into());
        ui.invoke_select(6, 10);
        key(&ui, "x");
        let ok = ui.get_text().starts_with("alpha x gamma");
        diffs += !ok as usize;
        lines.push(format!(
            "{tag} {} type-over-selection text={}",
            if ok { "ok  " } else { "DIFF" },
            ui.get_text().replace('\n', "\\n")
        ));

        // Double click inside "beta" selects "beta " (RichEdit and EDIT both
        // take the trailing space); a triple click the line, RichEdit with its
        // line break.
        ui.set_text(text.into());
        ui.invoke_select(8, 8);
        let (x, y) = (ui.get_caret_x() + 1.0, ui.get_caret_y() + 6.0);
        click(&ui, x, y, 2);
        let ok = selected(&ui) == "beta ";
        diffs += !ok as usize;
        lines.push(format!(
            "{tag} {} double-click-beta '{}'",
            if ok { "ok  " } else { "DIFF" },
            selected(&ui)
        ));
        ui.invoke_select(8, 8);
        // A fresh click sequence: move away so the next press does not count on.
        click(&ui, 590.0, 5.0, 0);
        slint_mock_wait();
        click(&ui, x, y, 3);
        let want = if single {
            "alpha beta gamma"
        } else {
            "alpha beta gamma\n"
        };
        let ok = selected(&ui) == want;
        diffs += !ok as usize;
        lines.push(format!(
            "{tag} {} triple-click-beta '{}'",
            if ok { "ok  " } else { "DIFF" },
            selected(&ui).replace('\n', "\\n")
        ));

        // IME: a conversion started over a selection replaces it at once.
        ui.set_text("開始".into());
        ui.invoke_select(0, "開始".len() as i32);
        ime(&ui, KeyEventType::UpdateComposition, "か");
        let during = ui.get_text().to_string();
        let preedit = ui.get_preedit().to_string();
        let ok_during = during.is_empty() && preedit == "か";
        ime(&ui, KeyEventType::UpdateComposition, "");
        ime(&ui, KeyEventType::CommitComposition, "か");
        let ok_after = ui.get_text() == "か";
        diffs += !(ok_during && ok_after) as usize;
        lines.push(format!(
            "{tag} {} ime-over-selection during='{during}' preedit='{preedit}' after='{}'",
            if ok_during && ok_after {
                "ok  "
            } else {
                "DIFF"
            },
            ui.get_text()
        ));

        // Control: a conversion with nothing selected leaves the text alone.
        ui.set_text("開始".into());
        ui.invoke_select(3, 3);
        ime(&ui, KeyEventType::UpdateComposition, "か");
        let ok = ui.get_text() == "開始";
        ime(&ui, KeyEventType::UpdateComposition, "");
        ime(&ui, KeyEventType::CommitComposition, "か");
        let ok = ok && ui.get_text() == "開か始";
        diffs += !ok as usize;
        lines.push(format!(
            "{tag} {} ime-no-selection after='{}'",
            if ok { "ok  " } else { "DIFF" },
            ui.get_text()
        ));
    }

    eprintln!("{}", lines.join("\n"));
    assert_eq!(
        diffs,
        0,
        "{} case(s) differ from the standard edit:\n{}",
        diffs,
        lines.join("\n")
    );
}

/// Lets slint's double-click window lapse so the next press starts a new count.
fn slint_mock_wait() {
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_secs(1));
}

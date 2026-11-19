use super::*;

/// The one thing the two screens' policies differ in, and the two modifiers
/// that must not see the difference: from `a` and `c` selected with the
/// anchor on `c`, a click on `b`.
#[test]
fn the_plain_click_policy_decides_only_the_unmodified_click() {
    let order: Vec<String> = ["a", "b", "c", "d"].map(str::to_owned).to_vec();
    let rows: [(PlainClick, bool, bool, ClickOutcome, &[&str]); 6] = [
        (
            PlainClick::Select,
            false,
            false,
            ClickOutcome::Selected,
            &["b"],
        ),
        (PlainClick::Ignore, false, false, ClickOutcome::Expand, &[]),
        (
            PlainClick::Select,
            true,
            false,
            ClickOutcome::Selected,
            &["b", "c"],
        ),
        (
            PlainClick::Ignore,
            true,
            false,
            ClickOutcome::Selected,
            &["b", "c"],
        ),
        (
            PlainClick::Select,
            false,
            true,
            ClickOutcome::Selected,
            &["a", "c", "b"],
        ),
        (
            PlainClick::Ignore,
            false,
            true,
            ClickOutcome::Selected,
            &["a", "c", "b"],
        ),
    ];
    for (plain, shift, ctrl, outcome, expected) in rows {
        let mut selection = Selection::new(plain);
        selection.click(&order[0], false, true, &order);
        selection.click(&order[2], false, true, &order);
        let got = selection.click(&order[1], shift, ctrl, &order);
        let picked: Vec<&str> = selection.selected().iter().map(String::as_str).collect();
        assert_eq!(
            (got, picked.as_slice()),
            (outcome, expected),
            "{plain:?} shift={shift} ctrl={ctrl}"
        );
    }
}

/// Ctrl+Shift with no anchor to range from: each screen keeps the answer its
/// own copy gave before t261025-67ce -- the history toggles, the clip grid
/// takes the row alone.
#[test]
fn ctrl_shift_without_an_anchor_keeps_each_screens_old_answer() {
    let order: Vec<String> = ["a", "b", "c"].map(str::to_owned).to_vec();
    for (plain, expected) in [
        (PlainClick::Select, ["a", "c", "b"].as_slice()),
        (PlainClick::Ignore, ["b"].as_slice()),
    ] {
        let mut selection = Selection::new(plain);
        selection.select_all(&[order[0].clone(), order[2].clone()]);
        selection.click(&order[1], true, true, &order);
        let picked: Vec<&str> = selection.selected().iter().map(String::as_str).collect();
        assert_eq!(picked, expected, "{plain:?}");
    }
}

//! The four operations a verification sweep cannot drive without the GUI, as a
//! one-line text command (task3750).
//!
//! Only the four. This is not an automation API: opening a `.lvb` in 確認,
//! setting the range, saving the range as a clip with a comment, and adding or
//! removing an auto-capture folder rule are the operations three sweeps in a
//! row named as the reason they could not reach the playback and gesture
//! tasks. Everything else a sweep needs it already does at the file layer with
//! the app stopped.
//!
//! The parsing lives here rather than beside the wiring because the wiring is
//! behind the `control` feature and `cargo test` runs the default one: the
//! rule from KNOWLEDGE.md「テストの配置と書き方の規約」is that the feature may
//! hide the plumbing, never the judgement. Nothing below knows about slint,
//! the drain, or the file the command travels in.
//!
//! # Grammar
//!
//! One command per line. The verb is the first whitespace-separated token
//! (`folder` takes a second one), and what follows is read by that verb alone:
//!
//! | 形式 | 引数 |
//! | --- | --- |
//! | `open <path>` | 行の残り全部。trim だけして verbatim（空白を含むパスがそのまま通る） |
//! | `range <開始秒> <終了秒>` | 空白区切りでちょうど2個。`123` / `12.5` の形だけ。開始 < 終了 |
//! | `clip <コメント>` | 行の残り全部を trim。空でもよい（GUI の 保存 と同じ） |
//! | `folder add <path>` | 行の残り全部。`open` と同じ規則 |
//! | `folder remove <path>` | 同上 |
//!
//! A path is never quoted and never escaped: it is the rest of the line. That
//! is the only rule that needs no shell-quoting convention on the sending
//! side, which is a PowerShell string inside another PowerShell string.

use std::fmt;

/// Seconds are carried as 100ns, the unit every timeline function speaks.
const HNS_PER_SECOND: i64 = 10_000_000;

/// One parsed command. The seconds are **offsets from the start of the loaded
/// session**, the same origin the panel's typed range edit uses -- a script has
/// no playhead and no wall clock to name an absolute instant with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    /// Open a `.lvb` on the review screen.
    Open(String),
    /// Set both range boundaries at once.
    Range { start_100ns: i64, end_100ns: i64 },
    /// Save the current range as a clip carrying this comment.
    Clip(String),
    /// Add an auto-capture folder rule.
    FolderAdd(String),
    /// Remove an auto-capture folder rule.
    FolderRemove(String),
}

impl ControlCommand {
    /// The `event = "…"` name the wiring logs this command's outcome under, so
    /// a sweep greps one fixed string per operation.
    pub fn event_name(&self) -> &'static str {
        match self {
            Self::Open(_) => "control_open",
            Self::Range { .. } => "control_range",
            Self::Clip(_) => "control_clip",
            Self::FolderAdd(_) => "control_folder_add",
            Self::FolderRemove(_) => "control_folder_remove",
        }
    }
}

/// Why a line is not a command. Every variant is a *refusal*, never a silent
/// drop: the sweep has to be able to tell "the app ignored me" from "the app
/// read me and said no".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlError {
    /// Nothing but whitespace.
    Empty,
    /// A first token that is not one of the four verbs.
    UnknownCommand(String),
    /// `folder` without `add` / `remove`.
    UnknownFolderAction(String),
    /// A verb whose argument is missing entirely.
    MissingArgument(&'static str),
    /// `range` with anything other than two tokens.
    WrongArgumentCount { expected: usize, found: usize },
    /// A seconds field that is not `123` or `12.5`.
    NotSeconds(String),
    /// `range` whose start is not strictly before its end.
    EmptyRange,
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "the control request is empty"),
            Self::UnknownCommand(name) => write!(
                f,
                "unknown command `{name}`; expected open, range, clip or folder"
            ),
            Self::UnknownFolderAction(name) => {
                write!(f, "unknown folder action `{name}`; expected add or remove")
            }
            Self::MissingArgument(what) => write!(f, "missing argument: {what}"),
            Self::WrongArgumentCount { expected, found } => {
                write!(f, "expected {expected} arguments, found {found}")
            }
            Self::NotSeconds(text) => write!(
                f,
                "`{text}` is not a count of seconds; write 12 or 12.5, with no sign and no exponent"
            ),
            Self::EmptyRange => write!(f, "the range start must be before its end"),
        }
    }
}

/// Splits `line` into its verb and the untouched rest.
fn split_verb(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    Some(match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    })
}

/// The whole grammar. Returns the command or the reason it is not one; never
/// panics and never guesses.
pub fn parse_control_command(line: &str) -> Result<ControlCommand, ControlError> {
    let Some((verb, rest)) = split_verb(line) else {
        return Err(ControlError::Empty);
    };
    match verb {
        // The rest of the line, verbatim: a path with spaces needs no quoting
        // because nothing after this point splits it again.
        "open" => require_argument(rest, "a path to a .lvb file").map(ControlCommand::Open),
        "range" => parse_range(rest),
        // Deliberately allowed to be empty: the GUI's own commit trims the
        // field and accepts what is left, so a control-side refusal would be a
        // rule the screen does not have.
        "clip" => Ok(ControlCommand::Clip(rest.to_owned())),
        "folder" => parse_folder(rest),
        other => Err(ControlError::UnknownCommand(other.to_owned())),
    }
}

fn require_argument(rest: &str, what: &'static str) -> Result<String, ControlError> {
    if rest.is_empty() {
        return Err(ControlError::MissingArgument(what));
    }
    Ok(rest.to_owned())
}

fn parse_folder(rest: &str) -> Result<ControlCommand, ControlError> {
    let Some((action, path)) = split_verb(rest) else {
        return Err(ControlError::MissingArgument("add or remove"));
    };
    match action {
        "add" => require_argument(path, "a folder path").map(ControlCommand::FolderAdd),
        "remove" => require_argument(path, "a folder path").map(ControlCommand::FolderRemove),
        other => Err(ControlError::UnknownFolderAction(other.to_owned())),
    }
}

fn parse_range(rest: &str) -> Result<ControlCommand, ControlError> {
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() != 2 {
        return Err(ControlError::WrongArgumentCount {
            expected: 2,
            found: fields.len(),
        });
    }
    let start_100ns = parse_seconds(fields[0])?;
    let end_100ns = parse_seconds(fields[1])?;
    // Equal is not a range: `clamp_range` would widen it back to a minimum
    // frame and the sweep would be looking at a boundary it did not ask for.
    if start_100ns >= end_100ns {
        return Err(ControlError::EmptyRange);
    }
    Ok(ControlCommand::Range {
        start_100ns,
        end_100ns,
    })
}

/// `123` or `12.5`, and nothing else.
///
/// Not `str::parse::<f64>`, for the reason `timeline::parse_range_time` gives
/// about its own seconds field: `parse` would take `1e2`, `inf`, `NaN` and a
/// leading `+`, and a control surface that silently accepted `inf` as a range
/// boundary would be worse than one that refused the line. Digits past the
/// seventh are below 100ns and are truncated rather than refused, the same way
/// the panel truncates past the tenth.
fn parse_seconds(text: &str) -> Result<i64, ControlError> {
    let not_seconds = || ControlError::NotSeconds(text.to_owned());
    let (whole, fraction) = match text.split_once('.') {
        None => (text, ""),
        Some((whole, fraction)) => (whole, fraction),
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(not_seconds());
    }
    if text.contains('.') && (fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(not_seconds());
    }
    let seconds: i64 = whole.parse().map_err(|_| not_seconds())?;
    let mut value = seconds
        .checked_mul(HNS_PER_SECOND)
        .ok_or_else(not_seconds)?;
    // Seven digits is exactly 100ns; anything the caller wrote past that is
    // finer than the unit and is dropped.
    let mut scale = HNS_PER_SECOND / 10;
    for digit in fraction.bytes().take(7) {
        // `checked_add` as well as the `checked_mul` above: a whole part just
        // under the ceiling passes the multiply and overflows on the fraction,
        // and this runs inside the drain tick where a panic is the app.
        value = value
            .checked_add(i64::from(digit - b'0') * scale)
            .ok_or_else(not_seconds)?;
        scale /= 10;
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_operations_each_parse_to_their_own_command() {
        assert_eq!(
            parse_control_command(r"open D:\Apps\Liveback\capture-0000.lvb"),
            Ok(ControlCommand::Open(
                r"D:\Apps\Liveback\capture-0000.lvb".to_owned()
            ))
        );
        assert_eq!(
            parse_control_command("range 12 34.5"),
            Ok(ControlCommand::Range {
                start_100ns: 120_000_000,
                end_100ns: 345_000_000,
            })
        );
        assert_eq!(
            parse_control_command("clip 検証用のクリップ"),
            Ok(ControlCommand::Clip("検証用のクリップ".to_owned()))
        );
        assert_eq!(
            parse_control_command(r"folder add C:\Users\me\Games"),
            Ok(ControlCommand::FolderAdd(r"C:\Users\me\Games".to_owned()))
        );
        assert_eq!(
            parse_control_command(r"folder remove C:\Users\me\Games"),
            Ok(ControlCommand::FolderRemove(
                r"C:\Users\me\Games".to_owned()
            ))
        );
    }

    #[test]
    fn each_command_logs_under_a_name_of_its_own() {
        let names = [
            ControlCommand::Open(String::new()).event_name(),
            ControlCommand::Range {
                start_100ns: 0,
                end_100ns: 1,
            }
            .event_name(),
            ControlCommand::Clip(String::new()).event_name(),
            ControlCommand::FolderAdd(String::new()).event_name(),
            ControlCommand::FolderRemove(String::new()).event_name(),
        ];
        let unique: std::collections::BTreeSet<&str> = names.iter().copied().collect();
        assert_eq!(unique.len(), names.len(), "one grep-able event per command");
    }

    #[test]
    fn a_path_keeps_its_spaces_because_it_is_the_rest_of_the_line() {
        assert_eq!(
            parse_control_command(r"open D:\My Recordings\a b\capture 1.lvb"),
            Ok(ControlCommand::Open(
                r"D:\My Recordings\a b\capture 1.lvb".to_owned()
            ))
        );
        assert_eq!(
            parse_control_command(r"folder add C:\Program Files\Some Game"),
            Ok(ControlCommand::FolderAdd(
                r"C:\Program Files\Some Game".to_owned()
            ))
        );
        // A comment is the rest of the line too, spaces and all.
        assert_eq!(
            parse_control_command("clip  two  words  "),
            Ok(ControlCommand::Clip("two  words".to_owned()))
        );
    }

    #[test]
    fn an_empty_comment_is_accepted_the_way_the_panel_accepts_one() {
        assert_eq!(
            parse_control_command("clip"),
            Ok(ControlCommand::Clip(String::new()))
        );
        assert_eq!(
            parse_control_command("clip    "),
            Ok(ControlCommand::Clip(String::new()))
        );
    }

    #[test]
    fn an_unknown_command_is_a_refusal_rather_than_a_silent_drop() {
        assert_eq!(
            parse_control_command("discard everything"),
            Err(ControlError::UnknownCommand("discard".to_owned()))
        );
        assert_eq!(parse_control_command("   "), Err(ControlError::Empty));
        assert_eq!(
            parse_control_command("folder toggle C:\\x"),
            Err(ControlError::UnknownFolderAction("toggle".to_owned()))
        );
        assert_eq!(
            parse_control_command("folder"),
            Err(ControlError::MissingArgument("add or remove"))
        );
        assert_eq!(
            parse_control_command("folder add"),
            Err(ControlError::MissingArgument("a folder path"))
        );
        assert_eq!(
            parse_control_command("open"),
            Err(ControlError::MissingArgument("a path to a .lvb file"))
        );
    }

    #[test]
    fn a_range_needs_two_numbers_in_order() {
        assert_eq!(
            parse_control_command("range 12"),
            Err(ControlError::WrongArgumentCount {
                expected: 2,
                found: 1
            })
        );
        assert_eq!(
            parse_control_command("range 1 2 3"),
            Err(ControlError::WrongArgumentCount {
                expected: 2,
                found: 3
            })
        );
        assert_eq!(
            parse_control_command("range"),
            Err(ControlError::WrongArgumentCount {
                expected: 2,
                found: 0
            })
        );
        assert_eq!(
            parse_control_command("range 34 12"),
            Err(ControlError::EmptyRange)
        );
        assert_eq!(
            parse_control_command("range 12 12"),
            Err(ControlError::EmptyRange),
            "a zero-length range is a mistake, not a request"
        );
    }

    #[test]
    fn seconds_that_are_not_a_plain_decimal_are_refused() {
        for text in [
            "abc", "1e2", "inf", "NaN", "-1", "+1", "1.", ".5", "1.2.3", "0x10",
        ] {
            let line = format!("range {text} 99");
            assert_eq!(
                parse_control_command(&line),
                Err(ControlError::NotSeconds(text.to_owned())),
                "`{text}` must not be read as a count of seconds"
            );
        }
        // A space splits the number into two arguments, which is a count
        // error rather than a malformed number.
        assert_eq!(
            parse_control_command("range 1 2 99"),
            Err(ControlError::WrongArgumentCount {
                expected: 2,
                found: 3
            })
        );
    }

    #[test]
    fn a_fraction_lands_on_the_hundred_nanosecond_the_timeline_counts_in() {
        assert_eq!(
            parse_control_command("range 0 1.5"),
            Ok(ControlCommand::Range {
                start_100ns: 0,
                end_100ns: 15_000_000,
            })
        );
        assert_eq!(
            parse_control_command("range 0 0.0000001"),
            Ok(ControlCommand::Range {
                start_100ns: 0,
                end_100ns: 1,
            })
        );
        assert_eq!(
            parse_control_command("range 0 0.00000019"),
            Ok(ControlCommand::Range {
                start_100ns: 0,
                end_100ns: 1,
            }),
            "finer than 100ns is truncated, the way the panel truncates past the tenth"
        );
    }

    #[test]
    fn seconds_too_large_to_count_in_100ns_are_refused_rather_than_wrapped() {
        let line = format!("range 0 {}", i64::MAX);
        assert!(matches!(
            parse_control_command(&line),
            Err(ControlError::NotSeconds(_))
        ));
        // The whole part alone fits; the fraction is what tips it over. A
        // panic here would be a panic inside the drain tick.
        let line = format!("range 0 {}.9999999", i64::MAX / HNS_PER_SECOND);
        assert!(matches!(
            parse_control_command(&line),
            Err(ControlError::NotSeconds(_))
        ));
    }
}

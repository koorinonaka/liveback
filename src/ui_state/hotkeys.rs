//! The one global hotkey and what happens when a re-registration fails
//! (task130).
//!
//! The platform side lives in the slint bin (`global-hotkey`'s
//! `GlobalHotKeyManager`); everything worth testing is here, behind
//! [`Registrar`], so plain `cargo test` covers it.
//!
//! There used to be two chords, marker and clip, with a `role` lookup to tell
//! their event ids apart and a per-role failure report (task730). Task1090
//! removed clip saving, so what is left is one accelerator -- the pair
//! machinery went with it rather than being kept general for a second chord
//! nobody has asked for.

/// The platform registrar. `register` yields the id the fired event carries,
/// which is how the handler recognises its own chord after a remap.
pub trait Registrar {
    fn register(&self, accelerator: &str) -> Result<u32, String>;
    fn unregister(&self, id: u32) -> Result<(), String>;
}

#[derive(Default)]
pub struct Hotkeys {
    /// The id the OS currently answers with. `None` while suspended, or when
    /// another app holds the chord.
    current: Option<u32>,
    /// The accelerator we last meant to hold, kept even while suspended so
    /// [`Hotkeys::resume`] and a refused [`Hotkeys::apply`] both know what to
    /// go back to (task160).
    wanted: Option<String>,
    failed: bool,
}

impl Hotkeys {
    /// Whether this event id is our chord.
    pub fn is_ours(&self, id: u32) -> bool {
        self.current == Some(id)
    }

    /// True when the accelerator stored in settings is *not* the one the OS
    /// currently answers for -- either nothing is registered, or a rejected
    /// remap left the previous chord in place.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// True when the chord is not registered while the app means it to be --
    /// another app holds it (task730). Suspended is not failure: while the
    /// settings screen is armed nothing is registered on purpose.
    pub fn unavailable(&self) -> bool {
        self.wanted.is_some() && self.current.is_none() && self.failed
    }

    /// Registers `marker`, replacing whatever was registered before. A failure
    /// rolls back to the previous accelerator (best effort) rather than leaving
    /// the app with no working hotkey, mirroring `set_hotkeys` in the Tauri
    /// build.
    pub fn apply(&mut self, registrar: &impl Registrar, marker: &str) -> Result<(), String> {
        self.unregister_current(registrar);
        match registrar.register(marker) {
            Ok(id) => {
                self.current = Some(id);
                self.wanted = Some(marker.to_owned());
                self.failed = false;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                match self.wanted.clone() {
                    // A rejected remap. Roll back to `wanted` rather than to
                    // what was registered a moment ago: while the settings
                    // screen is armed nothing is registered, and `wanted` is
                    // the only record of what the user is supposed to end up
                    // with (task146).
                    Some(previous) => {
                        self.current = registrar.register(&previous).ok();
                    }
                    // Startup: nothing to roll back to. Remember the intent so
                    // a suspend/resume cycle retries it.
                    None => {
                        self.current = None;
                        self.wanted = Some(marker.to_owned());
                    }
                }
                Err(error)
            }
        }
    }

    fn unregister_current(&mut self, registrar: &impl Registrar) {
        if let Some(id) = self.current.take() {
            let _ = registrar.unregister(id);
        }
    }

    /// Hands the accelerator back to the OS while the settings screen captures
    /// a chord: `RegisterHotKey` intercepts the combination system-wide, so a
    /// chord this app holds never reaches the focused window and could not be
    /// re-bound (task160). Idempotent, and deliberately not a failure -- it
    /// leaves `failed` alone.
    pub fn suspend(&mut self, registrar: &impl Registrar) {
        self.unregister_current(registrar);
    }

    /// Puts `wanted` back after a [`Hotkeys::suspend`]. A no-op when something
    /// is already registered, or when nothing was ever applied.
    pub fn resume(&mut self, registrar: &impl Registrar) -> Result<(), String> {
        if self.current.is_some() {
            return Ok(());
        }
        let Some(marker) = self.wanted.clone() else {
            return Ok(());
        };
        match registrar.register(&marker) {
            Ok(id) => {
                self.current = Some(id);
                self.failed = false;
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Hands out ids in order and refuses any accelerator in `rejected`.
    #[derive(Default)]
    struct Fake {
        next_id: RefCell<u32>,
        live: RefCell<Vec<u32>>,
        rejected: Vec<&'static str>,
    }

    impl Fake {
        fn rejecting(accelerator: &'static str) -> Self {
            Self {
                rejected: vec![accelerator],
                ..Self::default()
            }
        }
    }

    impl Registrar for Fake {
        fn register(&self, accelerator: &str) -> Result<u32, String> {
            if self.rejected.contains(&accelerator) {
                return Err(format!("{accelerator} is taken"));
            }
            let mut next = self.next_id.borrow_mut();
            *next += 1;
            self.live.borrow_mut().push(*next);
            Ok(*next)
        }

        fn unregister(&self, id: u32) -> Result<(), String> {
            self.live.borrow_mut().retain(|live| *live != id);
            Ok(())
        }
    }

    #[test]
    fn the_registered_id_is_the_one_the_handler_answers_to() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        assert!(hotkeys.is_ours(1));
        assert!(!hotkeys.is_ours(2));
        assert!(!hotkeys.failed());
        assert!(!hotkeys.unavailable());
    }

    #[test]
    fn a_remap_moves_the_chord_onto_the_new_id() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, "Ctrl+Alt+M").unwrap();
        assert!(!hotkeys.is_ours(1));
        assert!(hotkeys.is_ours(2));
        // The old chord is gone from the OS, not merely forgotten.
        assert_eq!(*registrar.live.borrow(), vec![2]);
    }

    #[test]
    fn a_rejected_remap_restores_the_previous_chord_and_reports_failure() {
        let registrar = Fake::rejecting("Ctrl+Alt+C");
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        let error = hotkeys.apply(&registrar, "Ctrl+Alt+C").unwrap_err();
        assert!(error.contains("Ctrl+Alt+C"), "{error}");
        assert!(hotkeys.failed());
        // Re-registered, so the id moved -- but the chord still answers.
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert!(hotkeys.is_ours(live[0]));
    }

    /// Task730's concern, now with one chord: another app holding it must be
    /// visible rather than silently doing nothing.
    #[test]
    fn a_taken_chord_at_startup_is_reported_as_unavailable() {
        let registrar = Fake::rejecting("Ctrl+Shift+R");
        let mut hotkeys = Hotkeys::default();
        assert!(hotkeys.apply(&registrar, "Ctrl+Shift+R").is_err());
        assert!(hotkeys.failed());
        assert!(hotkeys.unavailable());
        assert!(registrar.live.borrow().is_empty());
    }

    #[test]
    fn resume_after_a_refused_first_registration_retries_it() {
        let registrar = Fake::rejecting("Ctrl+Shift+R");
        let mut hotkeys = Hotkeys::default();
        let _ = hotkeys.apply(&registrar, "Ctrl+Shift+R");
        hotkeys.suspend(&registrar);
        // The intent is remembered even though registration failed, so leaving
        // the settings field tries again rather than losing the chord.
        assert!(hotkeys.resume(&registrar).is_err());
        assert!(hotkeys.unavailable());
    }

    #[test]
    fn suspending_hands_the_accelerator_back_to_the_os() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        assert!(!hotkeys.is_ours(1));
        // Giving it up on purpose is not a failure.
        assert!(!hotkeys.failed());
        assert!(!hotkeys.unavailable());
    }

    #[test]
    fn resuming_registers_the_same_chord_again() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        hotkeys.resume(&registrar).unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert!(hotkeys.is_ours(live[0]));
        assert!(!hotkeys.failed());
    }

    #[test]
    fn suspend_and_resume_are_both_idempotent() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        hotkeys.resume(&registrar).unwrap();
        hotkeys.resume(&registrar).unwrap();
        assert_eq!(registrar.live.borrow().len(), 1, "no double registration");
        // Resuming something that was never applied does nothing at all.
        let mut untouched = Hotkeys::default();
        untouched.resume(&registrar).unwrap();
        assert!(!untouched.failed());
    }

    #[test]
    fn a_capture_confirmed_while_suspended_registers_the_new_chord() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        hotkeys.apply(&registrar, "Ctrl+Shift+G").unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert!(hotkeys.is_ours(live[0]));
        assert!(!hotkeys.failed());
    }

    #[test]
    fn a_refused_capture_while_suspended_falls_back_to_the_suspended_chord() {
        let registrar = Fake::rejecting("Ctrl+Alt+C");
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        assert!(hotkeys.apply(&registrar, "Ctrl+Alt+C").is_err());
        // Nothing was registered when the refusal happened, so `current` could
        // not have said what to go back to -- `wanted` did.
        assert!(hotkeys.failed());
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert!(hotkeys.is_ours(live[0]));
    }

    #[test]
    fn a_resume_the_os_refuses_reports_failure() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        // Something else grabbed the chord while the field was armed.
        let taken = Fake::rejecting("Ctrl+Shift+R");
        let error = hotkeys.resume(&taken).unwrap_err();
        assert!(error.contains("Ctrl+Shift+R"), "{error}");
        assert!(hotkeys.failed());
        assert!(hotkeys.unavailable());
        assert!(taken.live.borrow().is_empty());
    }
}

//! The global hotkeys and what happens when a re-registration fails
//! (task130).
//!
//! The platform side lives in the slint bin (`global-hotkey`'s
//! `GlobalHotKeyManager`); everything worth testing is here, behind
//! [`Registrar`], so plain `cargo test` covers it.
//!
//! Two chords, each with its own [`Role`]: the marker, and the record
//! start/stop toggle (t261025-3669). Task1090 had cut the role machinery down
//! to one chord when clip saving went; the record toggle is the second chord
//! that was asked for, so the roles are back. The record chord may be unset
//! (an empty accelerator), which registers nothing and is not a failure.
//! t261025-034f added a third, the show-window chord, unset the same way.

/// The platform registrar. `register` yields the id the fired event carries,
/// which is how the handler recognises its own chord after a remap.
pub trait Registrar {
    fn register(&self, accelerator: &str) -> Result<u32, String>;
    fn unregister(&self, id: u32) -> Result<(), String>;
}

/// Which chord an event or a setting is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Marker,
    Record,
    /// Raise the window, and review the recording (t261025-034f). Unset by
    /// default, like `Record`.
    Show,
}

impl Role {
    const ALL: [Role; 3] = [Role::Marker, Role::Record, Role::Show];
}

#[derive(Default)]
struct Chord {
    /// The id the OS currently answers with. `None` while suspended, unset, or
    /// when another app holds the chord.
    current: Option<u32>,
    /// The accelerator we last meant to hold, kept even while suspended so
    /// [`Hotkeys::resume`] and a refused [`Hotkeys::apply`] both know what to
    /// go back to (task160). `None` is unset.
    wanted: Option<String>,
    failed: bool,
}

impl Chord {
    fn unregister_current(&mut self, registrar: &impl Registrar) {
        if let Some(id) = self.current.take() {
            let _ = registrar.unregister(id);
        }
    }

    fn apply(&mut self, registrar: &impl Registrar, accelerator: &str) -> Result<(), String> {
        self.unregister_current(registrar);
        if accelerator.is_empty() {
            self.wanted = None;
            self.failed = false;
            return Ok(());
        }
        match registrar.register(accelerator) {
            Ok(id) => {
                self.current = Some(id);
                self.wanted = Some(accelerator.to_owned());
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
                        self.wanted = Some(accelerator.to_owned());
                    }
                }
                Err(error)
            }
        }
    }

    fn resume(&mut self, registrar: &impl Registrar) -> Result<(), String> {
        if self.current.is_some() {
            return Ok(());
        }
        let Some(accelerator) = self.wanted.clone() else {
            return Ok(());
        };
        match registrar.register(&accelerator) {
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

/// Raised by [`Hotkeys::apply`] when the other role already holds the chord.
pub const CLASH: &str = "another hotkey already uses this chord";

#[derive(Default)]
pub struct Hotkeys {
    /// Indexed by `Role as usize`.
    chords: [Chord; 3],
}

impl Hotkeys {
    fn chord(&self, role: Role) -> &Chord {
        &self.chords[role as usize]
    }

    fn chord_mut(&mut self, role: Role) -> &mut Chord {
        &mut self.chords[role as usize]
    }

    /// Which chord this event id is, or `None` when it is not ours.
    pub fn role_of(&self, id: u32) -> Option<Role> {
        Role::ALL
            .into_iter()
            .find(|role| self.chord(*role).current == Some(id))
    }

    /// True when the accelerator stored in settings is *not* the one the OS
    /// currently answers for -- either nothing is registered, or a rejected
    /// remap left the previous chord in place.
    pub fn failed(&self, role: Role) -> bool {
        self.chord(role).failed
    }

    /// True when the chord is not registered while the app means it to be --
    /// another app holds it (task730). Suspended is not failure: while the
    /// settings screen is armed nothing is registered on purpose. Unset is not
    /// either.
    pub fn unavailable(&self, role: Role) -> bool {
        let chord = self.chord(role);
        chord.wanted.is_some() && chord.current.is_none() && chord.failed
    }

    /// Registers `accelerator` for `role`, replacing whatever that role held
    /// before. An empty accelerator unregisters and leaves the role unset. A
    /// failure rolls back to the previous accelerator (best effort) rather
    /// than leaving the app with no working hotkey, mirroring `set_hotkeys` in
    /// the Tauri build. A chord another role holds is refused with [`CLASH`]
    /// and changes nothing.
    pub fn apply(
        &mut self,
        registrar: &impl Registrar,
        role: Role,
        accelerator: &str,
    ) -> Result<(), String> {
        if !accelerator.is_empty()
            && Role::ALL
                .into_iter()
                .filter(|other| *other != role)
                .any(|other| {
                    self.chord(other)
                        .wanted
                        .as_deref()
                        .is_some_and(|held| held.eq_ignore_ascii_case(accelerator))
                })
        {
            return Err(CLASH.to_owned());
        }
        self.chord_mut(role).apply(registrar, accelerator)
    }

    /// Hands every accelerator back to the OS while the settings screen
    /// captures a chord: `RegisterHotKey` intercepts the combination
    /// system-wide, so a chord this app holds never reaches the focused window
    /// and could not be re-bound (task160). All of them, whichever row is
    /// armed: the other rows' chords have to reach the field too, to be
    /// refused as a clash. Idempotent, and deliberately not a failure -- it
    /// leaves `failed` alone.
    pub fn suspend(&mut self, registrar: &impl Registrar) {
        for chord in &mut self.chords {
            chord.unregister_current(registrar);
        }
    }

    /// Puts every `wanted` chord back after a [`Hotkeys::suspend`]. A no-op
    /// for a chord already registered or never applied. All are tried even
    /// when one fails; the first error (in `Role::ALL` order) is returned with
    /// the role it belongs to, so the caller can name that role's chord
    /// (t261026-43c3).
    pub fn resume(&mut self, registrar: &impl Registrar) -> Result<(), (Role, String)> {
        let mut first = Ok(());
        for (role, chord) in Role::ALL.into_iter().zip(&mut self.chords) {
            let result = chord.resume(registrar);
            if first.is_ok() {
                first = result.map_err(|error| (role, error));
            }
        }
        first
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

    const M: Role = Role::Marker;
    const R: Role = Role::Record;

    #[test]
    fn the_registered_id_is_the_one_the_handler_answers_to() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        assert_eq!(hotkeys.role_of(1), Some(M));
        assert_eq!(hotkeys.role_of(2), None);
        assert!(!hotkeys.failed(M));
        assert!(!hotkeys.unavailable(M));
    }

    #[test]
    fn a_remap_moves_the_chord_onto_the_new_id() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, M, "Ctrl+Alt+M").unwrap();
        assert_eq!(hotkeys.role_of(1), None);
        assert_eq!(hotkeys.role_of(2), Some(M));
        // The old chord is gone from the OS, not merely forgotten.
        assert_eq!(*registrar.live.borrow(), vec![2]);
    }

    #[test]
    fn a_rejected_remap_restores_the_previous_chord_and_reports_failure() {
        let registrar = Fake::rejecting("Ctrl+Alt+C");
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        let error = hotkeys.apply(&registrar, M, "Ctrl+Alt+C").unwrap_err();
        assert!(error.contains("Ctrl+Alt+C"), "{error}");
        assert!(hotkeys.failed(M));
        // Re-registered, so the id moved -- but the chord still answers.
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert_eq!(hotkeys.role_of(live[0]), Some(M));
    }

    /// Task730's concern: another app holding the chord must be visible
    /// rather than silently doing nothing.
    #[test]
    fn a_taken_chord_at_startup_is_reported_as_unavailable() {
        let registrar = Fake::rejecting("Ctrl+Shift+R");
        let mut hotkeys = Hotkeys::default();
        assert!(hotkeys.apply(&registrar, M, "Ctrl+Shift+R").is_err());
        assert!(hotkeys.failed(M));
        assert!(hotkeys.unavailable(M));
        assert!(registrar.live.borrow().is_empty());
    }

    #[test]
    fn resume_after_a_refused_first_registration_retries_it() {
        let registrar = Fake::rejecting("Ctrl+Shift+R");
        let mut hotkeys = Hotkeys::default();
        let _ = hotkeys.apply(&registrar, M, "Ctrl+Shift+R");
        hotkeys.suspend(&registrar);
        // The intent is remembered even though registration failed, so leaving
        // the settings field tries again rather than losing the chord.
        assert!(hotkeys.resume(&registrar).is_err());
        assert!(hotkeys.unavailable(M));
    }

    #[test]
    fn suspending_hands_the_accelerator_back_to_the_os() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        assert_eq!(hotkeys.role_of(1), None);
        // Giving it up on purpose is not a failure.
        assert!(!hotkeys.failed(M));
        assert!(!hotkeys.unavailable(M));
    }

    #[test]
    fn suspend_and_resume_are_both_idempotent() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        hotkeys.resume(&registrar).unwrap();
        hotkeys.resume(&registrar).unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1, "no double registration");
        // Resuming registers the same chord again, and it answers.
        assert_eq!(hotkeys.role_of(live[0]), Some(M));
        assert!(!hotkeys.failed(M));
        // Resuming something that was never applied does nothing at all.
        let mut untouched = Hotkeys::default();
        untouched.resume(&registrar).unwrap();
        assert!(!untouched.failed(M));
        assert_eq!(registrar.live.borrow().len(), 1);
    }

    #[test]
    fn a_capture_confirmed_while_suspended_registers_the_new_chord() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        hotkeys.apply(&registrar, M, "Ctrl+Shift+G").unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert_eq!(hotkeys.role_of(live[0]), Some(M));
        assert!(!hotkeys.failed(M));
    }

    #[test]
    fn a_refused_capture_while_suspended_falls_back_to_the_suspended_chord() {
        let registrar = Fake::rejecting("Ctrl+Alt+C");
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        assert!(hotkeys.apply(&registrar, M, "Ctrl+Alt+C").is_err());
        // Nothing was registered when the refusal happened, so `current` could
        // not have said what to go back to -- `wanted` did.
        assert!(hotkeys.failed(M));
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 1);
        assert_eq!(hotkeys.role_of(live[0]), Some(M));
    }

    #[test]
    fn a_resume_the_os_refuses_reports_failure() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.suspend(&registrar);
        // Something else grabbed the chord while the field was armed.
        let taken = Fake::rejecting("Ctrl+Shift+R");
        let (_, error) = hotkeys.resume(&taken).unwrap_err();
        assert!(error.contains("Ctrl+Shift+R"), "{error}");
        assert!(hotkeys.failed(M));
        assert!(hotkeys.unavailable(M));
        assert!(taken.live.borrow().is_empty());
    }

    // ---------- the second role (t261025-3669) ----------

    #[test]
    fn both_roles_register_and_answer_with_their_own_ids() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        assert_eq!(hotkeys.role_of(1), Some(M));
        assert_eq!(hotkeys.role_of(2), Some(R));
        assert_eq!(*registrar.live.borrow(), vec![1, 2]);
    }

    #[test]
    fn remapping_one_role_leaves_the_other_registered() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F10").unwrap();
        assert_eq!(*registrar.live.borrow(), vec![1, 3]);
        assert_eq!(hotkeys.role_of(1), Some(M));
        assert_eq!(hotkeys.role_of(3), Some(R));
    }

    /// Unset is the record role's default: nothing registered, nothing
    /// reported, and a suspend/resume cycle does not invent a registration.
    #[test]
    fn an_unset_chord_registers_nothing_and_is_not_unavailable() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, R, "").unwrap();
        assert!(registrar.live.borrow().is_empty());
        assert!(!hotkeys.unavailable(R));
        assert!(!hotkeys.failed(R));
        hotkeys.suspend(&registrar);
        hotkeys.resume(&registrar).unwrap();
        assert!(registrar.live.borrow().is_empty());
        // Setting it and unsetting it again gives the chord back to the OS.
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        hotkeys.apply(&registrar, R, "").unwrap();
        assert!(registrar.live.borrow().is_empty());
        assert!(!hotkeys.unavailable(R));
    }

    #[test]
    fn the_other_roles_chord_is_refused_as_a_clash_and_changes_nothing() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        // While suspended too: that is when the settings field is armed.
        hotkeys.suspend(&registrar);
        assert_eq!(
            hotkeys.apply(&registrar, R, "ctrl+shift+r").unwrap_err(),
            CLASH
        );
        assert_eq!(
            hotkeys.apply(&registrar, M, "Ctrl+Alt+F9").unwrap_err(),
            CLASH
        );
        assert!(!hotkeys.failed(R) && !hotkeys.failed(M));
        hotkeys.resume(&registrar).unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 2);
        assert_eq!(hotkeys.role_of(live[0]), Some(M));
        assert_eq!(hotkeys.role_of(live[1]), Some(R));
    }

    #[test]
    fn suspend_and_resume_cover_both_roles() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        hotkeys.resume(&registrar).unwrap();
        assert_eq!(registrar.live.borrow().len(), 2);
        // A resume the OS refuses for one role still restores the other.
        hotkeys.suspend(&registrar);
        let marker_taken = Fake::rejecting("Ctrl+Shift+R");
        assert!(hotkeys.resume(&marker_taken).is_err());
        assert!(hotkeys.unavailable(M));
        assert!(!hotkeys.unavailable(R));
        assert_eq!(marker_taken.live.borrow().len(), 1);
    }

    /// t261026-43c3: a refused resume names the role whose chord the OS
    /// refused, so the toast can show that chord rather than the marker's.
    #[test]
    fn a_refused_resume_names_the_role_that_failed() {
        for (taken, expected) in [
            ("Ctrl+Shift+R", Role::Marker),
            ("Ctrl+Alt+F9", Role::Record),
            ("Ctrl+Alt+F10", Role::Show),
        ] {
            let registrar = Fake::default();
            let mut hotkeys = Hotkeys::default();
            hotkeys
                .apply(&registrar, Role::Marker, "Ctrl+Shift+R")
                .unwrap();
            hotkeys
                .apply(&registrar, Role::Record, "Ctrl+Alt+F9")
                .unwrap();
            hotkeys
                .apply(&registrar, Role::Show, "Ctrl+Alt+F10")
                .unwrap();
            hotkeys.suspend(&registrar);
            let (role, error) = hotkeys.resume(&Fake::rejecting(taken)).unwrap_err();
            assert_eq!(role, expected, "{taken}");
            assert!(error.contains(taken), "{error}");
        }
    }

    // ---------- the third role (t261025-034f) ----------

    const S: Role = Role::Show;

    #[test]
    fn the_show_role_registers_beside_the_other_two() {
        let registrar = Fake::default();
        let mut hotkeys = Hotkeys::default();
        hotkeys.apply(&registrar, M, "Ctrl+Shift+R").unwrap();
        hotkeys.apply(&registrar, R, "Ctrl+Alt+F9").unwrap();
        hotkeys.apply(&registrar, S, "Ctrl+Alt+F10").unwrap();
        assert_eq!(hotkeys.role_of(3), Some(S));
        hotkeys.suspend(&registrar);
        assert!(registrar.live.borrow().is_empty());
        hotkeys.resume(&registrar).unwrap();
        let live = registrar.live.borrow().clone();
        assert_eq!(live.len(), 3);
        assert_eq!(hotkeys.role_of(live[2]), Some(S));
        hotkeys.apply(&registrar, S, "").unwrap();
        assert_eq!(registrar.live.borrow().len(), 2);
        assert!(!hotkeys.unavailable(S));
    }

    /// Each role against each other one: a two-way check that forgot the
    /// third role would let one of these through.
    #[test]
    fn every_pair_of_roles_refuses_a_shared_chord() {
        let chords = [(M, "Ctrl+Shift+R"), (R, "Ctrl+Alt+F9"), (S, "Ctrl+Alt+F10")];
        for (role, _) in chords {
            for (other, held) in chords {
                if role == other {
                    continue;
                }
                let registrar = Fake::default();
                let mut hotkeys = Hotkeys::default();
                hotkeys.apply(&registrar, other, held).unwrap();
                assert_eq!(
                    hotkeys.apply(&registrar, role, held).unwrap_err(),
                    CLASH,
                    "{role:?} took {other:?}'s {held}"
                );
                assert_eq!(*registrar.live.borrow(), vec![1]);
            }
        }
    }
}

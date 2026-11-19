//! Entering a COM apartment on the calling thread (t261025-7188).
//!
//! The apartment belongs to the thread, so a thread picks one and keeps it:
//! the shell calls want STA, Media Foundation and WASAPI threads use MTA.
//! The result is deliberately ignored. `S_FALSE` (already in this apartment)
//! and `RPC_E_CHANGED_MODE` (already in the other one) both mean someone set
//! the thread up first, which every caller is fine with, and `CoUninitialize`
//! is never called.

use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_MULTITHREADED};

/// Joins the multithreaded apartment.
pub fn init_mta() {
    // SAFETY: no pointer arguments; the result is deliberately dropped.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Enters a single-threaded apartment.
pub fn init_sta() {
    // SAFETY: no pointer arguments; the result is deliberately dropped.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

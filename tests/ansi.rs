//! Contracts of the ANSI processor seen through its public API.
#![cfg(feature = "ansi")]

use std::time::Duration;

use vvte::ansi::{
    Handler, KeyboardModes, KeyboardModesApplyBehavior, Processor, StdSyncHandler, Timeout,
};

#[derive(Default)]
struct Keyboard {
    set: Vec<(KeyboardModes, KeyboardModesApplyBehavior)>,
    pushed: Vec<KeyboardModes>,
}

impl Handler for Keyboard {
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        self.set.push((mode, behavior));
    }

    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        self.pushed.push(mode);
    }
}

#[test]
fn keyboard_flags_out_of_range_are_rejected_not_truncated() {
    let mut processor: Processor = Processor::new();
    let mut keyboard = Keyboard::default();

    // 257 used to be cut to the byte 1, enabling DISAMBIGUATE_ESC_CODES.
    processor.advance(&mut keyboard, b"\x1b[=257;2u\x1b[>256u");
    assert!(keyboard.set.is_empty());
    assert!(keyboard.pushed.is_empty());

    processor.advance(&mut keyboard, b"\x1b[=5;2u\x1b[>255u");
    let five = KeyboardModes::DISAMBIGUATE_ESC_CODES | KeyboardModes::REPORT_ALTERNATE_KEYS;
    assert_eq!(keyboard.set, [(five, KeyboardModesApplyBehavior::Union)]);
    // Unknown bits of an in-range value are still dropped.
    assert_eq!(keyboard.pushed, [KeyboardModes::all()]);
}

/// The processor holds input for as long as this reports a pending update, so it must not
/// turn `false` on its own when the deadline passes.
#[test]
fn std_sync_handler_stays_pending_past_its_deadline_until_cleared() {
    let mut timeout = StdSyncHandler::default();
    assert!(!timeout.pending_timeout());

    timeout.set_timeout(Duration::ZERO);
    let deadline = timeout.sync_timeout().expect("a deadline is recorded");
    assert!(deadline <= std::time::Instant::now());
    assert!(timeout.pending_timeout(), "an expired update is still pending until cleared");

    timeout.clear_timeout();
    assert!(!timeout.pending_timeout());
    assert_eq!(timeout.sync_timeout(), None);
}

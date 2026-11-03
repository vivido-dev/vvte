// SPDX-License-Identifier: Apache-2.0
//
// This module was originally part of the `alacritty_terminal` crate, which is
// licensed under the Apache License, Version 2.0 and is part of the Alacritty
// project (https://github.com/alacritty/alacritty).

//! Terminal semantics on top of the raw parser: cursor, colors, modes, and synchronized updates.
//!
//! [`Processor`] wraps a [`Parser`](crate::Parser) and translates the actions it reports into
//! calls on a [`Handler`], the interface a terminal emulator implements. Every [`Handler`]
//! method has a no-op default, so an implementation overrides only what it supports. The other
//! types in this module are the values those calls carry.
//!
//! # Synchronized updates
//!
//! `CSI ? 2026 h` (BSU) begins a synchronized update and `CSI ? 2026 l` (ESU) ends it, so that a
//! program can redraw the screen without the terminal showing it half-drawn. While an update is
//! pending, [`Processor::advance`] holds the incoming bytes instead of dispatching them, and
//! replays them in order when the update ends: at ESU, when the held bytes would reach 2 MiB, or
//! when the caller calls [`Processor::stop_sync`].
//!
//! A processor made with [`Processor::with_unbuffered_sync`] holds nothing: it dispatches every
//! byte as it arrives and only reports the update, through [`Handler::set_private_mode`] at BSU
//! and [`Handler::unset_private_mode`] when it ends. The handler then decides what not to
//! present in between. Such an update cannot be extended: a BSU inside it leaves its deadline
//! where it was, so nothing a program sends can keep the screen from being presented for more
//! than 150 ms at a time.
//!
//! The processor never ends an update because time has passed. At each BSU it calls
//! [`Timeout::set_timeout`] with a 150 ms deadline, and **the caller must check that deadline and
//! call [`Processor::stop_sync`] once it has passed** — with the default [`StdSyncHandler`],
//! through [`Processor::sync_timeout`]. A caller that never checks leaves a program's output held
//! until it sends ESU, which a crashed or misbehaving program never does.
//!
//! # Examples
//!
//! ```
//! use vvte::ansi::{Handler, Processor};
//!
//! #[derive(Default)]
//! struct Screen {
//!     text: String,
//!     title: Option<String>,
//! }
//!
//! impl Handler for Screen {
//!     fn input(&mut self, c: char) {
//!         self.text.push(c);
//!     }
//!
//!     fn set_title(&mut self, title: Option<String>) {
//!         self.title = title;
//!     }
//! }
//!
//! let mut processor: Processor = Processor::new();
//! let mut screen = Screen::default();
//! processor.advance(&mut screen, b"\x1b]2;build\x07ok");
//! assert_eq!(screen.text, "ok");
//! assert_eq!(screen.title.as_deref(), Some("build"));
//! ```

use core::fmt;
use core::ops::{Add, Mul};
use core::str::FromStr;
use core::time::Duration;
use core::{iter, mem, str};
use std::time::Instant;

use bitflags::bitflags;
#[doc(inline)]
pub use cursor_icon;
use cursor_icon::CursorIcon;
use log::debug;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::{Params, ParamsIter};

/// Maximum time before a synchronized update is aborted.
const SYNC_UPDATE_TIMEOUT: Duration = Duration::from_millis(150);

/// Maximum number of bytes read in one synchronized update (2MiB).
const SYNC_BUFFER_SIZE: usize = 0x20_0000;

/// Buffer capacity kept between synchronized updates (64KiB).
///
/// The buffer used to be reserved at `SYNC_BUFFER_SIZE` when a `Processor` was constructed, so
/// every pane paid 2MiB whether or not it ever saw a synchronized update. It now grows on demand
/// and keeps this much afterwards: a synchronized update is normally a screen repaint, comfortably
/// under this, so the usual case still never reallocates. Anything larger is a one-off and is
/// handed back rather than held for the life of the pane.
const SYNC_RETAINED_CAPACITY: usize = 64 * 1024;

/// Number of bytes in the BSU/ESU CSI sequences.
const SYNC_ESCAPE_LEN: usize = 8;

/// BSU CSI sequence for beginning or extending synchronized updates.
const BSU_CSI: [u8; SYNC_ESCAPE_LEN] = *b"\x1b[?2026h";

/// ESU CSI sequence for terminating synchronized updates.
const ESU_CSI: [u8; SYNC_ESCAPE_LEN] = *b"\x1b[?2026l";

/// A hyperlink set by OSC 8.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Hyperlink {
    /// Identifier for the given hyperlink.
    pub id: Option<String>,
    /// Resource identifier of the hyperlink.
    pub uri: String,
}

/// A 24-bit color.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Rgb {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Rgb {
    /// Implementation of [W3C's luminance algorithm], used by [`Rgb::contrast`].
    ///
    /// [W3C's luminance algorithm]: https://www.w3.org/TR/WCAG20/#relativeluminancedef
    fn luminance(self) -> f64 {
        let channel_luminance = |channel: u8| {
            let channel = f64::from(channel) / 255.;
            if channel <= 0.03928 {
                channel / 12.92
            } else {
                f64::powf((channel + 0.055) / 1.055, 2.4)
            }
        };

        let r_luminance = channel_luminance(self.r);
        let g_luminance = channel_luminance(self.g);
        let b_luminance = channel_luminance(self.b);

        0.2126 * r_luminance + 0.7152 * g_luminance + 0.0722 * b_luminance
    }

    /// Implementation of [W3C's contrast algorithm].
    ///
    /// [W3C's contrast algorithm]: https://www.w3.org/TR/WCAG20/#contrast-ratiodef
    #[must_use]
    pub fn contrast(self, other: Rgb) -> f64 {
        let self_luminance = self.luminance();
        let other_luminance = other.luminance();

        let (darker, lighter) = if self_luminance > other_luminance {
            (other_luminance, self_luminance)
        } else {
            (self_luminance, other_luminance)
        };

        (lighter + 0.05) / (darker + 0.05)
    }
}

// A multiply function for Rgb, as the default dim is just *2/3.
//
// Renderers call this per dimmed cell, so it must stay free of logging.
impl Mul<f32> for Rgb {
    type Output = Rgb;

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "each channel is clamped to 0..=255 before the cast"
    )]
    fn mul(self, rhs: f32) -> Rgb {
        Rgb {
            r: (f32::from(self.r) * rhs).clamp(0.0, 255.0) as u8,
            g: (f32::from(self.g) * rhs).clamp(0.0, 255.0) as u8,
            b: (f32::from(self.b) * rhs).clamp(0.0, 255.0) as u8,
        }
    }
}

impl Add<Rgb> for Rgb {
    type Output = Rgb;

    fn add(self, rhs: Rgb) -> Rgb {
        Rgb {
            r: self.r.saturating_add(rhs.r),
            g: self.g.saturating_add(rhs.g),
            b: self.b.saturating_add(rhs.b),
        }
    }
}

/// Parse colors in `XParseColor` format.
fn xparse_color(color: &[u8]) -> Option<Rgb> {
    if let Some(color) = color.strip_prefix(b"#") {
        parse_legacy_color(color)
    } else if let Some(color) = color.strip_prefix(b"rgb:") {
        parse_rgb_color(color)
    } else {
        None
    }
}

/// Parse colors in `rgb:r(rrr)/g(ggg)/b(bbb)` format.
fn parse_rgb_color(color: &[u8]) -> Option<Rgb> {
    let mut colors = str::from_utf8(color).ok()?.split('/');
    let (r, g, b) = (colors.next()?, colors.next()?, colors.next()?);
    if colors.next().is_some() {
        return None;
    }

    // Scale values instead of filling with `0`s.
    let scale = |input: &str| {
        // The largest value that many digits can hold; it maps to 255.
        let max: u32 = match input.len() {
            1 => 0xF,
            2 => 0xFF,
            3 => 0xFFF,
            4 => 0xFFFF,
            _ => return None,
        };
        let value = u32::from_str_radix(input, 16).ok()?;
        u8::try_from(255 * value / max).ok()
    };

    Some(Rgb { r: scale(r)?, g: scale(g)?, b: scale(b)? })
}

/// Parse colors in `#r(rrr)g(ggg)b(bbb)` format.
fn parse_legacy_color(color: &[u8]) -> Option<Rgb> {
    let item_len = color.len() / 3;

    // Truncate/Fill to two hex digits: keep the two most significant digits of the channel,
    // and fill a single digit with a zero. Only those digits are converted, so a channel of any
    // length, leading zeros included, cannot overflow.
    let color_from_slice = |slice: &[u8]| {
        if slice.is_empty() || !slice.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let top = str::from_utf8(&slice[..slice.len().min(2)]).ok()?;
        let value = u8::from_str_radix(top, 16).ok()?;
        Some(if slice.len() == 1 { value << 4 } else { value })
    };

    Some(Rgb {
        r: color_from_slice(&color[0..item_len])?,
        g: color_from_slice(&color[item_len..item_len * 2])?,
        b: color_from_slice(&color[item_len * 2..])?,
    })
}

fn parse_number(input: &[u8]) -> Option<u8> {
    if input.is_empty() {
        return None;
    }
    let mut num: u8 = 0;
    for &c in input {
        let digit = c.wrapping_sub(b'0');
        if digit > 9 {
            return None;
        }
        num = num.checked_mul(10)?.checked_add(digit)?;
    }
    Some(num)
}

/// Internal state for VTE processor.
#[derive(Debug, Default)]
struct ProcessorState<T: Timeout> {
    /// Last processed character for repetition.
    preceding_char: Option<char>,

    /// State for synchronized terminal updates.
    sync_state: SyncState<T>,
}

#[derive(Debug)]
struct SyncState<T: Timeout> {
    /// Handler for synchronized updates.
    timeout: T,

    /// Bytes read during the synchronized update.
    buffer: Vec<u8>,

    /// Whether bytes inside a synchronized update are held until it ends.
    buffered: bool,
}

impl<T: Timeout> Default for SyncState<T> {
    fn default() -> Self {
        // Grown on the first synchronized update rather than reserved up front; `advance_sync`
        // bounds it against `SYNC_BUFFER_SIZE` explicitly, so the reservation bought nothing but
        // 2MiB per pane.
        Self { buffer: Vec::new(), timeout: Default::default(), buffered: true }
    }
}

/// Parses a terminal byte stream and dispatches it to a [`Handler`].
///
/// A processor owns a [`Parser`](crate::Parser) plus the state the ANSI layer keeps between
/// calls: the last printed character, for REP, and any synchronized update in progress. Each
/// terminal needs its own. `T` tracks the synchronized-update deadline; the default
/// [`StdSyncHandler`] uses [`Instant`], and tests can supply their own [`Timeout`].
///
/// Callers must enforce the synchronized-update deadline themselves; the
/// [module documentation](crate::ansi#synchronized-updates) describes the contract.
///
/// # Examples
///
/// Ending a synchronized update whose deadline has passed:
///
/// ```
/// use std::time::Instant;
///
/// use vvte::ansi::{Handler, Processor};
///
/// #[derive(Default)]
/// struct Screen(String);
///
/// impl Handler for Screen {
///     fn input(&mut self, c: char) {
///         self.0.push(c);
///     }
/// }
///
/// /// Runs whenever the event loop wakes up, for example after a read times out.
/// fn on_wake(processor: &mut Processor, screen: &mut Screen) {
///     let deadline = processor.sync_timeout().sync_timeout();
///     if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
///         processor.stop_sync(screen);
///     }
/// }
///
/// let mut processor: Processor = Processor::new();
/// let mut screen = Screen::default();
/// processor.advance(&mut screen, b"\x1b[?2026hheld");
/// assert_eq!(screen.0, "", "output is held while the update is pending");
///
/// on_wake(&mut processor, &mut screen);
/// // Once 150 ms have passed, `on_wake` makes this call:
/// processor.stop_sync(&mut screen);
/// assert_eq!(screen.0, "held");
/// ```
#[derive(Default)]
pub struct Processor<T: Timeout = StdSyncHandler> {
    state: ProcessorState<T>,
    parser: crate::Parser,
}

/// Shows the parser and synchronized-update state, not the held bytes.
impl<T: Timeout + fmt::Debug> fmt::Debug for Processor<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Processor")
            .field("parser", &self.parser)
            .field("preceding_char", &self.state.preceding_char)
            .field("sync_timeout", &self.state.sync_state.timeout)
            .field("sync_bytes", &self.state.sync_state.buffer.len())
            .field("sync_buffered", &self.state.sync_state.buffered)
            .finish()
    }
}

impl<T: Timeout> Processor<T> {
    /// Creates a processor with no synchronized update pending.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a processor that dispatches a synchronized update's bytes as they arrive.
    ///
    /// A processor from [`Self::new`] holds them until the update ends and replays them then,
    /// so a terminal parses a whole repaint at once. This one parses them immediately and only
    /// reports the update's start and end to the [`Handler`], which must not present the screen
    /// in between. See the [module documentation](crate::ansi#synchronized-updates).
    ///
    /// # Examples
    ///
    /// ```
    /// use vvte::ansi::{Handler, NamedPrivateMode, PrivateMode, Processor};
    ///
    /// #[derive(Default)]
    /// struct Screen {
    ///     text: String,
    ///     synchronized: bool,
    /// }
    ///
    /// impl Handler for Screen {
    ///     fn input(&mut self, c: char) {
    ///         self.text.push(c);
    ///     }
    ///
    ///     fn set_private_mode(&mut self, mode: PrivateMode) {
    ///         if mode == PrivateMode::Named(NamedPrivateMode::SyncUpdate) {
    ///             self.synchronized = true;
    ///         }
    ///     }
    ///
    ///     fn unset_private_mode(&mut self, mode: PrivateMode) {
    ///         if mode == PrivateMode::Named(NamedPrivateMode::SyncUpdate) {
    ///             self.synchronized = false;
    ///         }
    ///     }
    /// }
    ///
    /// let mut processor: Processor = Processor::with_unbuffered_sync();
    /// let mut screen = Screen::default();
    /// processor.advance(&mut screen, b"\x1b[?2026hdrawn");
    /// assert_eq!(screen.text, "drawn", "the update's bytes are dispatched at once");
    /// assert!(screen.synchronized, "but the screen knows not to present them yet");
    ///
    /// processor.advance(&mut screen, b"\x1b[?2026l");
    /// assert!(!screen.synchronized);
    /// ```
    #[must_use]
    pub fn with_unbuffered_sync() -> Self {
        let mut processor = Self::default();
        processor.state.sync_state.buffered = false;
        processor
    }

    /// Returns the synchronized-update deadline tracker.
    ///
    /// With [`StdSyncHandler`], `processor.sync_timeout().sync_timeout()` is the deadline after
    /// which the caller must call [`Self::stop_sync`].
    pub fn sync_timeout(&self) -> &T {
        &self.state.sync_state.timeout
    }

    /// Parses `bytes` and dispatches the resulting operations to `handler`.
    ///
    /// Like [`Parser::advance`](crate::Parser::advance), `bytes` can end anywhere. While a
    /// synchronized update is pending, bytes are held instead of dispatched, unless the
    /// processor came from [`Self::with_unbuffered_sync`]; see the
    /// [module documentation](crate::ansi#synchronized-updates).
    #[inline]
    pub fn advance<H>(&mut self, handler: &mut H, bytes: &[u8])
    where
        H: Handler,
    {
        let mut processed = 0;
        while processed != bytes.len() {
            if self.state.sync_state.buffered && self.state.sync_state.timeout.pending_timeout() {
                processed += self.advance_sync(handler, &bytes[processed..]);
            } else {
                let mut performer = Performer::new(&mut self.state, handler);
                processed +=
                    self.parser.advance_until_terminated(&mut performer, &bytes[processed..]);
            }
        }
    }

    /// Ends the pending synchronized update and dispatches the bytes it held, in order.
    ///
    /// Call this once the update's deadline has passed (see [`Self::sync_timeout`]). It reports
    /// the end of the update through [`Handler::unset_private_mode`]; with no update pending,
    /// that report is all it does.
    pub fn stop_sync<H>(&mut self, handler: &mut H)
    where
        H: Handler,
    {
        self.stop_sync_internal(handler, None);
    }

    /// End a synchronized update.
    ///
    /// The `bsu_offset` parameter should be passed if the sync buffer contains
    /// a new BSU escape that is not part of the current synchronized
    /// update.
    fn stop_sync_internal<H>(&mut self, handler: &mut H, bsu_offset: Option<usize>)
    where
        H: Handler,
    {
        // Process all synchronized bytes.
        //
        // NOTE: We do not use `advance_until_terminated` here since BSU sequences are
        // processed automatically during the synchronized update.
        let buffer = mem::take(&mut self.state.sync_state.buffer);
        let offset = bsu_offset.unwrap_or(buffer.len());
        let mut performer = Performer::new(&mut self.state, handler);
        self.parser.advance(&mut performer, &buffer[..offset]);
        self.state.sync_state.buffer = buffer;

        if let Some(bsu_offset) = bsu_offset {
            // Just clear processed bytes if there is a new BSU.
            //
            // NOTE: We do not need to re-process for a new ESU since the `advance_sync`
            // function checks for BSUs in reverse.
            let new_len = self.state.sync_state.buffer.len() - bsu_offset;
            self.state.sync_state.buffer.copy_within(bsu_offset.., 0);
            self.state.sync_state.buffer.truncate(new_len);
        } else {
            // Report mode and clear state if no new BSU is present.
            handler.unset_private_mode(NamedPrivateMode::SyncUpdate.into());
            self.state.sync_state.timeout.clear_timeout();
            self.state.sync_state.buffer.clear();
            if self.state.sync_state.buffer.capacity() > SYNC_RETAINED_CAPACITY {
                self.state.sync_state.buffer.shrink_to(SYNC_RETAINED_CAPACITY);
            }
        }
    }

    /// Returns the number of bytes held by the pending synchronized update.
    #[inline]
    pub fn sync_bytes_count(&self) -> usize {
        self.state.sync_state.buffer.len()
    }

    /// Process a new byte during a synchronized update.
    ///
    /// Returns the number of bytes processed.
    #[cold]
    fn advance_sync<H>(&mut self, handler: &mut H, bytes: &[u8]) -> usize
    where
        H: Handler,
    {
        // Advance sync parser or stop sync if we'd exceed the maximum buffer size.
        if self.state.sync_state.buffer.len() + bytes.len() >= SYNC_BUFFER_SIZE - 1 {
            // Terminate the synchronized update.
            self.stop_sync_internal(handler, None);

            // Just parse the bytes normally.
            let mut performer = Performer::new(&mut self.state, handler);
            self.parser.advance_until_terminated(&mut performer, bytes)
        } else {
            self.state.sync_state.buffer.extend(bytes);
            self.advance_sync_csi(handler, bytes.len());
            bytes.len()
        }
    }

    /// Handle BSU/ESU CSI sequences during synchronized update.
    fn advance_sync_csi<H>(&mut self, handler: &mut H, new_bytes: usize)
    where
        H: Handler,
    {
        // Get constraints within which a new escape character might be relevant.
        let buffer_len = self.state.sync_state.buffer.len();
        let start_offset = (buffer_len - new_bytes).saturating_sub(SYNC_ESCAPE_LEN - 1);
        let end_offset = buffer_len.saturating_sub(SYNC_ESCAPE_LEN - 1);
        let search_buffer = &self.state.sync_state.buffer[start_offset..end_offset];

        // Search for termination/extension escapes in the added bytes.
        //
        // NOTE: It is technically legal to specify multiple private modes in the same
        // escape, but we only allow EXACTLY `\e[?2026h`/`\e[?2026l` to keep the parser
        // more simple.
        let mut bsu_offset = None;
        for index in memchr::memchr_iter(0x1B, search_buffer).rev() {
            let offset = start_offset + index;
            let escape = &self.state.sync_state.buffer[offset..offset + SYNC_ESCAPE_LEN];

            if escape == BSU_CSI {
                self.state.sync_state.timeout.set_timeout(SYNC_UPDATE_TIMEOUT);
                bsu_offset = Some(offset);
            } else if escape == ESU_CSI {
                self.stop_sync_internal(handler, bsu_offset);
                break;
            }
        }
    }
}

/// Helper type that implements `crate::Perform`.
///
/// Processor creates a Performer when running advance and passes the Performer
/// to `crate::Parser`.
struct Performer<'a, H: Handler, T: Timeout> {
    state: &'a mut ProcessorState<T>,
    handler: &'a mut H,

    /// Whether the parser should be prematurely terminated.
    terminated: bool,
}

impl<'a, H: Handler + 'a, T: Timeout> Performer<'a, H, T> {
    /// Create a performer.
    #[inline]
    pub fn new(state: &'a mut ProcessorState<T>, handler: &'a mut H) -> Performer<'a, H, T> {
        Performer { state, handler, terminated: Default::default() }
    }
}

/// The default [`Timeout`]: records the synchronized-update deadline as an [`Instant`].
///
/// It never checks the clock on its own; [`Self::sync_timeout`] exposes the deadline for the
/// caller to enforce.
#[derive(Debug, Default)]
pub struct StdSyncHandler {
    timeout: Option<Instant>,
}

impl StdSyncHandler {
    /// Returns the pending synchronized update's deadline, or `None` if none is pending.
    #[inline]
    #[must_use]
    pub fn sync_timeout(&self) -> Option<Instant> {
        self.timeout
    }
}

impl Timeout for StdSyncHandler {
    #[inline]
    fn set_timeout(&mut self, duration: Duration) {
        self.timeout = Some(Instant::now() + duration);
    }

    #[inline]
    fn clear_timeout(&mut self) {
        self.timeout = None;
    }

    #[inline]
    fn pending_timeout(&self) -> bool {
        self.timeout.is_some()
    }
}

/// Tracks whether a synchronized update is pending, and its deadline.
///
/// The [`Processor`] calls [`Self::set_timeout`] when a synchronized update begins or is
/// extended and [`Self::clear_timeout`] when it ends. It never checks whether the deadline has
/// passed: the caller does, and calls [`Processor::stop_sync`] (see the
/// [module documentation](crate::ansi#synchronized-updates)).
pub trait Timeout: Default {
    /// Records that the pending synchronized update must end `duration` from now.
    ///
    /// Called at the start of an update and again whenever a further BSU extends it.
    fn set_timeout(&mut self, duration: Duration);

    /// Records that no synchronized update is pending.
    fn clear_timeout(&mut self);

    /// Returns whether a synchronized update is pending.
    ///
    /// This must stay `true` from [`Self::set_timeout`] until [`Self::clear_timeout`], even
    /// after the deadline has passed. While it is `true`, a buffered [`Processor::advance`]
    /// holds input instead of dispatching it; returning `false` early would let new output
    /// overtake the bytes still held.
    fn pending_timeout(&self) -> bool;
}

/// Receives the terminal operations that a [`Processor`] decodes.
///
/// Each method corresponds to a control character, escape sequence, or OSC command. Every
/// method has a no-op default, so an implementation overrides only what it supports.
/// Positions and sizes are in character cells.
pub trait Handler {
    /// Sets the window title to `title` (OSC 0 and OSC 2); `None` restores the default title.
    fn set_title(&mut self, _title: Option<String>) {}

    /// Sets the cursor style (DECSCUSR); `None` restores the default style.
    fn set_cursor_style(&mut self, _style: Option<CursorStyle>) {}

    /// Set the cursor shape.
    fn set_cursor_shape(&mut self, _shape: CursorShape) {}

    /// A character to be displayed.
    fn input(&mut self, _c: char) {}

    /// Display a borrowed printable run, preserving scalar input semantics.
    fn input_str(&mut self, text: &str) {
        for c in text.chars() {
            self.input(c);
        }
    }

    /// Display `c` exactly `count` times, using the current attributes and character set.
    ///
    /// The processor normalizes REP's omitted or zero parameter to one. This method must have
    /// the same effect as repeated calls to [`Self::input`], including wrapping and scrolling.
    fn repeat_char(&mut self, c: char, count: usize) {
        for _ in 0..count {
            self.input(c);
        }
    }

    /// Moves the cursor to zero-based `line` and `col` (CUP, HVP).
    fn goto(&mut self, _line: i32, _col: usize) {}

    /// Moves the cursor to zero-based `line`, keeping its column (VPA).
    fn goto_line(&mut self, _line: i32) {}

    /// Moves the cursor to zero-based `col`, keeping its line (CHA, HPA).
    fn goto_col(&mut self, _col: usize) {}

    /// Inserts `count` blank cells at the cursor, shifting the rest of the line right (ICH).
    fn insert_blank(&mut self, _count: usize) {}

    /// Moves the cursor up `rows` lines (CUU).
    fn move_up(&mut self, _rows: usize) {}

    /// Moves the cursor down `rows` lines (CUD, VPR).
    fn move_down(&mut self, _rows: usize) {}

    /// Identify the terminal (should write back to the pty stream).
    fn identify_terminal(&mut self, _intermediate: Option<char>) {}

    /// Answers a device status report request (DSR): `kind` 5 asks for the terminal's status
    /// and 6 for the cursor position.
    fn device_status(&mut self, _kind: usize) {}

    /// Moves the cursor right `cols` columns (CUF, HPR).
    fn move_forward(&mut self, _cols: usize) {}

    /// Moves the cursor left `cols` columns (CUB).
    fn move_backward(&mut self, _cols: usize) {}

    /// Moves the cursor down `rows` lines and to the first column (CNL).
    fn move_down_and_cr(&mut self, _rows: usize) {}

    /// Moves the cursor up `rows` lines and to the first column (CPL).
    fn move_up_and_cr(&mut self, _rows: usize) {}

    /// Put `count` tabs.
    fn put_tab(&mut self, _count: u16) {}

    /// Moves the cursor one column left (BS).
    fn backspace(&mut self) {}

    /// Carriage return.
    fn carriage_return(&mut self) {}

    /// Linefeed.
    fn linefeed(&mut self) {}

    /// Ring the bell.
    ///
    /// Hopefully this is never implemented.
    fn bell(&mut self) {}

    /// Substitute char under cursor.
    fn substitute(&mut self) {}

    /// Newline.
    fn newline(&mut self) {}

    /// Set current position as a tabstop.
    fn set_horizontal_tabstop(&mut self) {}

    /// Scrolls the scrolling region up `rows` lines (SU).
    fn scroll_up(&mut self, _rows: usize) {}

    /// Scrolls the scrolling region down `rows` lines (SD).
    fn scroll_down(&mut self, _rows: usize) {}

    /// Inserts `count` blank lines at the cursor (IL).
    fn insert_blank_lines(&mut self, _count: usize) {}

    /// Deletes `count` lines at the cursor (DL).
    fn delete_lines(&mut self, _count: usize) {}

    /// Erases `count` cells starting at the cursor (ECH).
    ///
    /// Erase means resetting to the default state (default colors, no content,
    /// no mode flags).
    fn erase_chars(&mut self, _count: usize) {}

    /// Deletes `count` cells at the cursor (DCH).
    ///
    /// Deleting a character is like the delete key on the keyboard - everything
    /// to the right of the deleted things is shifted left.
    fn delete_chars(&mut self, _count: usize) {}

    /// Move backward `count` tabs.
    fn move_backward_tabs(&mut self, _count: u16) {}

    /// Move forward `count` tabs.
    fn move_forward_tabs(&mut self, _count: u16) {}

    /// Save current cursor position.
    fn save_cursor_position(&mut self) {}

    /// Restore cursor position.
    fn restore_cursor_position(&mut self) {}

    /// Clear current line.
    fn clear_line(&mut self, _mode: LineClearMode) {}

    /// Clear screen.
    fn clear_screen(&mut self, _mode: ClearMode) {}

    /// Clear tab stops.
    fn clear_tabs(&mut self, _mode: TabulationClearMode) {}

    /// Set tab stops at every `interval`.
    fn set_tabs(&mut self, _interval: u16) {}

    /// Reset terminal state.
    fn reset_state(&mut self) {}

    /// Reverse Index.
    ///
    /// Move the active position to the same horizontal position on the
    /// preceding line. If the active position is at the top margin, a scroll
    /// down is performed.
    fn reverse_index(&mut self) {}

    /// Set a terminal attribute.
    fn terminal_attribute(&mut self, _attr: Attr) {}

    /// The attributes of one SGR sequence follow, through [`Handler::terminal_attribute`], until
    /// [`Handler::finish_attributes`].
    ///
    /// Lets a handler apply a sequence's attributes together, for example to resolve the
    /// resulting style once rather than after each attribute.
    fn start_attributes(&mut self) {}

    /// The attributes announced by [`Handler::start_attributes`] are complete.
    fn finish_attributes(&mut self) {}

    /// Set mode.
    fn set_mode(&mut self, _mode: Mode) {}

    /// Unset mode.
    fn unset_mode(&mut self, _mode: Mode) {}

    /// DECRPM - report mode.
    fn report_mode(&mut self, _mode: Mode) {}

    /// Set private mode.
    fn set_private_mode(&mut self, _mode: PrivateMode) {}

    /// Unset private mode.
    fn unset_private_mode(&mut self, _mode: PrivateMode) {}

    /// DECRPM - report private mode.
    fn report_private_mode(&mut self, _mode: PrivateMode) {}

    /// Sets the scrolling region (DECSTBM) to one-based lines `top` through `bottom`.
    ///
    /// `bottom` is `None` when the sequence omits it or gives 0, meaning the last line.
    fn set_scrolling_region(&mut self, _top: usize, _bottom: Option<usize>) {}

    /// DECKPAM - Set keypad to applications mode (escape sequences instead of digits).
    fn set_keypad_application_mode(&mut self) {}

    /// DECKPNM - Set keypad to numeric mode (digits instead of escape sequences).
    fn unset_keypad_application_mode(&mut self) {}

    /// Set one of the graphic character sets, G0 to G3, as the active charset.
    ///
    /// 'Invoke' one of G0 to G3 in the GL area. Also referred to as shift in,
    /// shift out and locking shift depending on the set being activated.
    fn set_active_charset(&mut self, _index: CharsetIndex) {}

    /// Designates `charset` as the graphic character set `index` (G0 to G3).
    ///
    /// A designated set takes effect once it is invoked with
    /// [`Self::set_active_charset`].
    fn configure_charset(&mut self, _index: CharsetIndex, _charset: StandardCharset) {}

    /// Sets color slot `index` to `color`.
    ///
    /// Slots 0 to 255 are the indexed palette (OSC 4). The dynamic colors (OSC 10 to 12) use the
    /// slots [`NamedColor::Foreground`], [`NamedColor::Background`], and
    /// [`NamedColor::Cursor`], cast to `usize`.
    fn set_color(&mut self, _index: usize, _color: Rgb) {}

    /// Answers a color query (OSC 4, 10, 11, or 12 with `?`) for color slot `index`.
    ///
    /// Slots are numbered as in [`Self::set_color`]. The reply starts with `OSC {prefix};`,
    /// where `prefix` is the query's own prefix (such as `4;1` or `10`), continues with the color,
    /// conventionally as `rgb:rrrr/gggg/bbbb`, and ends with `terminator`, which matches the
    /// query's terminator (BEL or `ESC \`).
    fn dynamic_color_sequence(&mut self, _prefix: String, _index: usize, _terminator: &str) {}

    /// Resets color slot `index`, numbered as in [`Self::set_color`], to its configured value
    /// (OSC 104, 110, 111, and 112).
    fn reset_color(&mut self, _index: usize) {}

    /// Stores `base64`, an OSC 52 payload exactly as sent, in the selection `clipboard`.
    ///
    /// `clipboard` is the first byte of the selection parameter, or `b'c'` when that parameter
    /// is empty; xterm uses `c` for the clipboard and `p` or `s` for the primary selection. The
    /// payload is not decoded.
    fn clipboard_store(&mut self, _clipboard: u8, _base64: &[u8]) {}

    /// Answers a clipboard query (OSC 52 with `?`) for the selection `clipboard`.
    ///
    /// `clipboard` is chosen as for [`Self::clipboard_store`]. The reply should end with
    /// `terminator`, which matches the query's terminator (BEL or `ESC \`).
    fn clipboard_load(&mut self, _clipboard: u8, _terminator: &str) {}

    /// Run the decaln routine.
    fn decaln(&mut self) {}

    /// Push a title onto the stack.
    fn push_title(&mut self) {}

    /// Pop the last title from the stack.
    fn pop_title(&mut self) {}

    /// Report text area size in pixels.
    fn text_area_size_pixels(&mut self) {}

    /// Report text area size in characters.
    fn text_area_size_chars(&mut self) {}

    /// Report the size of one character cell in pixels.
    fn text_area_cell_size_pixels(&mut self) {}

    /// An application program command (`ESC _`) has started.
    fn apc_start(&mut self) {}

    /// Deliver the next run of bytes of the current application program command.
    ///
    /// Runs are split wherever the input was split and carry no framing of their own.
    fn apc_put(&mut self, _bytes: &[u8]) {}

    /// The current application program command has ended.
    ///
    /// `cancelled` is `true` unless the `ESC \` string terminator closed it; what was delivered
    /// of a cancelled command is incomplete.
    fn apc_end(&mut self, _cancelled: bool) {}

    /// Starts the hyperlink `link` for the text that follows (OSC 8); `None` ends it.
    fn set_hyperlink(&mut self, _link: Option<Hyperlink>) {}

    /// Sets the mouse pointer shape (OSC 22).
    fn set_mouse_cursor_icon(&mut self, _icon: CursorIcon) {}

    /// Report current keyboard mode.
    fn report_keyboard_mode(&mut self) {}

    /// Push keyboard mode into the keyboard mode stack.
    fn push_keyboard_mode(&mut self, _mode: KeyboardModes) {}

    /// Pop the given amount of keyboard modes from the
    /// keyboard mode stack.
    fn pop_keyboard_modes(&mut self, _to_pop: u16) {}

    /// Set the [`keyboard mode`] using the given [`behavior`].
    ///
    /// [`keyboard mode`]: crate::ansi::KeyboardModes
    /// [`behavior`]: crate::ansi::KeyboardModesApplyBehavior
    fn set_keyboard_mode(&mut self, _mode: KeyboardModes, _behavior: KeyboardModesApplyBehavior) {}
}

bitflags! {
    /// A set of [`kitty keyboard protocol'] modes.
    ///
    /// [`kitty keyboard protocol']: https://sw.kovidgoyal.net/kitty/keyboard-protocol
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct KeyboardModes : u8 {
        /// No keyboard protocol mode is set.
        const NO_MODE                 = 0b0000_0000;
        /// Report `Esc`, `alt` + `key`, `ctrl` + `key`, `ctrl` + `alt` + `key`, `shift`
        /// + `alt` + `key` keys using `CSI u` sequence instead of raw ones.
        const DISAMBIGUATE_ESC_CODES  = 0b0000_0001;
        /// Report key presses, release, and repetition alongside the escape. Key events
        /// that result in text are reported as plain UTF-8, unless the
        /// [`Self::REPORT_ALL_KEYS_AS_ESC`] is enabled.
        const REPORT_EVENT_TYPES      = 0b0000_0010;
        /// Additionally report shifted key and base layout key.
        const REPORT_ALTERNATE_KEYS   = 0b0000_0100;
        /// Report every key as an escape sequence.
        const REPORT_ALL_KEYS_AS_ESC  = 0b0000_1000;
        /// Report the text generated by the key event.
        const REPORT_ASSOCIATED_TEXT  = 0b0001_0000;
    }
}

/// Describes how the new [`KeyboardModes`] should be applied.
#[repr(u8)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardModesApplyBehavior {
    /// Replace the active flags with the new ones.
    #[default]
    Replace,
    /// Merge the given flags with currently active ones.
    Union,
    /// Remove the given flags from the active ones.
    Difference,
}

/// Terminal cursor configuration.
#[derive(Default, Debug, Eq, PartialEq, Copy, Clone, Hash)]
pub struct CursorStyle {
    /// The cursor's shape.
    pub shape: CursorShape,
    /// Whether the cursor blinks.
    pub blinking: bool,
}

/// Terminal cursor shape.
#[derive(Debug, Default, Eq, PartialEq, Copy, Clone, Hash)]
pub enum CursorShape {
    /// Cursor is a block like `▒`.
    #[default]
    Block,

    /// Cursor is an underscore like `_`.
    Underline,

    /// Cursor is a vertical bar `⎸`.
    Beam,

    /// Cursor is a box like `☐`.
    HollowBlock,

    /// Invisible cursor.
    Hidden,
}

/// Wrapper for the ANSI modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Mode {
    /// Known ANSI mode.
    Named(NamedMode),
    /// Unidentified public mode.
    Unknown(u16),
}

impl Mode {
    fn new(mode: u16) -> Self {
        match mode {
            4 => Self::Named(NamedMode::Insert),
            20 => Self::Named(NamedMode::LineFeedNewLine),
            _ => Self::Unknown(mode),
        }
    }

    /// Get the raw value of the mode.
    #[must_use]
    pub fn raw(self) -> u16 {
        match self {
            Self::Named(named) => named as u16,
            Self::Unknown(mode) => mode,
        }
    }
}

impl From<NamedMode> for Mode {
    fn from(value: NamedMode) -> Self {
        Self::Named(value)
    }
}

/// ANSI modes.
#[repr(u16)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NamedMode {
    /// IRM Insert Mode.
    Insert = 4,
    /// LNM Line Feed/New Line Mode: line feeds also return the cursor to the first column.
    LineFeedNewLine = 20,
}

/// Wrapper for the private DEC modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PrivateMode {
    /// Known private mode.
    Named(NamedPrivateMode),
    /// Unknown private mode.
    Unknown(u16),
}

impl PrivateMode {
    fn new(mode: u16) -> Self {
        match mode {
            1 => Self::Named(NamedPrivateMode::CursorKeys),
            3 => Self::Named(NamedPrivateMode::ColumnMode),
            6 => Self::Named(NamedPrivateMode::Origin),
            7 => Self::Named(NamedPrivateMode::LineWrap),
            12 => Self::Named(NamedPrivateMode::BlinkingCursor),
            25 => Self::Named(NamedPrivateMode::ShowCursor),
            1000 => Self::Named(NamedPrivateMode::ReportMouseClicks),
            1002 => Self::Named(NamedPrivateMode::ReportCellMouseMotion),
            1003 => Self::Named(NamedPrivateMode::ReportAllMouseMotion),
            1004 => Self::Named(NamedPrivateMode::ReportFocusInOut),
            1005 => Self::Named(NamedPrivateMode::Utf8Mouse),
            1006 => Self::Named(NamedPrivateMode::SgrMouse),
            1007 => Self::Named(NamedPrivateMode::AlternateScroll),
            1042 => Self::Named(NamedPrivateMode::UrgencyHints),
            1049 => Self::Named(NamedPrivateMode::SwapScreenAndSetRestoreCursor),
            2004 => Self::Named(NamedPrivateMode::BracketedPaste),
            2026 => Self::Named(NamedPrivateMode::SyncUpdate),
            _ => Self::Unknown(mode),
        }
    }

    /// Get the raw value of the mode.
    #[must_use]
    pub fn raw(self) -> u16 {
        match self {
            Self::Named(named) => named as u16,
            Self::Unknown(mode) => mode,
        }
    }
}

impl From<NamedPrivateMode> for PrivateMode {
    fn from(value: NamedPrivateMode) -> Self {
        Self::Named(value)
    }
}

/// Private DEC modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NamedPrivateMode {
    /// Cursor keys send application sequences instead of ANSI ones (DECCKM).
    CursorKeys = 1,
    /// Select 80 or 132 columns per page (DECCOLM).
    ///
    /// CSI ? 3 h -> set 132 column font.
    /// CSI ? 3 l -> reset 80 column font.
    ///
    /// Additionally,
    ///
    /// * set margins to default positions
    /// * erases all data in page memory
    /// * resets DECLRMM to unavailable
    /// * clears data from the status line (if set to host-writable)
    ColumnMode = 3,
    /// Cursor positions are relative to the scrolling region (DECOM).
    Origin = 6,
    /// Text wraps to the next line at the right margin (DECAWM).
    LineWrap = 7,
    /// The cursor blinks.
    BlinkingCursor = 12,
    /// The cursor is visible (DECTCEM).
    ShowCursor = 25,
    /// Report mouse button presses and releases.
    ReportMouseClicks = 1000,
    /// Also report mouse motion while a button is held.
    ReportCellMouseMotion = 1002,
    /// Report all mouse motion.
    ReportAllMouseMotion = 1003,
    /// Report when the terminal gains or loses focus.
    ReportFocusInOut = 1004,
    /// Encode mouse reports as UTF-8.
    Utf8Mouse = 1005,
    /// Encode mouse reports in the SGR format.
    SgrMouse = 1006,
    /// In the alternate screen, the mouse wheel sends cursor keys.
    AlternateScroll = 1007,
    /// The bell sets the window's urgency hint.
    UrgencyHints = 1042,
    /// Switch to the alternate screen, saving and restoring the cursor.
    SwapScreenAndSetRestoreCursor = 1049,
    /// Pasted text is wrapped in bracketed-paste markers.
    BracketedPaste = 2004,
    /// Synchronized updates; the [`Processor`] handles this mode itself.
    SyncUpdate = 2026,
}

/// Mode for clearing line.
///
/// Relative to cursor.
#[derive(Debug)]
pub enum LineClearMode {
    /// Clear right of cursor.
    Right,
    /// Clear left of cursor.
    Left,
    /// Clear entire line.
    All,
}

/// Mode for clearing terminal.
///
/// Relative to cursor.
#[derive(Debug)]
pub enum ClearMode {
    /// Clear below cursor.
    Below,
    /// Clear above cursor.
    Above,
    /// Clear entire terminal.
    All,
    /// Clear 'saved' lines (scrollback).
    Saved,
}

/// Mode for clearing tab stops.
#[derive(Debug)]
pub enum TabulationClearMode {
    /// Clear stop under cursor.
    Current,
    /// Clear all stops.
    All,
}

/// Standard colors.
///
/// The order here matters since the enum should be castable to a `usize` for
/// indexing a color list.
#[derive(Debug, Copy, Clone, Eq, PartialEq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum NamedColor {
    /// Black.
    Black = 0,
    /// Red.
    Red,
    /// Green.
    Green,
    /// Yellow.
    Yellow,
    /// Blue.
    Blue,
    /// Magenta.
    Magenta,
    /// Cyan.
    Cyan,
    /// White.
    White,
    /// Bright black.
    BrightBlack,
    /// Bright red.
    BrightRed,
    /// Bright green.
    BrightGreen,
    /// Bright yellow.
    BrightYellow,
    /// Bright blue.
    BrightBlue,
    /// Bright magenta.
    BrightMagenta,
    /// Bright cyan.
    BrightCyan,
    /// Bright white.
    BrightWhite,
    /// The foreground color.
    Foreground = 256,
    /// The background color.
    Background,
    /// Color for the cursor itself.
    Cursor,
    /// Dim black.
    DimBlack,
    /// Dim red.
    DimRed,
    /// Dim green.
    DimGreen,
    /// Dim yellow.
    DimYellow,
    /// Dim blue.
    DimBlue,
    /// Dim magenta.
    DimMagenta,
    /// Dim cyan.
    DimCyan,
    /// Dim white.
    DimWhite,
    /// The bright foreground color.
    BrightForeground,
    /// Dim foreground.
    DimForeground,
}

impl NamedColor {
    /// Returns the next brighter variant: dim colors become normal, normal colors bright.
    ///
    /// Bright colors and colors without variants are returned unchanged.
    #[must_use]
    pub fn to_bright(self) -> Self {
        match self {
            NamedColor::Foreground => NamedColor::BrightForeground,
            NamedColor::Black => NamedColor::BrightBlack,
            NamedColor::Red => NamedColor::BrightRed,
            NamedColor::Green => NamedColor::BrightGreen,
            NamedColor::Yellow => NamedColor::BrightYellow,
            NamedColor::Blue => NamedColor::BrightBlue,
            NamedColor::Magenta => NamedColor::BrightMagenta,
            NamedColor::Cyan => NamedColor::BrightCyan,
            NamedColor::White => NamedColor::BrightWhite,
            NamedColor::DimForeground => NamedColor::Foreground,
            NamedColor::DimBlack => NamedColor::Black,
            NamedColor::DimRed => NamedColor::Red,
            NamedColor::DimGreen => NamedColor::Green,
            NamedColor::DimYellow => NamedColor::Yellow,
            NamedColor::DimBlue => NamedColor::Blue,
            NamedColor::DimMagenta => NamedColor::Magenta,
            NamedColor::DimCyan => NamedColor::Cyan,
            NamedColor::DimWhite => NamedColor::White,
            val => val,
        }
    }

    /// Returns the next dimmer variant: bright colors become normal, normal colors dim.
    ///
    /// Dim colors and colors without variants are returned unchanged.
    #[must_use]
    pub fn to_dim(self) -> Self {
        match self {
            NamedColor::Black => NamedColor::DimBlack,
            NamedColor::Red => NamedColor::DimRed,
            NamedColor::Green => NamedColor::DimGreen,
            NamedColor::Yellow => NamedColor::DimYellow,
            NamedColor::Blue => NamedColor::DimBlue,
            NamedColor::Magenta => NamedColor::DimMagenta,
            NamedColor::Cyan => NamedColor::DimCyan,
            NamedColor::White => NamedColor::DimWhite,
            NamedColor::Foreground => NamedColor::DimForeground,
            NamedColor::BrightBlack => NamedColor::Black,
            NamedColor::BrightRed => NamedColor::Red,
            NamedColor::BrightGreen => NamedColor::Green,
            NamedColor::BrightYellow => NamedColor::Yellow,
            NamedColor::BrightBlue => NamedColor::Blue,
            NamedColor::BrightMagenta => NamedColor::Magenta,
            NamedColor::BrightCyan => NamedColor::Cyan,
            NamedColor::BrightWhite => NamedColor::White,
            NamedColor::BrightForeground => NamedColor::Foreground,
            val => val,
        }
    }
}

/// A color as set by SGR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Color {
    /// One of the terminal's named colors.
    Named(NamedColor),
    /// A direct 24-bit color.
    Spec(Rgb),
    /// An entry of the 256-color palette.
    Indexed(u8),
}

/// Terminal character attributes.
#[derive(Debug, Eq, PartialEq)]
pub enum Attr {
    /// Clear all special abilities.
    Reset,
    /// Bold text.
    Bold,
    /// Dim or secondary color.
    Dim,
    /// Italic text.
    Italic,
    /// Underline text.
    Underline,
    /// Underlined twice.
    DoubleUnderline,
    /// Undercurled text.
    Undercurl,
    /// Dotted underlined text.
    DottedUnderline,
    /// Dashed underlined text.
    DashedUnderline,
    /// Blink cursor slowly.
    BlinkSlow,
    /// Blink cursor fast.
    BlinkFast,
    /// Invert colors.
    Reverse,
    /// Do not display characters.
    Hidden,
    /// Strikeout text.
    Strike,
    /// Cancel bold.
    CancelBold,
    /// Cancel bold and dim.
    CancelBoldDim,
    /// Cancel italic.
    CancelItalic,
    /// Cancel all underlines.
    CancelUnderline,
    /// Cancel blink.
    CancelBlink,
    /// Cancel inversion.
    CancelReverse,
    /// Cancel text hiding.
    CancelHidden,
    /// Cancel strikeout.
    CancelStrike,
    /// Set indexed foreground color.
    Foreground(Color),
    /// Set indexed background color.
    Background(Color),
    /// Underline color.
    UnderlineColor(Option<Color>),
}

/// Identifiers which can be assigned to a graphic character set.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CharsetIndex {
    /// Default set, is designated as ASCII at startup.
    #[default]
    G0,
    /// The G1 set, invoked by shift out (SO).
    G1,
    /// The G2 set.
    G2,
    /// The G3 set.
    G3,
}

/// Standard or common character sets which can be designated as G0-G3.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum StandardCharset {
    /// US ASCII.
    #[default]
    Ascii,
    /// The DEC Special Character and Line Drawing Set.
    SpecialCharacterAndLineDrawing,
}

impl StandardCharset {
    /// Switch/Map character to the active charset. Ascii is the common case and
    /// for that we want to do as little as possible.
    #[inline]
    #[must_use]
    pub fn map(self, c: char) -> char {
        match self {
            StandardCharset::Ascii => c,
            StandardCharset::SpecialCharacterAndLineDrawing => match c {
                '_' => ' ',
                '`' => '◆',
                'a' => '▒',
                'b' => '\u{2409}', // Symbol for horizontal tabulation
                'c' => '\u{240c}', // Symbol for form feed
                'd' => '\u{240d}', // Symbol for carriage return
                'e' => '\u{240a}', // Symbol for line feed
                'f' => '°',
                'g' => '±',
                'h' => '\u{2424}', // Symbol for newline
                'i' => '\u{240b}', // Symbol for vertical tabulation
                'j' => '┘',
                'k' => '┐',
                'l' => '┌',
                'm' => '└',
                'n' => '┼',
                'o' => '⎺',
                'p' => '⎻',
                'q' => '─',
                'r' => '⎼',
                's' => '⎽',
                't' => '├',
                'u' => '┤',
                'v' => '┴',
                'w' => '┬',
                'x' => '│',
                'y' => '≤',
                'z' => '≥',
                '{' => 'π',
                '|' => '≠',
                '}' => '£',
                '~' => '·',
                _ => c,
            },
        }
    }
}

impl<'a, H, T> crate::Perform for Performer<'a, H, T>
where
    H: Handler + 'a,
    T: Timeout,
{
    #[inline]
    fn print(&mut self, c: char) {
        self.handler.input(c);
        self.state.preceding_char = Some(c);
    }

    fn print_str(&mut self, text: &str) {
        self.handler.input_str(text);
        if let Some(c) = text.chars().next_back() {
            self.state.preceding_char = Some(c);
        }
    }

    fn osc_capture(&mut self, command: &[u8]) -> bool {
        log::log_enabled!(log::Level::Debug)
            || matches!(
                command,
                b"0" | b"2"
                    | b"4"
                    | b"8"
                    | b"10"
                    | b"11"
                    | b"12"
                    | b"22"
                    | b"50"
                    | b"52"
                    | b"104"
                    | b"110"
                    | b"111"
                    | b"112"
            )
    }

    #[inline]
    fn execute(&mut self, byte: u8) {
        match byte {
            C0::HT => self.handler.put_tab(1),
            C0::BS => self.handler.backspace(),
            C0::CR => self.handler.carriage_return(),
            C0::LF | C0::VT | C0::FF => self.handler.linefeed(),
            C0::BEL => self.handler.bell(),
            C0::SUB => self.handler.substitute(),
            C0::SI => self.handler.set_active_charset(CharsetIndex::G0),
            C0::SO => self.handler.set_active_charset(CharsetIndex::G1),
            _ => debug!(
                event = "vvte.execute.unhandled",
                byte;
                "unhandled control byte {byte:#04x}"
            ),
        }
    }

    // DCS strings are not handled. Their data bytes, passed to `put` one at a time, are dropped
    // without logging: a single image can be megabytes.

    #[inline]
    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        debug!(
            event = "vvte.dcs.unhandled",
            params:?,
            intermediates:% = intermediates.escape_ascii(),
            ignore,
            action:?;
            "unhandled DCS {}{action}",
            intermediates.escape_ascii()
        );
    }

    #[inline]
    fn unhook(&mut self) {
        debug!(event = "vvte.dcs.end"; "end of unhandled DCS");
    }

    #[inline]
    fn apc_start(&mut self) {
        self.handler.apc_start();
    }

    #[inline]
    fn apc_put(&mut self, bytes: &[u8]) {
        self.handler.apc_put(bytes);
    }

    #[inline]
    fn apc_end(&mut self, cancelled: bool) {
        self.handler.apc_end(cancelled);
    }

    #[inline]
    #[expect(clippy::too_many_lines, reason = "one arm per OSC command, like the dispatch table")]
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // The parameters can carry user data, such as clipboard contents or paths, so they go in
        // a field, which loggers can redact, and only the command number goes in the message.
        // Both are formatted only if the record is written.
        #[cold]
        #[inline(never)]
        fn unhandled(params: &[&[u8]]) {
            let command = params.first().copied().unwrap_or_default();
            debug!(
                event = "vvte.osc.unhandled",
                params:% = OscParams(params);
                "unhandled OSC {}",
                command.escape_ascii()
            );
        }

        let terminator = if bell_terminated { "\x07" } else { "\x1b\\" };

        if params.is_empty() || params[0].is_empty() {
            return;
        }

        match params[0] {
            // Set window title.
            b"0" | b"2" => {
                if params.len() >= 2 {
                    // NOTE: `;` is a separator, so a title containing one arrives split
                    // across parameters and has to be rebuilt. Parameters that are not
                    // valid UTF-8 are dropped, but still separate their neighbours.
                    let mut title = String::new();
                    for (i, part) in
                        params[1..].iter().filter_map(|x| str::from_utf8(x).ok()).enumerate()
                    {
                        if i != 0 {
                            title.push(';');
                        }
                        title.push_str(part);
                    }

                    // Trim in place to avoid re-allocating the whole title.
                    title.truncate(title.trim_end().len());
                    let leading = title.len() - title.trim_start().len();
                    title.drain(..leading);

                    self.handler.set_title(Some(title));
                    return;
                }
                unhandled(params);
            },

            // Set color index.
            b"4" => {
                if params.len() <= 1 || params.len().is_multiple_of(2) {
                    unhandled(params);
                    return;
                }

                for chunk in params[1..].chunks(2) {
                    let Some(index) = parse_number(chunk[0]) else {
                        unhandled(params);
                        continue;
                    };

                    if let Some(c) = xparse_color(chunk[1]) {
                        self.handler.set_color(index as usize, c);
                    } else if chunk[1] == b"?" {
                        let prefix = format!("4;{index}");
                        self.handler.dynamic_color_sequence(prefix, index as usize, terminator);
                    } else {
                        unhandled(params);
                    }
                }
            },

            // Hyperlink.
            b"8" if params.len() > 2 => {
                let link_params = params[1];

                // NOTE: The escape sequence is of form 'OSC 8 ; params ; URI ST', where
                // URI is URL-encoded. However `;` is a special character and might be
                // passed as is, thus we need to rebuild the URI.
                let capacity = params[2..].iter().map(|param| param.len() + 1).sum::<usize>() - 1;
                let mut uri = String::with_capacity(capacity);
                uri.push_str(str::from_utf8(params[2]).unwrap_or_default());
                for param in &params[3..] {
                    uri.push(';');
                    uri.push_str(str::from_utf8(param).unwrap_or_default());
                }

                // The OSC 8 escape sequence must be stopped when getting an empty `uri`.
                if uri.is_empty() {
                    self.handler.set_hyperlink(None);
                    return;
                }

                // Link parameters are in format of `key1=value1:key2=value2`. Currently only
                // key `id` is defined.
                let id = link_params
                    .split(|&b| b == b':')
                    .find_map(|kv| kv.strip_prefix(b"id="))
                    .and_then(|kv| str::from_utf8(kv).ok().map(str::to_owned));

                self.handler.set_hyperlink(Some(Hyperlink { id, uri }));
            },

            // Get/set Foreground, Background, Cursor colors.
            b"10" | b"11" | b"12" => {
                if params.len() >= 2
                    && let Some(mut dynamic_code) = parse_number(params[0])
                {
                    for param in &params[1..] {
                        // 10 is the first dynamic color, also the foreground.
                        let offset = dynamic_code as usize - 10;
                        let index = NamedColor::Foreground as usize + offset;

                        // End of setting dynamic colors.
                        if index > NamedColor::Cursor as usize {
                            unhandled(params);
                            break;
                        }

                        if let Some(color) = xparse_color(param) {
                            self.handler.set_color(index, color);
                        } else if param == b"?" {
                            self.handler.dynamic_color_sequence(
                                dynamic_code.to_string(),
                                index,
                                terminator,
                            );
                        } else {
                            unhandled(params);
                        }
                        dynamic_code += 1;
                    }
                    return;
                }
                unhandled(params);
            },

            // Set mouse cursor shape.
            b"22" if params.len() == 2 => {
                let shape = String::from_utf8_lossy(params[1]);
                match CursorIcon::from_str(&shape) {
                    Ok(cursor_icon) => self.handler.set_mouse_cursor_icon(cursor_icon),
                    Err(_) => debug!(
                        event = "vvte.osc22.unknown_shape",
                        shape:%;
                        "unrecognized OSC 22 cursor shape"
                    ),
                }
            },

            // Set cursor style.
            b"50" => {
                if params.len() >= 2
                    && params[1].len() >= 13
                    && params[1][0..12] == *b"CursorShape="
                {
                    let shape = match params[1][12] as char {
                        '0' => CursorShape::Block,
                        '1' => CursorShape::Beam,
                        '2' => CursorShape::Underline,
                        _ => return unhandled(params),
                    };
                    self.handler.set_cursor_shape(shape);
                    return;
                }
                unhandled(params);
            },

            // Set clipboard.
            b"52" => {
                if params.len() < 3 {
                    return unhandled(params);
                }

                let clipboard = params[1].first().unwrap_or(&b'c');
                match params[2] {
                    b"?" => self.handler.clipboard_load(*clipboard, terminator),
                    base64 => self.handler.clipboard_store(*clipboard, base64),
                }
            },

            // Reset color index.
            b"104" => {
                // Reset all color indexes when no parameters are given.
                if params.len() == 1 || params[1].is_empty() {
                    for i in 0..256 {
                        self.handler.reset_color(i);
                    }
                    return;
                }

                // Reset color indexes given as parameters.
                for param in &params[1..] {
                    match parse_number(param) {
                        Some(index) => self.handler.reset_color(index as usize),
                        None => unhandled(params),
                    }
                }
            },

            // Reset foreground color.
            b"110" => self.handler.reset_color(NamedColor::Foreground as usize),

            // Reset background color.
            b"111" => self.handler.reset_color(NamedColor::Background as usize),

            // Reset text cursor color.
            b"112" => self.handler.reset_color(NamedColor::Cursor as usize),

            _ => unhandled(params),
        }
    }

    #[inline]
    #[expect(clippy::too_many_lines, reason = "one arm per CSI sequence, like the dispatch table")]
    fn csi_dispatch(
        &mut self,
        params: &Params,
        intermediates: &[u8],
        has_ignored_intermediates: bool,
        action: char,
    ) {
        macro_rules! unhandled {
            () => {{
                debug!(
                    event = "vvte.csi.unhandled",
                    params:?,
                    intermediates:% = intermediates.escape_ascii(),
                    action:?;
                    "unhandled CSI {:?}{}{}",
                    params,
                    intermediates.escape_ascii(),
                    action
                );
            }};
        }

        if has_ignored_intermediates || intermediates.len() > 2 {
            unhandled!();
            return;
        }

        let mut params_iter = params.iter();
        let handler = &mut *self.handler;

        let mut next_param_or = |default: u16| match params_iter.next() {
            Some(&[param, ..]) if param != 0 => param,
            _ => default,
        };

        match (action, intermediates) {
            ('@', []) => handler.insert_blank(next_param_or(1) as usize),
            ('A', []) => handler.move_up(next_param_or(1) as usize),
            ('B' | 'e', []) => handler.move_down(next_param_or(1) as usize),
            ('b', []) => {
                if let Some(c) = self.state.preceding_char {
                    handler.repeat_char(c, usize::from(next_param_or(1)));
                } else {
                    debug!(event = "vvte.rep.no_preceding_char"; "REP without a preceding character");
                }
            },
            ('C' | 'a', []) => handler.move_forward(next_param_or(1) as usize),
            ('c', intermediates) if next_param_or(0) == 0 => {
                handler.identify_terminal(intermediates.first().map(|&i| i as char));
            },
            ('D', []) => handler.move_backward(next_param_or(1) as usize),
            ('d', []) => handler.goto_line(i32::from(next_param_or(1)) - 1),
            ('E', []) => handler.move_down_and_cr(next_param_or(1) as usize),
            ('F', []) => handler.move_up_and_cr(next_param_or(1) as usize),
            ('G' | '`', []) => handler.goto_col(next_param_or(1) as usize - 1),
            ('W', [b'?']) if next_param_or(0) == 5 => handler.set_tabs(8),
            ('g', []) => {
                let mode = match next_param_or(0) {
                    0 => TabulationClearMode::Current,
                    3 => TabulationClearMode::All,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_tabs(mode);
            },
            ('H' | 'f', []) => {
                let y = i32::from(next_param_or(1));
                let x = next_param_or(1) as usize;
                handler.goto(y - 1, x - 1);
            },
            ('h', []) => {
                for param in params_iter.map(|param| param[0]) {
                    handler.set_mode(Mode::new(param));
                }
            },
            ('h', [b'?']) => {
                for param in params_iter.map(|param| param[0]) {
                    // Handle sync updates opaquely.
                    if param == NamedPrivateMode::SyncUpdate as u16 {
                        let sync = &mut self.state.sync_state;
                        // A buffered update holds bytes, so its size bounds it. An unbuffered
                        // one holds only presentation, so its first deadline must stand.
                        if sync.buffered || !sync.timeout.pending_timeout() {
                            sync.timeout.set_timeout(SYNC_UPDATE_TIMEOUT);
                        }
                        // Stop here so that `Processor::advance` starts holding bytes.
                        self.terminated |= sync.buffered;
                    }

                    handler.set_private_mode(PrivateMode::new(param));
                }
            },
            ('I', []) => handler.move_forward_tabs(next_param_or(1)),
            ('J', []) => {
                let mode = match next_param_or(0) {
                    0 => ClearMode::Below,
                    1 => ClearMode::Above,
                    2 => ClearMode::All,
                    3 => ClearMode::Saved,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_screen(mode);
            },
            ('K', []) => {
                let mode = match next_param_or(0) {
                    0 => LineClearMode::Right,
                    1 => LineClearMode::Left,
                    2 => LineClearMode::All,
                    _ => {
                        unhandled!();
                        return;
                    },
                };

                handler.clear_line(mode);
            },
            ('L', []) => handler.insert_blank_lines(next_param_or(1) as usize),
            ('l', []) => {
                for param in params_iter.map(|param| param[0]) {
                    handler.unset_mode(Mode::new(param));
                }
            },
            ('l', [b'?']) => {
                for param in params_iter.map(|param| param[0]) {
                    // A buffered update ends in `Processor::stop_sync_internal`, which may
                    // replay this ESU after a later BSU has already started the next update.
                    let sync = &mut self.state.sync_state;
                    if !sync.buffered && param == NamedPrivateMode::SyncUpdate as u16 {
                        sync.timeout.clear_timeout();
                    }
                    handler.unset_private_mode(PrivateMode::new(param));
                }
            },
            ('M', []) => handler.delete_lines(next_param_or(1) as usize),
            ('m', []) => {
                if params.is_empty() {
                    handler.terminal_attribute(Attr::Reset);
                } else {
                    handler.start_attributes();
                    attrs_from_sgr_parameters(handler, &mut params_iter);
                    handler.finish_attributes();
                }
            },
            ('n', []) => handler.device_status(next_param_or(0) as usize),
            ('P', []) => handler.delete_chars(next_param_or(1) as usize),
            ('p', [b'$']) => {
                let mode = next_param_or(0);
                handler.report_mode(Mode::new(mode));
            },
            ('p', [b'?', b'$']) => {
                let mode = next_param_or(0);
                handler.report_private_mode(PrivateMode::new(mode));
            },
            ('q', [b' ']) => {
                // DECSCUSR (CSI Ps SP q) -- Set Cursor Style.
                let cursor_style_id = next_param_or(0);
                let shape = match cursor_style_id {
                    0 => None,
                    1 | 2 => Some(CursorShape::Block),
                    3 | 4 => Some(CursorShape::Underline),
                    5 | 6 => Some(CursorShape::Beam),
                    _ => {
                        unhandled!();
                        return;
                    },
                };
                let cursor_style =
                    shape.map(|shape| CursorStyle { shape, blinking: cursor_style_id % 2 == 1 });

                handler.set_cursor_style(cursor_style);
            },
            ('r', []) => {
                let top = next_param_or(1) as usize;
                let bottom =
                    params_iter.next().map(|param| param[0] as usize).filter(|&param| param != 0);

                handler.set_scrolling_region(top, bottom);
            },
            ('S', []) => handler.scroll_up(next_param_or(1) as usize),
            ('s', []) => handler.save_cursor_position(),
            ('T', []) => handler.scroll_down(next_param_or(1) as usize),
            ('t', []) => match next_param_or(1) as usize {
                14 => handler.text_area_size_pixels(),
                16 => handler.text_area_cell_size_pixels(),
                18 => handler.text_area_size_chars(),
                22 => handler.push_title(),
                23 => handler.pop_title(),
                _ => unhandled!(),
            },
            ('u', [b'?']) => handler.report_keyboard_mode(),
            ('u', [b'=']) => {
                // Flags that do not fit the protocol's flag byte are rejected, not truncated.
                let Ok(flags) = u8::try_from(next_param_or(0)) else {
                    unhandled!();
                    return;
                };
                let mode = KeyboardModes::from_bits_truncate(flags);
                let behavior = match next_param_or(1) {
                    3 => KeyboardModesApplyBehavior::Difference,
                    2 => KeyboardModesApplyBehavior::Union,
                    // Default is replace.
                    _ => KeyboardModesApplyBehavior::Replace,
                };
                handler.set_keyboard_mode(mode, behavior);
            },
            ('u', [b'>']) => {
                let Ok(flags) = u8::try_from(next_param_or(0)) else {
                    unhandled!();
                    return;
                };
                handler.push_keyboard_mode(KeyboardModes::from_bits_truncate(flags));
            },
            ('u', [b'<']) => {
                // The default is 1.
                handler.pop_keyboard_modes(next_param_or(1));
            },
            ('u', []) => handler.restore_cursor_position(),
            ('X', []) => handler.erase_chars(next_param_or(1) as usize),
            ('Z', []) => handler.move_backward_tabs(next_param_or(1)),
            _ => unhandled!(),
        }
    }

    #[inline]
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        macro_rules! unhandled {
            () => {{
                debug!(
                    event = "vvte.esc.unhandled",
                    intermediates:% = intermediates.escape_ascii(),
                    byte;
                    "unhandled ESC {}{}",
                    intermediates.escape_ascii(),
                    byte.escape_ascii()
                );
            }};
        }

        macro_rules! configure_charset {
            ($charset:path, $intermediates:expr) => {{
                let index: CharsetIndex = match $intermediates {
                    [b'('] => CharsetIndex::G0,
                    [b')'] => CharsetIndex::G1,
                    [b'*'] => CharsetIndex::G2,
                    [b'+'] => CharsetIndex::G3,
                    _ => {
                        unhandled!();
                        return;
                    },
                };
                self.handler.configure_charset(index, $charset)
            }};
        }

        match (byte, intermediates) {
            (b'B', intermediates) => configure_charset!(StandardCharset::Ascii, intermediates),
            (b'D', []) => self.handler.linefeed(),
            (b'E', []) => {
                self.handler.linefeed();
                self.handler.carriage_return();
            },
            (b'H', []) => self.handler.set_horizontal_tabstop(),
            (b'M', []) => self.handler.reverse_index(),
            (b'Z', []) => self.handler.identify_terminal(None),
            (b'c', []) => self.handler.reset_state(),
            (b'0', intermediates) => {
                configure_charset!(StandardCharset::SpecialCharacterAndLineDrawing, intermediates);
            },
            (b'7', []) => self.handler.save_cursor_position(),
            (b'8', [b'#']) => self.handler.decaln(),
            (b'8', []) => self.handler.restore_cursor_position(),
            (b'=', []) => self.handler.set_keypad_application_mode(),
            (b'>', []) => self.handler.unset_keypad_application_mode(),
            // String terminator, do nothing (parser handles as string terminator).
            (b'\\', []) => (),
            _ => unhandled!(),
        }
    }

    #[inline]
    fn terminated(&self) -> bool {
        self.terminated
    }
}

#[inline]
fn attrs_from_sgr_parameters<H: Handler>(handler: &mut H, params: &mut ParamsIter<'_>) {
    while let Some(param) = params.next() {
        let attr = match param {
            [0] => Some(Attr::Reset),
            [1] => Some(Attr::Bold),
            [2] => Some(Attr::Dim),
            [3] => Some(Attr::Italic),
            [4, 0] => Some(Attr::CancelUnderline),
            [4, 2] => Some(Attr::DoubleUnderline),
            [4, 3] => Some(Attr::Undercurl),
            [4, 4] => Some(Attr::DottedUnderline),
            [4, 5] => Some(Attr::DashedUnderline),
            [4, ..] => Some(Attr::Underline),
            [5] => Some(Attr::BlinkSlow),
            [6] => Some(Attr::BlinkFast),
            [7] => Some(Attr::Reverse),
            [8] => Some(Attr::Hidden),
            [9] => Some(Attr::Strike),
            [21] => Some(Attr::CancelBold),
            [22] => Some(Attr::CancelBoldDim),
            [23] => Some(Attr::CancelItalic),
            [24] => Some(Attr::CancelUnderline),
            [25] => Some(Attr::CancelBlink),
            [27] => Some(Attr::CancelReverse),
            [28] => Some(Attr::CancelHidden),
            [29] => Some(Attr::CancelStrike),
            [30] => Some(Attr::Foreground(Color::Named(NamedColor::Black))),
            [31] => Some(Attr::Foreground(Color::Named(NamedColor::Red))),
            [32] => Some(Attr::Foreground(Color::Named(NamedColor::Green))),
            [33] => Some(Attr::Foreground(Color::Named(NamedColor::Yellow))),
            [34] => Some(Attr::Foreground(Color::Named(NamedColor::Blue))),
            [35] => Some(Attr::Foreground(Color::Named(NamedColor::Magenta))),
            [36] => Some(Attr::Foreground(Color::Named(NamedColor::Cyan))),
            [37] => Some(Attr::Foreground(Color::Named(NamedColor::White))),
            [38] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(Attr::Foreground)
            },
            [38, params @ ..] => handle_colon_rgb(params).map(Attr::Foreground),
            [39] => Some(Attr::Foreground(Color::Named(NamedColor::Foreground))),
            [40] => Some(Attr::Background(Color::Named(NamedColor::Black))),
            [41] => Some(Attr::Background(Color::Named(NamedColor::Red))),
            [42] => Some(Attr::Background(Color::Named(NamedColor::Green))),
            [43] => Some(Attr::Background(Color::Named(NamedColor::Yellow))),
            [44] => Some(Attr::Background(Color::Named(NamedColor::Blue))),
            [45] => Some(Attr::Background(Color::Named(NamedColor::Magenta))),
            [46] => Some(Attr::Background(Color::Named(NamedColor::Cyan))),
            [47] => Some(Attr::Background(Color::Named(NamedColor::White))),
            [48] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(Attr::Background)
            },
            [48, params @ ..] => handle_colon_rgb(params).map(Attr::Background),
            [49] => Some(Attr::Background(Color::Named(NamedColor::Background))),
            [58] => {
                let mut iter = params.map(|param| param[0]);
                parse_sgr_color(&mut iter).map(|color| Attr::UnderlineColor(Some(color)))
            },
            [58, params @ ..] => {
                handle_colon_rgb(params).map(|color| Attr::UnderlineColor(Some(color)))
            },
            [59] => Some(Attr::UnderlineColor(None)),
            [90] => Some(Attr::Foreground(Color::Named(NamedColor::BrightBlack))),
            [91] => Some(Attr::Foreground(Color::Named(NamedColor::BrightRed))),
            [92] => Some(Attr::Foreground(Color::Named(NamedColor::BrightGreen))),
            [93] => Some(Attr::Foreground(Color::Named(NamedColor::BrightYellow))),
            [94] => Some(Attr::Foreground(Color::Named(NamedColor::BrightBlue))),
            [95] => Some(Attr::Foreground(Color::Named(NamedColor::BrightMagenta))),
            [96] => Some(Attr::Foreground(Color::Named(NamedColor::BrightCyan))),
            [97] => Some(Attr::Foreground(Color::Named(NamedColor::BrightWhite))),
            [100] => Some(Attr::Background(Color::Named(NamedColor::BrightBlack))),
            [101] => Some(Attr::Background(Color::Named(NamedColor::BrightRed))),
            [102] => Some(Attr::Background(Color::Named(NamedColor::BrightGreen))),
            [103] => Some(Attr::Background(Color::Named(NamedColor::BrightYellow))),
            [104] => Some(Attr::Background(Color::Named(NamedColor::BrightBlue))),
            [105] => Some(Attr::Background(Color::Named(NamedColor::BrightMagenta))),
            [106] => Some(Attr::Background(Color::Named(NamedColor::BrightCyan))),
            [107] => Some(Attr::Background(Color::Named(NamedColor::BrightWhite))),
            _ => None,
        };

        if let Some(attr) = attr {
            handler.terminal_attribute(attr);
        }
    }
}

/// OSC parameters rendered for diagnostics as they were sent, `;`-separated and ASCII-escaped.
struct OscParams<'a>(&'a [&'a [u8]]);

impl fmt::Display for OscParams<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, param) in self.0.iter().enumerate() {
            if i != 0 {
                f.write_str(";")?;
            }
            write!(f, "{}", param.escape_ascii())?;
        }
        Ok(())
    }
}

/// Handle colon separated rgb color escape sequence.
#[inline]
fn handle_colon_rgb(params: &[u16]) -> Option<Color> {
    let rgb_start = if params.len() > 4 { 2 } else { 1 };
    let rgb_iter = params[rgb_start..].iter().copied();
    let mut iter = iter::once(params[0]).chain(rgb_iter);

    parse_sgr_color(&mut iter)
}

/// Parse a color specifier from list of attributes.
#[inline]
fn parse_sgr_color<I: Iterator<Item = u16>>(params: &mut I) -> Option<Color> {
    match params.next() {
        Some(2) => Some(Color::Spec(Rgb {
            r: u8::try_from(params.next()?).ok()?,
            g: u8::try_from(params.next()?).ok()?,
            b: u8::try_from(params.next()?).ok()?,
        })),
        Some(5) => Some(Color::Indexed(u8::try_from(params.next()?).ok()?)),
        _ => None,
    }
}

/// C0 control characters dispatched by the processor (from ANSI X3.4-1977).
#[expect(non_snake_case, reason = "named after the C0 control set it holds")]
mod C0 {
    /// Bell, triggers the bell, buzzer, or beeper on the terminal.
    pub const BEL: u8 = 0x07;
    /// Backspace, can be used to define overstruck characters.
    pub const BS: u8 = 0x08;
    /// Horizontal Tabulation, move to next predetermined position.
    pub const HT: u8 = 0x09;
    /// Linefeed, move to same position on next line (see also NL).
    pub const LF: u8 = 0x0A;
    /// Vertical Tabulation, move to next predetermined line.
    pub const VT: u8 = 0x0B;
    /// Form Feed, move to next form or page.
    pub const FF: u8 = 0x0C;
    /// Carriage Return, move to first character of current line.
    pub const CR: u8 = 0x0D;
    /// Shift Out, switch to G1 (other half of character set).
    pub const SO: u8 = 0x0E;
    /// Shift In, switch to G0 (normal half of character set).
    pub const SI: u8 = 0x0F;
    /// Substitute (VT100 uses this to display parity errors).
    pub const SUB: u8 = 0x1A;
}

// Tests for parsing escape sequences.
//
// Byte sequences used in these tests are recording of pty stdout.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rep_normalizes_counts_and_preserves_preceding_character() {
        #[derive(Default)]
        struct Recorder {
            printed: Vec<char>,
            repeated: Vec<(char, usize)>,
        }
        impl Handler for Recorder {
            fn input(&mut self, c: char) {
                self.printed.push(c);
            }
            fn repeat_char(&mut self, c: char, count: usize) {
                self.repeated.push((c, count));
            }
        }
        let input = b"\x1b[10bA\x1b[b\x1b[0b\x1b[2b\x1b[31m\x1b[65535bB\x1b[3b";
        for chunk_size in [1, 2, 7, input.len()] {
            let mut handler = Recorder::default();
            let mut parser: Processor = Processor::new();
            for chunk in input.chunks(chunk_size) {
                parser.advance(&mut handler, chunk);
            }
            assert_eq!(handler.printed, ['A', 'B']);
            assert_eq!(handler.repeated, [('A', 1), ('A', 1), ('A', 2), ('A', 65535), ('B', 3)]);
        }
    }

    #[test]
    fn default_repeat_handler_calls_input() {
        #[derive(Default)]
        struct Recorder(Vec<char>);
        impl Handler for Recorder {
            fn input(&mut self, c: char) {
                self.0.push(c);
            }
        }
        let mut handler = Recorder::default();
        Processor::<StdSyncHandler>::new().advance(&mut handler, b"a\x1b[3b");
        handler.repeat_char('b', 0);
        assert_eq!(handler.0, ['a'; 4]);
    }

    #[derive(Default)]
    pub struct TestSyncHandler {
        is_sync: usize,
    }

    impl Timeout for TestSyncHandler {
        #[inline]
        fn set_timeout(&mut self, _: Duration) {
            self.is_sync += 1;
        }

        #[inline]
        fn clear_timeout(&mut self) {
            self.is_sync = 0;
        }

        #[inline]
        fn pending_timeout(&self) -> bool {
            self.is_sync != 0
        }
    }

    struct MockHandler {
        index: CharsetIndex,
        charset: StandardCharset,
        attr: Option<Attr>,
        identity_reported: bool,
        color: Option<Rgb>,
        reset_colors: Vec<usize>,
        /// Order-sensitive record of the events the APC tests care about.
        events: Vec<String>,
    }

    impl Handler for MockHandler {
        fn terminal_attribute(&mut self, attr: Attr) {
            self.attr = Some(attr);
        }

        fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
            self.index = index;
            self.charset = charset;
        }

        fn set_active_charset(&mut self, index: CharsetIndex) {
            self.index = index;
        }

        fn identify_terminal(&mut self, _intermediate: Option<char>) {
            self.identity_reported = true;
        }

        fn reset_state(&mut self) {
            *self = Self::default();
        }

        fn set_color(&mut self, _: usize, c: Rgb) {
            self.color = Some(c);
        }

        fn reset_color(&mut self, index: usize) {
            self.reset_colors.push(index);
        }

        fn goto(&mut self, line: i32, col: usize) {
            self.events.push(format!("goto {line} {col}"));
        }

        fn text_area_cell_size_pixels(&mut self) {
            self.events.push("cell size".into());
        }

        fn apc_start(&mut self) {
            self.events.push("apc:".into());
        }

        fn apc_put(&mut self, bytes: &[u8]) {
            self.events.last_mut().unwrap().push_str(str::from_utf8(bytes).unwrap());
        }

        fn apc_end(&mut self, cancelled: bool) {
            self.events.last_mut().unwrap().push_str(if cancelled { " cancelled" } else { " end" });
        }
    }

    impl Default for MockHandler {
        fn default() -> MockHandler {
            MockHandler {
                index: CharsetIndex::G0,
                charset: StandardCharset::Ascii,
                attr: None,
                identity_reported: false,
                color: None,
                reset_colors: Vec::new(),
                events: Vec::new(),
            }
        }
    }

    #[test]
    fn parse_control_attribute() {
        static BYTES: &[u8] = &[0x1B, b'[', b'1', b'm'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        assert_eq!(handler.attr, Some(Attr::Bold));
    }

    #[test]
    fn parse_terminal_identity_csi() {
        let bytes: &[u8] = &[0x1B, b'[', b'1', b'c'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(!handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'[', b'c'];

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'[', b'0', b'c'];

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
    }

    #[test]
    fn parse_terminal_identity_esc() {
        let bytes: &[u8] = &[0x1B, b'Z'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(handler.identity_reported);
        handler.reset_state();

        let bytes: &[u8] = &[0x1B, b'#', b'Z'];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert!(!handler.identity_reported);
        handler.reset_state();
    }

    #[test]
    fn parse_truecolor_attr() {
        static BYTES: &[u8] = &[
            0x1B, b'[', b'3', b'8', b';', b'2', b';', b'1', b'2', b'8', b';', b'6', b'6', b';',
            b'2', b'5', b'5', b'm',
        ];

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        let spec = Rgb { r: 128, g: 66, b: 255 };

        assert_eq!(handler.attr, Some(Attr::Foreground(Color::Spec(spec))));
    }

    /// No exactly a test; useful for debugging.
    #[test]
    fn parse_zsh_startup() {
        static BYTES: &[u8] = &[
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'7', b'm', b'%', 0x1B, b'[', b'2', b'7', b'm',
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'0', b'm', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b'\r', b' ', b'\r', b'\r', 0x1B, b'[', b'0', b'm', 0x1B, b'[', b'2',
            b'7', b'm', 0x1B, b'[', b'2', b'4', b'm', 0x1B, b'[', b'J', b'j', b'w', b'i', b'l',
            b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd', b'e', b's', b'k', b' ', 0x1B,
            b'[', b'0', b'1', b';', b'3', b'2', b'm', 0xE2, 0x9E, 0x9C, b' ', 0x1B, b'[', b'0',
            b'1', b';', b'3', b'2', b'm', b' ', 0x1B, b'[', b'3', b'6', b'm', b'~', b'/', b'c',
            b'o', b'd', b'e',
        ];

        let mut handler = MockHandler::default();
        let mut parser = Processor::<TestSyncHandler>::new();

        parser.advance(&mut handler, BYTES);
    }

    #[test]
    fn parse_designate_g0_as_line_drawing() {
        static BYTES: &[u8] = &[0x1B, b'(', b'0'];
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, BYTES);

        assert_eq!(handler.index, CharsetIndex::G0);
        assert_eq!(handler.charset, StandardCharset::SpecialCharacterAndLineDrawing);
    }

    #[test]
    fn parse_designate_g1_as_line_drawing_and_invoke() {
        static BYTES: &[u8] = &[0x1B, b')', b'0', 0x0E];
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, &BYTES[..3]);

        assert_eq!(handler.index, CharsetIndex::G1);
        assert_eq!(handler.charset, StandardCharset::SpecialCharacterAndLineDrawing);

        let mut handler = MockHandler::default();
        parser.advance(&mut handler, &[BYTES[3]]);

        assert_eq!(handler.index, CharsetIndex::G1);
    }

    #[test]
    fn parse_valid_rgb_colors() {
        assert_eq!(xparse_color(b"rgb:f/e/d"), Some(Rgb { r: 0xFF, g: 0xEE, b: 0xDD }));
        assert_eq!(xparse_color(b"rgb:11/aa/ff"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"rgb:f/ed1/cb23"), Some(Rgb { r: 0xFF, g: 0xEC, b: 0xCA }));
        assert_eq!(xparse_color(b"rgb:ffff/0/0"), Some(Rgb { r: 0xFF, g: 0x0, b: 0x0 }));
    }

    #[test]
    fn parse_valid_legacy_rgb_colors() {
        assert_eq!(xparse_color(b"#1af"), Some(Rgb { r: 0x10, g: 0xA0, b: 0xF0 }));
        assert_eq!(xparse_color(b"#11aaff"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"#110aa0ff0"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
        assert_eq!(xparse_color(b"#1100aa00ff00"), Some(Rgb { r: 0x11, g: 0xAA, b: 0xFF }));
    }

    #[test]
    fn parse_invalid_rgb_colors() {
        assert_eq!(xparse_color(b"rgb:0//"), None);
        assert_eq!(xparse_color(b"rgb://///"), None);
    }

    #[test]
    fn parse_invalid_legacy_rgb_colors() {
        assert_eq!(xparse_color(b"#"), None);
        assert_eq!(xparse_color(b"#f"), None);
        assert_eq!(xparse_color(b"#+f+f+f"), None, "signs are not hex digits");
    }

    /// A channel of 17 or more digits, possible with leading zeros, overflowed the shift that
    /// kept its top digits: debug builds panicked and release builds returned the wrong color.
    #[test]
    fn long_legacy_colors_keep_their_top_digits() {
        let channel = |top: &str| format!("{top}{}", "0".repeat(15));
        let color = format!("#{}{}{}", channel("ab"), channel("00"), channel("f1"));
        assert_eq!(xparse_color(color.as_bytes()), Some(Rgb { r: 0xAB, g: 0x00, b: 0xF1 }));

        let osc = format!("\x1b]4;0;{color}\x07");
        let mut handler = MockHandler::default();
        Processor::<TestSyncHandler>::new().advance(&mut handler, osc.as_bytes());
        assert_eq!(handler.color, Some(Rgb { r: 0xAB, g: 0x00, b: 0xF1 }));
    }

    #[test]
    fn parse_invalid_number() {
        assert_eq!(parse_number(b"1abc"), None);
    }

    #[test]
    fn parse_valid_number() {
        assert_eq!(parse_number(b"123"), Some(123));
    }

    #[test]
    fn parse_number_too_large() {
        assert_eq!(parse_number(b"321"), None);
    }

    #[test]
    fn parse_osc4_set_color() {
        let bytes: &[u8] = b"\x1b]4;0;#fff\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert_eq!(handler.color, Some(Rgb { r: 0xF0, g: 0xF0, b: 0xF0 }));
    }

    #[test]
    fn parse_osc104_reset_color() {
        let bytes: &[u8] = b"\x1b]104;1;\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        assert_eq!(handler.reset_colors, vec![1]);
    }

    #[test]
    fn parse_osc104_reset_all_colors() {
        let bytes: &[u8] = b"\x1b]104;\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        let expected: Vec<usize> = (0..256).collect();
        assert_eq!(handler.reset_colors, expected);
    }

    #[test]
    fn parse_osc104_reset_all_colors_no_semicolon() {
        let bytes: &[u8] = b"\x1b]104\x1b\\";

        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, bytes);

        let expected: Vec<usize> = (0..256).collect();
        assert_eq!(handler.reset_colors, expected);
    }

    #[test]
    fn apc_reaches_the_handler_in_order_with_surrounding_sequences() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, b"\x1b[3;4H\x1b_Gi=1;QUJD\x1b\\\x1b[5;6H\x1b[16t");

        assert_eq!(handler.events, ["goto 2 3", "apc:Gi=1;QUJD end", "goto 4 5", "cell size"]);
    }

    /// A graphics command placed with the cursor must not run before a cursor move that is still
    /// held in the synchronized-update buffer.
    #[test]
    fn apc_inside_a_synchronized_update_is_replayed_in_order() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        parser.advance(&mut handler, b"\x1b[?2026h\x1b[3;4H\x1b_Gi=1;QU");
        parser.advance(&mut handler, b"JD\x1b\\\x1b[5;6H");
        assert!(handler.events.is_empty(), "nothing runs while the update is held");

        parser.advance(&mut handler, b"\x1b[?2026l");
        assert_eq!(handler.events, ["goto 2 3", "apc:Gi=1;QUJD end", "goto 4 5"]);
    }

    /// Records what an unbuffered synchronized update dispatches, in order.
    #[derive(Default)]
    struct SyncRecorder(Vec<String>);

    impl Handler for SyncRecorder {
        fn input(&mut self, c: char) {
            match self.0.last_mut() {
                Some(last) if last.starts_with('"') => last.push(c),
                _ => self.0.push(format!("\"{c}")),
            }
        }

        fn goto(&mut self, line: i32, col: usize) {
            self.0.push(format!("goto {line} {col}"));
        }

        fn set_private_mode(&mut self, mode: PrivateMode) {
            self.0.push(format!("set {mode:?}"));
        }

        fn unset_private_mode(&mut self, mode: PrivateMode) {
            self.0.push(format!("unset {mode:?}"));
        }

        fn apc_start(&mut self) {
            self.0.push("apc:".into());
        }

        fn apc_put(&mut self, bytes: &[u8]) {
            self.0.last_mut().unwrap().push_str(str::from_utf8(bytes).unwrap());
        }

        fn apc_end(&mut self, _: bool) {
            self.0.last_mut().unwrap().push_str(" end");
        }
    }

    const SYNC: &str = "set Named(SyncUpdate)";
    const UNSYNC: &str = "unset Named(SyncUpdate)";

    #[test]
    fn unbuffered_sync_dispatches_at_once_and_reports_its_bounds() {
        let mut parser = Processor::<TestSyncHandler>::with_unbuffered_sync();
        let mut handler = SyncRecorder::default();

        parser.advance(&mut handler, b"\x1b[?2026h\x1b[3;4Hab\x1b_Gi=1;QU");
        assert!(parser.state.sync_state.timeout.pending_timeout());
        assert_eq!(handler.0, [SYNC, "goto 2 3", "\"ab", "apc:Gi=1;QU"]);

        parser.advance(&mut handler, b"JD\x1b\\\x1b[?2026lc");
        assert!(!parser.state.sync_state.timeout.pending_timeout());
        assert_eq!(parser.sync_bytes_count(), 0);
        assert_eq!(handler.0, [SYNC, "goto 2 3", "\"ab", "apc:Gi=1;QUJD end", UNSYNC, "\"c"]);
    }

    #[test]
    fn unbuffered_sync_keeps_its_first_deadline() {
        let mut parser = Processor::<TestSyncHandler>::with_unbuffered_sync();
        let mut handler = SyncRecorder::default();

        parser.advance(&mut handler, b"\x1b[?2026ha\x1b[?2026hb\x1b[?2026h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1, "a BSU inside must not extend it");

        parser.advance(&mut handler, b"\x1b[?2026l\x1b[?2026h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1, "the next update starts afresh");
    }

    #[test]
    fn unbuffered_sync_ends_at_stop_sync() {
        let mut parser = Processor::<TestSyncHandler>::with_unbuffered_sync();
        let mut handler = SyncRecorder::default();

        parser.advance(&mut handler, b"\x1b[?2026hab");
        parser.stop_sync(&mut handler);
        assert!(!parser.state.sync_state.timeout.pending_timeout());
        parser.advance(&mut handler, b"c\x1b[?2026l");
        assert_eq!(handler.0, [SYNC, "\"ab", UNSYNC, "\"c", UNSYNC]);
    }

    #[test]
    fn unbuffered_sync_bulk_and_byte_feeds_match() {
        let input = b"x\x1b[?2026h\x1b[1;2Hab\x1b_Gi=1;QUJD\x1b\\\x1b[?2026hcd\x1b[?2026ley";
        let mut bulk = SyncRecorder::default();
        Processor::<TestSyncHandler>::with_unbuffered_sync().advance(&mut bulk, input);

        let mut bytewise = SyncRecorder::default();
        let mut parser = Processor::<TestSyncHandler>::with_unbuffered_sync();
        for byte in input {
            parser.advance(&mut bytewise, &[*byte]);
        }
        assert_eq!(bulk.0, bytewise.0);
        assert!(!parser.state.sync_state.timeout.pending_timeout());
    }

    #[test]
    #[cfg_attr(miri, ignore = "feeds 4 MiB, which is too slow under Miri")]
    fn unbuffered_sync_holds_nothing_past_the_buffer_limit() {
        let mut parser = Processor::<TestSyncHandler>::with_unbuffered_sync();
        let mut handler = SyncRecorder::default();

        parser.advance(&mut handler, b"\x1b[?2026h");
        parser.advance(&mut handler, "a".repeat(2 * SYNC_BUFFER_SIZE).as_bytes());
        assert_eq!(parser.state.sync_state.buffer.capacity(), 0);
        assert!(
            parser.state.sync_state.timeout.pending_timeout(),
            "only ESU or the deadline ends it"
        );
        assert_eq!(handler.0.len(), 2);
        assert_eq!(handler.0[1].len(), 1 + 2 * SYNC_BUFFER_SIZE);
    }

    #[test]
    fn partial_sync_updates() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Dispatch some data.

        parser.advance(&mut handler, b"random \x1b[31m stuff");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Extend synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26h");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert!(handler.attr.is_none());

        // Terminate synchronized update.

        parser.advance(&mut handler, b"\x1b[?20");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert!(handler.attr.is_none());

        parser.advance(&mut handler, b"26l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_some());
    }

    #[test]
    #[cfg_attr(miri, ignore = "feeds 4 MiB, which is too slow under Miri")]
    fn sync_bursts_buffer() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Repeat test twice to ensure internal state is reset properly.
        for _ in 0..2 {
            // Start synchronized update.
            parser.advance(&mut handler, b"\x1b[?2026h");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
            assert!(handler.attr.is_none());

            // Ensure sync works.
            parser.advance(&mut handler, b"\x1b[31m");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
            assert!(handler.attr.is_none());

            // Exceed sync buffer dimensions.
            parser.advance(&mut handler, "a".repeat(SYNC_BUFFER_SIZE).as_bytes());
            assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
            assert!(handler.attr.take().is_some());

            // Ensure new events are dispatched directly.
            parser.advance(&mut handler, b"\x1b[31m");
            assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
            assert!(handler.attr.take().is_some());
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "feeds 256 KiB through the sync buffer, too slow under Miri")]
    fn the_sync_buffer_is_grown_on_demand_and_handed_back() {
        // Every `Processor` used to reserve SYNC_BUFFER_SIZE at construction, so a pane that never
        // saw a synchronized update still paid 2MiB for one.
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(
            parser.state.sync_state.buffer.capacity(),
            0,
            "a fresh processor reserves nothing",
        );

        // An ordinary synchronized update still buffers and replays correctly.
        parser.advance(&mut handler, b"\x1b[?2026h");
        parser.advance(&mut handler, b"\x1b[31m");
        assert!(handler.attr.is_none(), "the update is held while synchronized");
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert!(handler.attr.take().is_some(), "the held bytes are replayed on ESU");
        assert!(parser.state.sync_state.buffer.capacity() <= SYNC_RETAINED_CAPACITY);

        // A large update grows the buffer, and ending it hands the excess back.
        parser.advance(&mut handler, b"\x1b[?2026h");
        parser.advance(&mut handler, "a".repeat(SYNC_RETAINED_CAPACITY * 4).as_bytes());
        assert!(
            parser.state.sync_state.buffer.capacity() > SYNC_RETAINED_CAPACITY,
            "the buffer grows to hold a large update",
        );
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert!(
            parser.state.sync_state.buffer.capacity() <= SYNC_RETAINED_CAPACITY,
            "capacity past the retained floor is released rather than held for the pane's life",
        );
    }

    #[test]
    fn mixed_sync_escape() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update with immediate SGR.
        parser.advance(&mut handler, b"\x1b[?2026h\x1b[31m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Terminate synchronized update and check for SGR.
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_some());
    }

    #[test]
    fn sync_bsu_with_esu() {
        let mut parser = Processor::<TestSyncHandler>::new();
        let mut handler = MockHandler::default();

        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert!(handler.attr.is_none());

        // Start synchronized update with immediate SGR.
        parser.advance(&mut handler, b"\x1b[?2026h\x1b[1m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 1);
        assert!(handler.attr.is_none());

        // Terminate synchronized update, but immediately start a new one.
        parser.advance(&mut handler, b"\x1b[?2026l\x1b[?2026h\x1b[4m");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 2);
        assert_eq!(handler.attr.take(), Some(Attr::Bold));

        // Terminate again, expecting one buffered SGR.
        parser.advance(&mut handler, b"\x1b[?2026l");
        assert_eq!(parser.state.sync_state.timeout.is_sync, 0);
        assert_eq!(handler.attr.take(), Some(Attr::Underline));
    }

    #[test]
    fn contrast() {
        // `powf` may differ in its last bits between math libraries, and Miri perturbs it on
        // purpose, so compare to a relative tolerance far above that noise.
        fn assert_close(actual: f64, expected: f64) {
            let error = (actual - expected).abs() / expected;
            assert!(error < 1e-12, "contrast {actual} differs from {expected}");
        }

        let rgb1 = Rgb { r: 0xFF, g: 0xFF, b: 0xFF };
        let rgb2 = Rgb { r: 0x00, g: 0x00, b: 0x00 };
        assert_close(rgb1.contrast(rgb2), 21.);

        let rgb1 = Rgb { r: 0xFF, g: 0xFF, b: 0xFF };
        assert_close(rgb1.contrast(rgb1), 1.);

        let rgb1 = Rgb { r: 0xFF, g: 0x00, b: 0xFF };
        let rgb2 = Rgb { r: 0x00, g: 0xFF, b: 0x00 };
        assert_close(rgb1.contrast(rgb2), 2.285_543_608_124_253_3);

        let rgb1 = Rgb { r: 0x12, g: 0x34, b: 0x56 };
        let rgb2 = Rgb { r: 0xFE, g: 0xDC, b: 0xBA };
        assert_close(rgb1.contrast(rgb2), 9.786_558_997_257_74);
    }
}

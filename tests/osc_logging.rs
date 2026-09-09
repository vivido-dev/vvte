#![cfg(feature = "ansi")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};

use vvte::ansi::{Handler, Processor};

thread_local! {
    // Only allocations on this test's thread inside the measured section count.
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn allocated() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(value) = count.get() {
            count.set(Some(value + 1));
        }
    });
}

// SAFETY: Every allocation operation is forwarded to System with the original arguments.
// The thread-local counter only observes calls and does not access allocated memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated();
        // SAFETY: The caller supplies the layout required by GlobalAlloc::alloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated();
        // SAFETY: The caller supplies the layout required by GlobalAlloc::alloc_zeroed.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated();
        // SAFETY: The caller supplies the pointer, layout, and size required by realloc.
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout belong to the forwarded System allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

thread_local! {
    static RECORDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}
struct Logger;
impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        assert_eq!(record.target(), "vvte::ansi");
        RECORDS.with(|records| records.borrow_mut().push(record.args().to_string()));
    }
    fn flush(&self) {}
}

struct Terminal;
impl Handler for Terminal {}

#[test]
fn unsupported_osc_formats_only_when_debug_is_enabled() {
    log::set_logger(&Logger).unwrap();
    log::set_max_level(log::LevelFilter::Off);
    let mut parser: Processor = Processor::new();
    let mut terminal = Terminal;
    let input = b"\x1b]6;ab\x07";

    // Warm the parser's reusable OSC buffer before counting diagnostic allocations.
    parser.advance(&mut terminal, input);
    ALLOCATIONS.with(|count| count.set(Some(0)));
    parser.advance(&mut terminal, input);
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    assert_eq!(allocations, 0, "disabled diagnostics must not allocate");
    RECORDS.with(|records| assert!(records.borrow().is_empty()));

    log::set_max_level(log::LevelFilter::Debug);
    parser.advance(&mut terminal, input);
    RECORDS.with(|records| {
        let records = records.borrow();
        assert_eq!(records.len(), 1);
        assert!(records[0].starts_with("[unhandled osc_dispatch]: [['6'],['a''b'],] at line "));
    });
}

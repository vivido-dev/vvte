//! Diagnostics of the ANSI processor: cost when disabled, structure, and volume.
#![cfg(feature = "ansi")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};

use vvte::ansi::{Handler, Processor, Rgb};

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

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        allocated();
        // SAFETY: The caller supplies the pointer, layout, and size required by realloc.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The pointer and layout belong to the forwarded System allocation.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One log record: its message and its key-value fields, in order.
#[derive(Debug)]
struct Record {
    message: String,
    fields: Vec<(String, String)>,
}

impl Record {
    fn field(&self, key: &str) -> Option<&str> {
        self.fields.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str())
    }
}

thread_local! {
    static RECORDS: RefCell<Vec<Record>> = const { RefCell::new(Vec::new()) };
}

struct Fields(Vec<(String, String)>);

impl<'kvs> log::kv::VisitSource<'kvs> for Fields {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'kvs>,
        value: log::kv::Value<'kvs>,
    ) -> Result<(), log::kv::Error> {
        self.0.push((key.to_string(), value.to_string()));
        Ok(())
    }
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        assert_eq!(record.target(), "vvte::ansi");
        let mut fields = Fields(Vec::new());
        record.key_values().visit(&mut fields).expect("collecting fields cannot fail");
        let record = Record { message: record.args().to_string(), fields: fields.0 };
        RECORDS.with(|records| records.borrow_mut().push(record));
    }

    fn flush(&self) {}
}

struct Terminal;
impl Handler for Terminal {}

fn take_records() -> Vec<Record> {
    RECORDS.with(RefCell::take)
}

// The logger and the maximum level are process-wide, so every check shares one test.
#[test]
fn diagnostics_stay_cheap_structured_and_bounded() {
    log::set_logger(&Logger).unwrap();
    log::set_max_level(log::LevelFilter::Off);
    let mut parser: Processor = Processor::new();
    let mut terminal = Terminal;
    let unsupported_osc = b"\x1b]6;ab\x07";

    // Disabled diagnostics cost nothing. Warm the parser's reusable OSC buffer before counting.
    parser.advance(&mut terminal, unsupported_osc);
    ALLOCATIONS.with(|count| count.set(Some(0)));
    parser.advance(&mut terminal, unsupported_osc);
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    assert_eq!(allocations, 0, "disabled diagnostics must not allocate");
    assert!(take_records().is_empty());

    // Enabled diagnostics are named events. Payloads, which can hold user data, stay in fields.
    log::set_max_level(log::LevelFilter::Debug);
    parser.advance(&mut terminal, unsupported_osc);
    let records = take_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].message, "unhandled OSC 6");
    assert_eq!(records[0].field("event"), Some("vvte.osc.unhandled"));
    assert_eq!(records[0].field("params"), Some("6;ab"));

    parser.advance(&mut terminal, b"\x1b[?1;2$x");
    let records = take_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].message, "unhandled CSI [1;2]?$x");
    assert_eq!(records[0].field("event"), Some("vvte.csi.unhandled"));

    // A DCS string logs its start and end, however long it is.
    let mut dcs = b"\x1bPq".to_vec();
    dcs.extend(std::iter::repeat_n(b'#', 10_000));
    dcs.extend(b"\x1b\\");
    parser.advance(&mut terminal, &dcs);
    let records = take_records();
    let events: Vec<_> = records.iter().filter_map(|record| record.field("event")).collect();
    assert_eq!(events, ["vvte.dcs.unhandled", "vvte.dcs.end"], "{records:?}");

    // Renderers scale colors per cell, so scaling must never log, even at trace level.
    log::set_max_level(log::LevelFilter::Trace);
    let dimmed = Rgb { r: 200, g: 100, b: 50 } * 0.5;
    assert_eq!(dimmed, Rgb { r: 100, g: 50, b: 25 });
    assert!(take_records().is_empty());
}

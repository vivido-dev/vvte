# vvte Microsoft Rust guideline gaps

Audit date: **2026-10-28**. Guideline baseline: the repository's
[ms-rust skill](../../.agents/skills/ms-rust/SKILL.md), compliance date **2026-10-07**.

This is an audit baseline; the [remediation record](#remediation-record-2026-10-28) at the end
tracks what was fixed against it afterwards. vvte is not declared guideline compliant, and no
compliance markers were added.

## Scope and method

- Reviewed vvte 0.15.3 at commit `fe1697c2c7b85d4cbb8286a70e5ee3cc9b50058a` (clean worktree).
  Every file was read in full: `src/lib.rs` (1,783 lines), `src/ansi.rs` (2,452), `src/params.rs`
  (148), `tests/osc_logging.rs` (94), `Cargo.toml`, `README.md`, and `rustfmt.toml`.
- Classification: **library**, published-shaped (`repository`, dual license, `readme`), consumed in
  this repository by `vivido` (`ansi`, `serde`), `vvmux` / `vvmux-terminal` (`ansi`), and
  `vrowser` (raw parser only). Library guidance (files 12–15) applies in full; application
  guidance (file 02) does not.
- Read guideline files 01–15. Files 06 and 10 contain headings only; library and unsafe rules come
  from files 03 and 12–15. vvte has no FFI, no proc macros, no async code, no error types, no
  builders, no I/O, and no statics, so file 05, most of file 07, M-ASYNC-*, M-YIELD-POINTS,
  M-ERRORS-CANONICAL-STRUCTS, M-FROM-ERROR, M-INIT-*, M-SERVICES-CLONE, M-ESCAPE-HATCHES, and
  M-AVOID-STATICS are not applicable. The workspace guidelines (M-CARGO-WORKSPACE and siblings) do
  not apply to a single-crate repository.
- Measured rather than inferred where possible: the M-STATIC-VERIFICATION lint set run via
  command-line flags, rustdoc, cargo-hack, cargo-audit, cargo-udeps, MSRV 1.88, Miri, and a probe
  binary that reproduces each behavioral finding. See [Audit validation](#audit-validation).
- Every finding below was present in the initial import (`81f339f`); none was introduced by the
  fork's later changes (bulk OSC, APC dispatch, printable spans, sync-buffer growth).
- P1 means a safety or security fix to prioritize; P2 a material contract, resilience, or
  verification gap; P3 lower-risk maintenance or a recommendation that needs design or performance
  judgment. "Breaking" means the fix changes public API that a downstream crate can observe.

## Findings at a glance

| ID | Priority | Gap | Guideline IDs | Breaking fix? |
| --- | --- | --- | --- | --- |
| G01 | P1 | OSC payloads are buffered without any bound | M-MOCKABLE-SYSCALLS (bounded input), repo AGENTS.md | No |
| G02 | P2 | Iterator and trait contracts disagree with the code | M-COLLECTION-TRAITS, M-CANONICAL-DOCS, M-UPSTREAM-GUIDELINES | No |
| G03 | P2 | The required gate skips the `ansi` code; no lint table or CI | M-STATIC-VERIFICATION | No |
| G04 | P2 | Public docs are incomplete, example-free, and miss the sync contract | M-CANONICAL-DOCS, M-MODULE-DOCS, M-DESIGN-FOR-AI | No |
| G05 | P2 | Diagnostics fire per byte and per rendered cell | M-LOG-OVERHEAD, M-LOG-STRUCTURED | No |
| G06 | P3 | `unsafe` lacks benchmark evidence and a Miri gate | M-UNSAFE, M-STATIC-VERIFICATION | No |
| G07 | P3 | Vestigial const parameter and features | M-FEATURES-ADDITIVE, C-FEATURE, M-DESIGN-FOR-AI | Yes |
| G08 | P3 | A third-party crate is re-exported and doc-inlined | M-FOREIGN-REEXPORTS, M-DOC-INLINE, M-DONT-LEAK-TYPES | Yes |
| G09 | P3 | Public types miss `Debug`, common traits, and `Parser::new` | M-PUBLIC-DEBUG, C-COMMON-TRAITS, C-CTOR | No |
| G10 | P3 | Lint overrides, panic message, and casts need tidying | M-LINT-OVERRIDE-EXPECT, M-PANIC-MESSAGE | No |
| G11 | P3 | Limits and sizes lack a stated rationale | M-DOCUMENTED-MAGIC | No |
| G12 | P3 | User-facing docs carry design-journey notes | M-NO-META-DESIGN-DOCUMENTATION | No |
| G13 | P3 | Callback signatures use bare `bool`, `usize`, and owned `String` | M-DESIGN-FOR-AI (C-NEWTYPE), M-MEM-REUSE | Yes |
| G14 | P3 | No in-crate benchmarks; DCS data is still per byte | M-HOTPATH, M-THROUGHPUT | No |
| G15 | P3 | Large module, long dispatchers, control-flow macros, tests in `src` | M-BALANCED-MODULES, M-MACRO-LAST-RESORT, M-INTEGRATION-TESTS, M-SMALLER-CRATES | Partly |
| G16 | P2 | Long `#…` colors overflow a shift: panic in debug, wrong color in release (found during remediation) | M-PANIC-ON-BUG, repo AGENTS.md (checked arithmetic) | No |

## Safety and resilience

### G01 — OSC payloads are buffered without any bound

**Evidence:** `src/lib.rs` lines 49 (`osc_raw: Vec<u8>`), 459–472 (bulk append), 626–628
(per-byte append); the `Parser` doc at lines 40–41 states the const parameter "does not impose a
size limit"; README "Growable OSC buffer".

The parser appends every OSC body byte to `osc_raw` until a terminator arrives, with no ceiling.
Any program writing to the PTY — including `cat` of a hostile file or remote output over SSH —
can send `ESC ] 52 ; c ;` followed by an endless payload. The probe fed 64 MiB of unterminated OSC
52 to a `Parser` and it held 64 MiB; it keeps growing until the process runs out of memory. The
`ansi::Processor` accepts OSC 52 (and the title commands) through `osc_capture`, and `vrowser`'s raw
parser captures every OSC, so all three consumers are exposed. Consumers cannot bound this
themselves: the buffer is private and filled before any callback sees it.

The tip under M-MOCKABLE-SYSCALLS requires code handling external input to provide bounded
operations, and the root AGENTS.md requires bounded lengths and resources before allocation. The
synchronized-update buffer already shows the intended pattern: it is capped at 2 MiB and shrunk
afterwards.

**Recommendation:** add a named maximum OSC body length with a documented rationale (it must
admit realistic OSC 52 clipboard payloads). When a body would exceed it, reuse the existing
`osc_discard` path: clear and stop retaining the body, keep framing, and suppress
`osc_dispatch`, exactly as a rejected `osc_capture` does today. Add a regression test that feeds
more than the limit across many `advance` calls and asserts bounded capacity and that the next
command still dispatches (the existing `rejected_osc_body_is_not_buffered_and_next_command_survives`
test is the template).

## Contracts

### G02 — Iterator and trait contracts disagree with the code

**Evidence and gaps:**

1. **`ParamsIter::size_hint` overstates its lower bound** (`src/params.rs` lines 121–125). It
   returns `(remaining, Some(remaining))`, where `remaining` counts parameter *and* subparameter
   slots. For `CSI 38:2:255:0:255;1 m` the probe got `(6, Some(6))` from an iterator that yields 2
   items. The `Iterator` contract requires the lower bound not to exceed the actual count;
   M-COLLECTION-TRAITS requires a truthful `size_hint`. A correct hint is
   `(usize::from(remaining != 0), Some(remaining))`.
2. **`Params::len` is documented as "the number of parameters"** (`src/params.rs` lines 29–33)
   but returns parameters plus subparameters (6 in the example above). Fix the doc; changing the
   semantics would be breaking.
3. **`Timeout::pending_timeout` promises "active and has not yet expired"** (`src/ansi.rs` line
   435), but `StdSyncHandler` returns `timeout.is_some()` (lines 416–419). The probe measured
   `pending_timeout() == true` 200 ms after a BSU with a 150 ms timeout, and the processor kept
   buffering. vivido and vvmux handle this correctly by polling `sync_timeout()` and calling
   `stop_sync`, but that obligation is documented nowhere (see G04). Either make the doc say
   "until cleared, regardless of the deadline" or check the deadline.
4. **Kitty keyboard flags are truncated, not validated** (`src/ansi.rs` lines 1663 and 1673):
   `next_param_or(0) as u8` turns `CSI = 257 u` into flags `1`. Use `u8::try_from` and treat
   out-of-range values as unhandled.

## Verification

### G03 — The required gate skips the `ansi` code; no lint table or CI

**Evidence:** no `[lints]` table in `Cargo.toml`; `#![deny(clippy::all, clippy::if_not_else,
clippy::enum_glob_use)]` inline at `src/lib.rs` line 23; no CI configuration in vvte or for vvte
in the root repository; zero doctests.

- The root AGENTS.md gate, `cargo test --all-targets`, runs with default features only: 41 tests.
  The 28 `ansi` unit tests and `tests/osc_logging.rs` only run with `--features ansi`, yet `ansi`
  is what vivido and vvmux use. `cargo clippy --all-targets` likewise never lints `src/ansi.rs`.
- Running the M-STATIC-VERIFICATION lint set over library code (all features) produced **158
  warnings**: 40 `semicolon_if_nothing_returned`, 37 `missing_docs`, 15 `inline_always`, 10
  `match_same_arms`, 9 `must_use_candidate`, 8 `cast_possible_truncation`, 5 `doc_markdown`, 5
  `missing_debug_implementations`, and 29 others. Including tests: 329.
- cargo-hack, cargo-audit, cargo-udeps, and Miri are not part of any gate (all are installed
  locally and were run for this audit).

**Recommendation:** add the guideline `[lints.rust]` / `[lints.clippy]` tables to `Cargo.toml`
(replacing the inline `#![deny]`), with explicit, reasoned opt-outs where a pedantic lint fights
the state-machine style (`inline_always` and `match_same_arms` are candidates). Make the vvte gate
`cargo test --all-targets --all-features` plus `cargo clippy --all-targets --all-features -- -D
warnings`, and add `cargo hack check --feature-powerset --all-targets`, `cargo audit`, and
`cargo +nightly miri test --lib --all-features` (see G06). Add `package.categories` and
`package.keywords` (flagged by `clippy::cargo`).

## Documentation

### G04 — Public docs are incomplete, example-free, and miss the sync contract

**Evidence:**

- `missing_docs` reports 37 public items without docs, including the `Params`, `Rgb`,
  `Hyperlink`, and `StdSyncHandler` types, `Color` and all its variants, `Processor::new`,
  `NamedColor::to_bright` / `to_dim`, `CursorStyle` fields, and 15 `NamedPrivateMode` variants.
- No public item has an `# Examples` section and `cargo test --doc` runs zero tests. The README
  example is not compiled.
- The `ansi` module doc is one line (`src/ansi.rs` line 7). It does not explain what `Processor`
  adds over `Parser`, which callbacks `Handler` receives, or the synchronized-update contract:
  while a BSU is pending, input is held in a buffer of at most 2 MiB, and **the caller must poll
  `Processor::sync_timeout()` and call `stop_sync` once the deadline passes**, because the
  processor never flushes on its own. Both consumers had to learn this from the source.
- Many `Handler` methods take anonymous parameters with docs that do not name them, so the
  meaning is only recoverable from the call site: `set_color(_: usize, _: Rgb)`,
  `dynamic_color_sequence(_: String, _: usize, _: &str)`, `clipboard_store(_: u8, _: &[u8])`,
  `clipboard_load(_: u8, _: &str)`, `reset_color`, `insert_blank`, `scroll_up`, `erase_chars`,
  `device_status` (lines 445–648). Several docs name parameters that do not exist: `backspace`
  ("`count` characters"), `move_up` ("`rows`").
- The crate doc lists the original state-machine differences but not vvte's own (APC dispatch,
  `osc_capture`, `print_str`), and uses `struct.Parser.html` links instead of intra-doc links
  (C-LINK).
- Typos in public docs: "rotuine", "publc", "an dbase", "suparameters", and the `#[must_use]`
  message "processs".

**Recommendation:** document every public item with a summary sentence; give `Parser::advance`,
`Processor::advance`, and the crate root a compiled example (the README example is a good crate
example); expand the `ansi` module doc to cover `Processor`, `Handler`, synchronized updates, and
the timeout obligation; name every `Handler` parameter and describe it in prose. Renaming trait
parameters is not breaking.

## Operations and diagnostics

### G05 — Diagnostics fire per byte and per rendered cell

**Evidence and gaps:**

- `Performer::put` (`src/ansi.rs` lines 1238–1241) emits `debug!` for **every DCS passthrough
  byte**. The probe counted 10,002 records for one 10,000-byte DCS; a sixel image is megabytes.
  With debug off, the cost is still a level check per byte.
- `impl Mul<f32> for Rgb` (line 115) emits `trace!` with two `Debug`-formatted colors per call.
  vivido calls it per dimmed cell while rendering (`vivido/src/display/content.rs` line 414 via
  `vivido/src/display/color.rs` line 216).
- `execute` logs each unhandled control byte (line 1226).
- All messages are free-form strings (`"[unhandled hook] params={:?}, ..."`) with no event name
  or named properties (M-LOG-STRUCTURED). The `log` facade supports key-value fields.

M-LOG-OVERHEAD asks libraries to assume telemetry is always on and keep it out of inner loops.

**Recommendation:** log a DCS once at `hook` (or at `unhook` with a byte count), drop the `trace!`
from `Rgb::mul`, and give the remaining events stable names and key-value fields. The OSC path
already shows the right shape: formatting gated behind a level check, with an allocation-counting
test.

## Correctness and maintenance

### G06 — `unsafe` lacks benchmark evidence and a Miri gate

**Evidence:** `src/lib.rs` lines 702–704 (`str::from_utf8_unchecked`), 758–760
(`unwrap_unchecked`), and 776–782 (both).

All three blocks have correct `SAFETY` comments, and Miri reports no undefined behavior across
the parser tests that exercise them (see [Audit validation](#audit-validation)). The two
multi-MiB sync-buffer tests are too slow for a Miri gate; mark them `#[cfg_attr(miri, ignore)]`. M-UNSAFE allows performance `unsafe` only after
benchmarking, and requires Miri. No benchmark backs these, and the two in
`advance_partial_utf8` run at most once per `advance` call, on a chunk boundary that split a
code point, where `unwrap_unchecked` saves one predictable branch. Those two have no valid reason
under M-UNSAFE and can become safe code with no measurable cost. The block in `advance_ground`
avoids re-validating the valid prefix of a chunk, which is a real saving; keep it, but back it
with a benchmark (G14) and keep Miri in the gate (G03).

### G07 — Vestigial const parameter and features

**Evidence:** `Parser<const OSC_RAW_BUF_SIZE: usize = 1024>` (`src/lib.rs` line 43);
`Cargo.toml` lines 16–20; README lines 35–43.

- `OSC_RAW_BUF_SIZE` does nothing. It is kept "for API compatibility", but its name reads as a
  limit, which misleads readers and agents (M-DESIGN-FOR-AI) and hides G01. No code in this
  repository names it (`Parser::<` has zero uses outside vvte).
- The `std` feature is not a switch: the crate always requires `std`. The feature only forwards
  to `memchr/std`, so `--no-default-features` silently drops memchr's runtime SIMD detection and
  slows parsing for no benefit. The README feature table describes it as "the standard-library
  parser", which is not what it does.
- `serde` without `ansi` compiles `serde` and uses nothing from it (`cargo udeps --features serde`
  reports it unused), so the feature relies on another feature being enabled manually
  (M-FEATURES-ADDITIVE).

The root AGENTS.md says not to preserve obsolete behavior. **Recommendation:** in the next
breaking release (0.16), remove the const parameter (or, if G01's limit should be
caller-configurable, rename it to say so and make it real), remove the `std` feature and depend on
`memchr` with default features, and make `serde = ["ansi", "dep:serde"]` (that one change is not
breaking). vivido, vvmux, and vrowser list `"std"` explicitly and need matching manifest edits.

### G08 — A third-party crate is re-exported and doc-inlined

**Evidence:** `src/ansi.rs` lines 17–18: `#[doc(inline)] pub use cursor_icon;`.

M-FOREIGN-REEXPORTS says crates should not re-export other crates' items, and M-DOC-INLINE
says third-party re-exports must never be inlined, so readers can tell they are external.
`CursorIcon` also appears in `Handler::set_mouse_cursor_icon`. That leak is acceptable under
M-DONT-LEAK-TYPES (feature-gated and the winit ecosystem's shared type), but the re-export is not
needed for it. vivido imports `vvte::ansi::cursor_icon::CursorIcon` in one place.
**Recommendation:** remove the re-export in 0.16 and have vivido depend on `cursor-icon` directly.
Short of that, drop `#[doc(inline)]` now (not breaking).

### G09 — Public types miss `Debug`, common traits, and `Parser::new`

**Evidence:** `missing_debug_implementations` flags `Parser` (`src/lib.rs` line 43), `ParamsIter`
(`src/params.rs` line 90), `Processor` (`src/ansi.rs` line 228), `StdSyncHandler` (line 393), and
`KeyboardModesApplyBehavior` (line 732).

- M-PUBLIC-DEBUG: derive or implement `Debug` for all five. `Parser` and `Processor` can omit
  large buffers from the output.
- C-COMMON-TRAITS: `Hyperlink` lacks `Clone`; `Attr`, `LineClearMode`, `ClearMode`, and
  `TabulationClearMode` lack `Clone`/`Copy`/`PartialEq`; `Rgb`, `Color`, `NamedColor`, `Mode`,
  `PrivateMode`, `CharsetIndex`, and `StandardCharset` lack `Hash`; `Params` and `ParamsIter` lack
  `Clone`; `ParamsIter` could implement `FusedIterator`.
- C-CTOR: `Parser` has only `Default`; add `Parser::new()` (`Processor::new` exists).
- M-TYPES-SEND is met: `Parser`, `Params`, and `Processor` are `Send + Sync` (checked with a
  compile-time assertion); adding that assertion to the crate would keep it so.

All of these are additive.

### G10 — Lint overrides, panic message, and casts need tidying

- `#[allow(clippy::cognitive_complexity)]` (`src/ansi.rs` line 1483) is dead: the lint is in the
  nursery group and never fires, so it would be an unfulfilled `#[expect]`. Remove it.
- `#[allow(non_snake_case)]` (line 1859) is live; make it
  `#[expect(non_snake_case, reason = "...")]` (M-LINT-OVERRIDE-EXPECT), or rename the module.
- `unreachable!()` in `change_state` (`src/lib.rs` line 159) has no message (M-PANIC-MESSAGE). It
  is unreachable today because `advance` dispatches `Ground` elsewhere; say so in the message.
- Lossy casts: besides G02's `as u8`, `as` casts in color parsing (lines 110–112, 156–172) are
  bounded by construction; switching to `From` / `try_from` where possible lets the
  `cast_possible_truncation` lint stay on.

### G11 — Limits and sizes lack a stated rationale

`MAX_INTERMEDIATES = 2`, `MAX_OSC_PARAMS = 16` (`src/lib.rs` lines 33–34), and `MAX_PARAMS = 32`
(`src/params.rs` line 5) have no comment at all; `SYNC_UPDATE_TIMEOUT = 150 ms` and
`SYNC_BUFFER_SIZE = 2 MiB` (`src/ansi.rs` lines 27–30) say what they are but not why, or what
changes if they move. The default `1024` on the const parameter means nothing (G07). Inline
values `0..256` (OSC 104, line 1455) and the `13` / `12` length checks for `CursorShape=`
(lines 1423–1426) should be a named constant and `strip_prefix`. `SYNC_RETAINED_CAPACITY` (lines
32–39) is the model to follow, minus its history sentence (G12).

### G12 — User-facing docs carry design-journey notes

M-NO-META-DESIGN-DOCUMENTATION asks for end-state documentation only:

- `Handler` doc: "XXX Should probably not provide default impls for everything…" (`src/ansi.rs`
  lines 441–442); `bell`: "Hopefully this is never implemented." (line 523).
- `Perform` doc: "I've done my best to describe them in a useful way in my own words…"
  (`src/lib.rs` lines 859–862).
- `Parser` doc: "The const parameter preserves upstream parser type specialization" (line 40).
- `SYNC_RETAINED_CAPACITY`: "The buffer used to be reserved at `SYNC_BUFFER_SIZE`…" (line 34).
- README "Batched text and selective OSC capture" (line 82) is a 0.15.3 release note; it belongs
  in a changelog. "Why the fork" is a reasonable design-principles section and can stay.

### G13 — Callback signatures use bare `bool`, `usize`, and owned `String`

`Perform::csi_dispatch` / `hook` / `esc_dispatch` take `ignore: bool`, `osc_dispatch` takes
`bell_terminated: bool`, and `apc_end` (added by vvte) takes `cancelled: bool`. `Handler` passes
color slots as bare `usize` (named colors are 256 and up), clipboard selectors as `u8`, and
allocates owned `String`s for `set_title` and `dynamic_color_sequence` on every call (M-MEM-REUSE).
Small enums (`ApcEnd::{Terminated, Cancelled}`, `OscTerminator::{Bel, St}`) and `&str` arguments
would make misuse harder for humans and agents (M-DESIGN-FOR-AI, C-NEWTYPE). All of these are
breaking for every `Perform` / `Handler` implementor, and most are upstream API that vvte keeps
for compatibility; treat this as a judgment call for a future major version, starting with the
vvte-owned `apc_end`.

### G14 — No in-crate benchmarks; DCS data is still per byte

vvte's README positions it as the hot path of a GPU terminal, but the crate has no benchmarks;
the only throughput measurement lives in vivido (`parser_throughput`), and depends on vivido's
build. M-HOTPATH asks for benchmarks around hot paths in the crate itself (criterion or divan),
which would also back G06. M-THROUGHPUT asks for batched APIs: vvte batches printable text
(`print_str`), OSC bodies, and APC data, but DCS passthrough still calls `Perform::put` once per
byte. A `put_slice(&[u8])` default method that forwards to `put`, mirroring `print_str`, would
batch DCS (sixel) without breaking implementors.

### G15 — Large module, long dispatchers, control-flow macros, tests in `src`

- `src/ansi.rs` is 2,452 lines with 23 public items in one flat module.
  `Performer::osc_dispatch` is 164 lines and `csi_dispatch` 179 (`clippy::too_many_lines`).
  Splitting the private implementation (for example OSC handling and SGR parsing into private
  submodules) is not breaking; moving public items to new paths would be (M-BALANCED-MODULES).
- `csi_dispatch` and `esc_dispatch` define local `macro_rules!` (`unhandled!`,
  `configure_charset!`) that `return` from the enclosing function (lines 1492, 1689, 1698).
  A function returning `Option<CharsetIndex>` and a plain helper would do (M-MACRO-LAST-RESORT).
- 52 of the 69 unit tests (36 of 41 in `lib.rs`, 16 of 28 in `ansi.rs`) use only public API and
  belong in `tests/` (M-INTEGRATION-TESTS); they are 1,387 of the 4,383 lines in `src/`.
  `parse_zsh_startup` has no assertion and only checks that parsing does not panic.
- M-SMALLER-CRATES would put `ansi` in its own crate over the parser. That needs
  `advance_until_terminated` to become public API and is breaking for every consumer; the
  current feature split is a defensible exception.

## Existing strengths, exceptions, and audit limits

**Strengths.** The parser does not panic on any input the audit could construct: parameter pushes
are guarded by `is_full`, CSI arithmetic saturates, and OSC parameter slots are capped. A
randomized stress run (256 MiB of state-transition-biased bytes at random split points, through
both `Parser` and `ansi::Processor`, in debug and release builds) completed without a panic. The
synchronized-update buffer is bounded and shrinks after use. Every `unsafe` block has a correct
`SAFETY` comment and Miri finds no undefined behavior. There are no statics, no `println!`, and no I/O; the clock is
injectable through the `Timeout` trait (M-MOCKABLE-SYSCALLS met). The tests are meaningful rather
than tautological, notably `bulk_and_byte_feeds_have_identical_actions` (every split point of
mixed input) and the allocation-counting test proving disabled OSC diagnostics do not allocate.
Edition 2024, MSRV 1.88 builds, rustfmt is clean and configured, all 12 feature combinations
compile, and cargo-audit reports no advisories.

**Accepted exceptions.** `CursorIcon` and `bitflags`' traits leak through the `ansi` feature;
both are ecosystem interop types behind a feature flag, which M-DONT-LEAK-TYPES allows. The
`Handler` and `Perform` traits giving every method a no-op default is intentional and lets
implementors handle only what they need.

**Limits.** This is a guideline audit, not a fuzzing campaign: the stress run is random, not
coverage-guided. Miri ran the unit tests only, not adversarial inputs. Upstream vte was not available locally, so "inherited from upstream" is
inferred from the README and the initial-import commit rather than diffed.

## Audit validation

Run on macOS (aarch64), stable 1.98.1, with scratch target directories; `Cargo.lock` was
byte-identical afterwards.

| Check | Result |
| --- | --- |
| `cargo fmt --all --check` | clean |
| `cargo test --all-targets` (default features) | 41 passed |
| `cargo test --all-targets --all-features` | 69 unit + 1 integration passed; 0 doctests |
| `cargo clippy --all-targets [--all-features] -- -D warnings` | clean |
| Guideline lint set, library code, all features | 158 warnings (see G03) |
| `cargo +1.88.0 check --all-targets --all-features` | builds |
| `cargo hack check --feature-powerset --all-targets` | 12/12 combinations build |
| `cargo audit` | no advisories |
| `cargo +nightly udeps --lib --features serde` | `serde` unused (G07) |
| `cargo +nightly miri test --lib --all-features` | no UB; all 41 parser tests (every `unsafe` block) and 26 of 28 `ansi` tests passed; stopped during `sync_bursts_buffer`, which with `the_sync_buffer_is_grown_on_demand_and_handed_back` pushes multi-MiB input through safe code only and is impractically slow under Miri |
| `RUSTDOCFLAGS="-W rustdoc::all" cargo doc --no-deps --all-features` | no warnings |
| Probe binary | reproduced G01, G02 items 1 and 3, and G05 |
| Random-input stress, debug and release | 256 MiB per parser, no panic |

## Suggested remediation order

1. G01 — bound OSC bodies (security; not breaking).
2. G02 and G03 — fix the contracts, then make the gate cover `ansi`, lints, and Miri so the rest
   lands against a real gate.
3. G05, G06, G10 — small, contained code changes.
4. G04, G09, G11, G12 — documentation and additive traits.
5. G14 and the non-breaking parts of G15 — benchmarks first, so refactors can be measured.
   Compare baseline and candidate builds by alternating runs; build layout alone moves parser
   throughput by about 3%.
6. G07, G08, G13, and the breaking parts of G15 — batch into 0.16 with matching edits to vivido,
   vvmux, and vrowser.

## Remediation record (2026-10-28)

Scope, as requested: the P1 and P2 findings (G01–G05), plus G16, a P2 defect found while fixing
G03's cast lints. All changes are backward compatible: no public item was removed or changed
signature. The version stays 0.15.3.

| ID | Status | What changed |
| --- | --- | --- |
| G01 | Fixed | `MAX_OSC_LEN` (8 MiB, rationale documented) bounds every OSC append through one helper, `osc_append`. A body that would exceed it is dropped at once through the existing `osc_discard` path: framing continues, nothing is dispatched, and the next command parses normally. `OSC_RETAINED_CAPACITY` (64 KiB) hands back the capacity a large body needed. Probe: 64 MiB of unterminated OSC 52 now leaves ~0 MiB held (was 64 MiB). Test: `osc_bodies_are_bounded` (limit exactly, limit + 1, continued input, recovery). |
| G02 | Fixed | 1. `ParamsIter::size_hint` returns `(min(1, remaining), Some(remaining))`; `tests/params.rs` checks the bounds against what is yielded for five inputs. 2. `Params::len` documents that it counts every value and that `iter().count()` counts parameters. 3. `Timeout::pending_timeout` now documents the contract the processor relies on: `true` from `set_timeout` until `clear_timeout`, deadline or not, because returning `false` early lets new output overtake held bytes; `tests/ansi.rs` pins it for `StdSyncHandler`. Behavior was already correct. 4. Kitty keyboard flags use `u8::try_from`; out-of-range values are logged as unhandled instead of truncated (`CSI = 257 u` no longer enables flag 1). |
| G03 | Fixed | `[lints.rust]` and `[lints.clippy]` tables with the full guideline set, replacing the inline `#![deny]`. Opt-outs, with reasons in `Cargo.toml`: `inline_always` (deliberate per-byte inlining), `match_same_arms` (tables kept in specification order), and `literal_string_with_formatting_args` (per the guideline). Two `#[expect(too_many_lines)]` with reasons on the OSC and CSI dispatchers (splitting them is G15). All 158 library and 171 test-code hits resolved. `package.keywords` and `categories` added. `vvte/AGENTS.md` makes the gate all features, default features, the feature powerset, audit, and Miri; the three multi-MiB tests carry `#[cfg_attr(miri, ignore)]`. No CI workflow was added: the root repository's workflows were deliberately moved out of `.github/workflows` (commit `43d1ed7eb`). |
| G04 | Fixed | No `missing_docs` hits remain. Five compiled examples (crate root, `Parser::advance`, `Params`, the `ansi` module, `Processor`, which shows the deadline check). The `ansi` module doc explains `Processor`, `Handler`, and the synchronized-update contract, with the caller's duty to call `stop_sync` in bold. Every `Handler` parameter is named and described, with its control function (CUP, ICH, DSR, OSC 52, …); color slots, color-query replies, and clipboard selectors are spelled out. Crate docs list vvte's differences and use intra-doc links. Listed typos fixed. The README example now compiles as written (`let mut parser: Parser`). |
| G05 | Fixed | No log per DCS byte (the `put` override is gone) and no `trace!` in `Rgb * f32`. The DCS start and end log once each. Every remaining diagnostic is a debug event with `event = "vvte.<kind>.<state>"` and key-value fields (`log` 0.4.29 with `kv`, which adds no dependencies); vivido's and vvmux's loggers already read `event` and fields. OSC parameters are formatted lazily, without the old `String`, and appear only in a field, which vivido redacts by default, never in the message. Probe: one 10,000-byte DCS now logs 2 records (was 10,002). Test: `tests/logging.rs` covers zero allocations when disabled, event names and fields, DCS volume, and silent color scaling. Kept: unhandled C0 bytes still log once each, since they are rare in real output. |
| G16 | Fixed | `parse_legacy_color` shifted by `4 * (len - 1)` bits after parsing the whole channel, so a channel of 17+ digits (leading zeros parse) overflowed the shift: `OSC 4;0;#` with three 17-digit channels panicked in debug builds and gave 16 instead of 0 in release. It now validates the digits and converts only the two most significant, which also stops accepting `+` signs. Test: `long_legacy_colors_keep_their_top_digits`. The randomized stress run had not found it. |

**P3 items touched only because the new lint gate required them:** G09's `Debug` for the five
types (`Parser` and `Processor` hand-written so they show sizes, not buffered data) and
`#[must_use]` on getters; G10's dead `cognitive_complexity` allow (removed), the
`non_snake_case` allow (now `#[expect]` with a reason), and its lossy casts (now `From`,
`try_from`, or an `#[expect]` explaining the clamp); G12's "XXX" note on `Handler` (replaced by
the new trait doc). G06–G08, the rest of G09–G12, and G13–G15 are untouched.

**Observable behavior changes:** OSC bodies over 8 MiB are not dispatched; kitty keyboard flags
over 255 are ignored; legacy `#…` colors reject `+` and accept channels of any length; debug log
messages and fields changed (no consumer test depends on the old text).

**Test hygiene:** the `contrast` test compared `powf` results to within `f64::EPSILON`. Miri
perturbs transcendental functions on purpose, so it failed under Miri for every seed tried. It
now uses a 1e-12 relative tolerance, which also covers math-library differences between
platforms.

**Verification (macOS aarch64, stable 1.98.1):** fmt clean; 71 unit, 5 integration, and 5 doc
tests pass with all features, and 42 unit and 2 integration with default features; clippy
`-D warnings` clean in both configurations and across `cargo hack clippy --feature-powerset`;
rustdoc `-D warnings` clean with and without features; MSRV 1.88 builds; `cargo audit` and
`cargo udeps` clean; Miri finds no undefined behavior (68 unit tests pass, the 3 multi-MiB tests
ignored under Miri); the 256 MiB random stress run is still panic-free with
an unchanged action checksum; vivido, vvmux (workspace), and vrowser pass
`cargo check --all-targets`. vrowser's build rewrote its stale `Cargo.lock` (it still recorded
vvte 0.15.0); that file was restored.

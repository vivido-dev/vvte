# vvte guidance

vvte is the terminal stream parser under vivido, vvmux (`vvmux-terminal`), and vrowser.
`src/lib.rs` is the `Parser`/`Perform` state machine, `src/params.rs` its CSI/DCS parameter
storage, and `src/ansi.rs` (feature `ansi`) the `Processor`/`Handler` layer that vivido and vvmux
use. `docs/ms-rust-gaps.md` records the guideline audit and what has been fixed against it.

## Rules

- Parser input is untrusted terminal output. Every buffer that grows with input has a named,
  documented limit (`MAX_OSC_LEN`, `SYNC_BUFFER_SIZE`) and gives back excess capacity afterwards.
  New buffering needs the same, plus a test that feeds past the limit.
- Feeding a stream in one call, in arbitrary chunks, or a byte at a time must produce the same
  actions; extend `bulk_and_byte_feeds_have_identical_actions` when adding a bulk path.
- No logging in per-byte, per-cell, or per-color paths. Diagnostics are debug-level `log` events
  with an `event` field named `vvte.<kind>.<state>`; payloads that can carry user data (OSC
  parameters, clipboard data) go in fields, never in the message.
- `Perform` and `Handler` methods keep no-op defaults, so adding one is not a breaking change;
  changing a signature is.

## Verification

Run from `vvte/`:

```sh
cargo fmt --all --check
cargo test --all-targets --all-features
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets -- -D warnings
cargo hack check --feature-powerset --all-targets
cargo audit
cargo +nightly miri test --lib --all-features
```

The default-feature runs alone skip all of `ansi`. Tests that feed megabytes are marked
`#[cfg_attr(miri, ignore)]`; mark new ones the same way. After a change, also run
`cargo check --all-targets` in vivido, vvmux, and vrowser, restoring their `Cargo.lock` files if
the build rewrites them.

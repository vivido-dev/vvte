# vvte

Terminal stream parser for [Vivido](https://github.com/vivido-dev/vivido) and related projects.

`vvte` is a fork of [`vte` 0.15.0](https://crates.io/crates/vte) (upstream revision
`3b3da71c34cc1256c7e20981cf03f8eb95e08ffc`). [`Parser`] implements Paul Williams's
[ANSI parser state machine](https://vt100.net/emu/dec_ansi_parser): it handles the bookkeeping of
parsing a byte stream into control actions, while your implementation of the [`Perform`] trait
decides what those actions mean. The parser assigns no semantics of its own, so it can sit under
any terminal emulator or stream consumer.

Like upstream, it supports UTF-8 input, allows OSC strings to be terminated by `0x07` as well as
the canonical terminator, and only supports 7-bit codes.

## Why the fork

`vvte` keeps the upstream parser and API, with three changes that matter for Vivido's use as a
long-lived GPU terminal:

- **Growable OSC buffer.** Upstream reserves a fixed 2 MiB inline array for OSC/DCS payloads in
  every parser. Here the payload lives in a heap `Vec` that grows on demand — the
  `Parser::<OSC_RAW_BUF_SIZE>` const parameter remains for API compatibility, but it no longer
  caps payload size.
- **Bulk OSC handling.** The advance loop leaves the per-byte dispatch for OSC runs and appends
  payloads in bulk, which shortens the hot path for the large in-band sequences Vivido terminals
  process.
- **Debug-gated diagnostics.** Formatting of unsupported OSC sequence diagnostics happens only
  when debug logging is enabled, so release builds never pay for it.

Upstream's `no_std` support is dropped; the parser requires `std`.

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `std`   | yes     | The standard-library parser (`Parser`/`Perform`/`Params`). |
| `ansi`  | no      | The `ansi::Processor` with synchronized-update handling, the `ansi::Handler` color/hyperlink model, and keyboard-mode flags. Adds `log`, `cursor-icon`, `bitflags`. |
| `serde` | no      | `Serialize`/`Deserialize` for the ANSI types. |

## Usage

```rust
use vvte::{Parser, Perform};

#[derive(Default)]
struct Printer;

impl Perform for Printer {
    fn print(&mut self, c: char) {
        print!("{c}");
    }

    fn execute(&mut self, byte: u8) {
        if byte == b'\n' {
            println!();
        }
    }
}

let mut parser = Parser::default();
parser.advance(&mut Printer, b"hello \x1b[1mworld\x1b[0m\r\n");
```

Feed arbitrary chunks to `Parser::advance`; UTF-8 sequences split across chunk boundaries are
reassembled internally. Implement only the callbacks you care about — every [`Perform`] method has
a no-op default.

[`Parser`]: https://docs.rs/vvte/latest/vvte/struct.Parser.html
[`Perform`]: https://docs.rs/vvte/latest/vvte/trait.Perform.html

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.

Derived from `vte` 0.15.0; `src/ansi.rs` retains its upstream Apache-2.0 license notice.

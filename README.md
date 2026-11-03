# vvte

Terminal stream parser for [Vivido](https://github.com/vivido-dev/vivido) and related projects.

[`Parser`] implements Paul Williams's [ANSI parser state machine](https://vt100.net/emu/dec_ansi_parser):
it handles the bookkeeping of parsing a byte stream into control actions, while your implementation
of the [`Perform`] trait decides what those actions mean. The parser assigns no semantics of its own,
so it can sit under any terminal emulator or stream consumer.

It supports UTF-8 input, allows OSC strings to be terminated by `0x07` as well as the canonical
terminator, and only supports 7-bit codes.

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

let mut parser: Parser = Parser::default();
parser.advance(&mut Printer, b"hello \x1b[1mworld\x1b[0m\r\n");
```

Feed arbitrary chunks to `Parser::advance`; UTF-8 sequences split across chunk boundaries are
reassembled internally. Implement only the callbacks you care about — every [`Perform`] method has
a no-op default.

## Batched text and selective OSC capture

`Perform::print_str` and `ansi::Handler::input_str` have scalar defaults. Optimized handlers can
consume borrowed printable runs without changing existing consumers. CSI parameters are consumed
in spans. The opt-in `Perform::osc_capture` hook can reject an OSC body after its command
separator; framing still runs, but rejected bodies are neither retained nor dispatched. The ANSI
adapter uses this for unsupported commands when debug logging is off. Split UTF-8 C1 controls now
dispatch identically to complete input chunks.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.

Derived from `vte` 0.15.0; `src/ansi.rs` retains its upstream Apache-2.0 license notice.

[`Parser`]: https://docs.rs/vvte/latest/vvte/struct.Parser.html
[`Perform`]: https://docs.rs/vvte/latest/vvte/trait.Perform.html

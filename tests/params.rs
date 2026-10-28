//! Contracts of the CSI parameter list seen through the public parser API.

use vvte::{Params, Parser, Perform};

/// What a CSI dispatch reported about its parameters.
#[derive(Debug, Default, PartialEq)]
struct Seen {
    params: Vec<Vec<u16>>,
    len: usize,
    hints: Vec<(usize, Option<usize>)>,
}

#[derive(Default)]
struct Recorder(Vec<Seen>);

impl Perform for Recorder {
    fn csi_dispatch(&mut self, params: &Params, _: &[u8], _: bool, _: char) {
        let mut iter = params.iter();
        let mut seen = Seen { len: params.len(), ..Seen::default() };
        loop {
            seen.hints.push(iter.size_hint());
            let Some(param) = iter.next() else { break };
            seen.params.push(param.to_vec());
        }
        self.0.push(seen);
    }
}

fn dispatch(input: &[u8]) -> Seen {
    let mut recorder = Recorder::default();
    let mut parser: Parser = Parser::default();
    parser.advance(&mut recorder, input);
    assert_eq!(recorder.0.len(), 1);
    recorder.0.remove(0)
}

#[test]
fn size_hint_bounds_the_parameters_actually_yielded() {
    for input in [
        &b"\x1b[m"[..],
        b"\x1b[1;2;3m",
        b"\x1b[38:2:255:0:255;1m",
        b"\x1b[::::::::::::::::::::::::::::::::m",
        b"\x1b[4:3;38:5:208;48:2::10:20:30m",
    ] {
        let seen = dispatch(input);
        for (step, &(lower, upper)) in seen.hints.iter().enumerate() {
            let remaining = seen.params.len() - step;
            assert!(lower <= remaining, "{input:?} step {step}: lower bound {lower} > {remaining}");
            assert!(
                upper.is_some_and(|upper| remaining <= upper),
                "{input:?} step {step}: upper bound {upper:?} < {remaining}"
            );
        }
    }
}

#[test]
fn len_counts_values_and_iter_counts_parameters() {
    let seen = dispatch(b"\x1b[1;38:2:255:0:0m");
    assert_eq!(seen.params, [vec![1], vec![38, 2, 255, 0, 0]]);
    assert_eq!(seen.len, 6);
}

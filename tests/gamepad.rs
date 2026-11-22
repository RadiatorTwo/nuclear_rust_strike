//! The gamepad bindings file: written and read back the same, unknown names reported.
use nstrike::platform::gamepad::{ACTIONS, Control, parse, render};

#[test]
fn bindings_round_trip() {
    let b = parse("", |w| panic!("{w}"));
    assert_eq!(b.len(), ACTIONS.len());
    assert_eq!(parse(&render(&b), |w| panic!("{w}")), b);
}

#[test]
fn bindings_parse() {
    let mut warnings = Vec::new();
    let b = parse("forward = rup, DUp # comment\nchaingun =\nnothing = A\nhud = Q", |w| warnings.push(w));
    assert_eq!(b[0], vec![Control::RUp, Control::DUp]);
    assert!(b[6].is_empty());
    assert_eq!(warnings.len(), 2);
    assert!(b[13].is_empty(), "an unknown control drops out");
}

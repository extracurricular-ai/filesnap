use std::num::NonZeroU64;

use filesnap::DECLARED_WINDOW_TURNS;
use filesnap::DeclaredWindow;
use pretty_assertions::assert_eq;

use super::parse;

fn turns(n: u64) -> DeclaredWindow {
    DeclaredWindow::Turns(NonZeroU64::new(n).expect("test uses a non-zero window"))
}

#[test]
fn a_turn_count_and_the_word_are_both_accepted() {
    assert_eq!(parse("1"), Ok(turns(1)));
    assert_eq!(parse("100"), Ok(turns(100)));
    assert_eq!(parse("unlimited"), Ok(DeclaredWindow::Unlimited));
    assert_eq!(parse("Unlimited"), Ok(DeclaredWindow::Unlimited));
}

/// The default the flag falls back to is the library's, not a number spelled
/// again over here. Two constants would agree until one of them changed.
#[test]
fn the_default_is_the_librarys_own() {
    assert_eq!(
        DeclaredWindow::default(),
        DeclaredWindow::Turns(DECLARED_WINDOW_TURNS)
    );
}

/// clap renders the default through `Display` and hands the string back to
/// [`parse`], so a spelling either side stops recognising is a broken flag
/// rather than a failing test somewhere far away.
#[test]
fn every_value_survives_a_round_trip_through_its_own_display() {
    for window in [
        DeclaredWindow::default(),
        turns(1),
        DeclaredWindow::Unlimited,
    ] {
        assert_eq!(parse(&window.to_string()), Ok(window));
    }
}

#[test]
fn zero_is_refused_rather_than_promoted_to_one() {
    assert_eq!(
        parse("0"),
        Err("expected a turn count of 1 or more, or `unlimited`".to_string())
    );
}

#[test]
fn what_is_neither_a_count_nor_the_word_is_refused() {
    for raw in ["", "-1", "1.5", "many", "100 turns"] {
        assert_eq!(
            parse(raw),
            Err("expected a turn count of 1 or more, or `unlimited`".to_string()),
            "{raw:?} should not parse"
        );
    }
}

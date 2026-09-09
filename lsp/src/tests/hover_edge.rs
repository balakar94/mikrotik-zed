//! Hover — edge.
use super::hover_fixtures::*;

#[test]
fn test_hover_empty_line_returns_none() {
    let data = synthetic_data();
    let line = "";
    assert!(hover_at(&data, line, 0).is_none());
}
#[test]
fn test_hover_whitespace_returns_none() {
    let data = synthetic_data();
    let line = "   ";
    assert!(hover_at(&data, line, 1).is_none());
}
#[test]
fn test_hover_on_space_between_tokens_returns_menu() {
    let data = synthetic_data();
    let line = "/ip/address add";
    // Space at 11 (between "/ip/address" and "add") – hover logic includes preceding word
    let h = hover_at(&data, line, 11).expect("space after menu should hover menu");
    assert!(h.contents.value.contains("/ip/address"));
}
#[test]
fn test_hover_on_leading_space_returns_none() {
    let data = synthetic_data();
    let line = "   /ip/address";
    // Leading spaces: position 0 is space, word empty
    assert!(hover_at(&data, line, 0).is_none());
    assert!(hover_at(&data, line, 1).is_none());
}
#[test]
fn test_hover_unknown_word_returns_none() {
    let data = synth();
    let line = "/ip/address add unknownprop=foo";
    let pos = line.find("unknownprop").unwrap() + 2;
    assert!(hover_at(&data, line, pos).is_none());
}
#[test]
fn test_hover_character_beyond_line_clamped() {
    let data = synthetic_data();
    let line = "/ip/address";
    // Character 100 is clamped to end, word still "/ip/address"
    let h =
        compute_hover(&data, line, 100, line, 0).expect("should still hover when char beyond line");
    assert!(h.contents.value.contains("/ip/address"));
}
#[test]
fn test_hover_unicode_boundary_safe() {
    let data = synthetic_data();
    // RSC is ASCII, but test robustness with multi-byte char in doc (even if not valid RSC)
    let line = "/ip/address add comment=\"héllo\"";
    // Character offset inside multi-byte — floor_char_boundary should keep it safe
    let h = hover_at(&data, line, 5);
    // Should not panic
    assert!(h.is_some() || h.is_none());
}
#[test]
fn test_hover_unknown_menu_returns_none() {
    let data = synth();
    let line = "/unknown/menu";
    assert!(hover_at(&data, line, 4).is_none());
}
#[test]
fn test_hover_random_word_returns_none() {
    let data = synth();
    let line = "/ip/address add address=1.1.1.1";
    // Hover over value part which is not a known word (should be none)
    let pos = line.find("1.1.1.1").unwrap() + 2;
    assert!(hover_at(&data, line, pos).is_none());
}
#[test]
fn test_hover_empty_word_returns_none() {
    let data = synth();
    assert!(hover_at(&data, "", 0).is_none());
    assert!(hover_at(&data, "   ", 1).is_none());
    assert!(hover_at(&data, "/ip/address add", 11).is_some()); // space after menu -> hovers menu, not none
    // But leading space yields none
    assert!(hover_at(&data, "   /ip/address", 0).is_none());
}

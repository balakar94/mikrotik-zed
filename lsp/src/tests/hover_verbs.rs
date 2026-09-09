//! Hover — verbs.
use super::hover_fixtures::*;

#[test]
fn test_hover_menu_path_unknown_returns_none_or_verb() {
    let data = synthetic_data();
    let line = "/ip/unknown";
    // "/ip/unknown" not in menu_by_path, next checks property/verb -> unknown -> None
    let h = hover_at(&data, line, 4);
    // Could be None or verb check (not a verb), so None
    assert!(
        h.is_none(),
        "unknown menu should return None, got {:?}",
        h.map(|x| x.contents.value)
    );
}
#[test]
fn test_hover_verb_add() {
    let data = synthetic_data();
    let line = "/ip/address add";
    // Use rfind to get the verb's "add", not the "add" inside "address"
    let verb_pos = line.rfind("add").unwrap() + 1;
    let h = compute_hover(&data, line, verb_pos, line, 0).expect("should hover verb add");
    assert!(h.contents.value.contains("**add**"));
    assert!(h.contents.value.contains("Creates a new entry"));
}
#[test]
fn test_hover_verb_print_without_menu() {
    let data = synthetic_data();
    // Hovering over "print" alone (no menu) should still return verb hover
    // But context path is empty, so property check fails, then verb check succeeds
    let line = "print";
    let h = hover_at(&data, line, 2).expect("should hover verb print even without menu");
    assert!(h.contents.value.contains("print"));
}
#[test]
fn test_hover_verb_case_sensitive() {
    let data = synthetic_data();
    // Verbs are now case-insensitive (RouterOS is case-insensitive for commands)
    let line = "/ip/address Add"; // capital A
    let h = hover_at(&data, line, line.find("Add").unwrap() + 1);
    assert!(
        h.is_some(),
        "verb hover is now case-insensitive — 'Add' should resolve, got None"
    );
    assert!(h.unwrap().contents.value.contains("Add"));
}
#[test]
fn test_hover_verb_case_insensitive() {
    let data = synthetic_data();
    for (line, word, gloss) in [
        ("/ip/address Add", "Add", "creates a new entry"),
        ("/ip/address PRINT", "PRINT", "lists entries (read-only)"),
        ("/ip/firewall/filter Disable", "Disable", "disables entries"),
    ] {
        let pos = line.find(word).unwrap() + 1;
        let h = hover_at(&data, line, pos)
            .unwrap_or_else(|| panic!("case-insensitive verb '{word}' should hover"));
        assert!(
            h.contents.value.contains(word),
            "hover for '{word}' must contain original casing, got: {}",
            h.contents.value
        );
        assert!(
            h.contents.value.to_lowercase().contains(gloss),
            "hover for '{word}' must describe its role ({gloss}), got: {}",
            h.contents.value
        );
    }
}
#[test]
fn test_hover_verb_shows_standard_message() {
    let data = synth();
    let line = "/ip/address add";
    let pos = line.rfind("add").unwrap() + 1;
    let h = hover_at(&data, line, pos).expect("verb hover");
    assert!(h.contents.value.contains("**add**"));
    assert!(h.contents.value.contains("Creates a new entry"));
}
#[test]
fn test_hover_verb_all_standard_verbs() {
    let data = synth();
    for verb in MenuData::STANDARD_VERBS {
        let line = format!("/ip/address {verb}");
        let pos = line.find(verb).unwrap() + 1;
        let h = hover_at(&data, &line, pos);
        assert!(h.is_some(), "verb {verb} should hover");
        assert!(h.unwrap().contents.value.contains(verb));
    }
}
#[test]
fn test_hover_verb_without_menu_also_works() {
    let data = synth();
    let h = hover_at(&data, "print", 2).expect("bare verb");
    assert!(h.contents.value.contains("print"));
}

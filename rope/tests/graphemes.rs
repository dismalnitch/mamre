//! Grapheme cluster boundaries, found chunk by chunk, must be exactly
//! unicode-segmentation's on the whole text: at every position, with chunks
//! small enough that clusters straddle seams all the time.

use mamre_rope::{Plain, Rope};
use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

/// Pieces that make awkward clusters, and the plain text around them.
const PIECES: &[&str] = &[
    "a",
    "Z",
    " ",
    "\n",
    "\r",
    "\r\n",
    // A letter and combining accents.
    "e\u{301}",
    "\u{301}",
    "o\u{308}\u{304}",
    // Emoji: modifiers, joiners, flags (regional indicators pair up).
    "👍",
    "👍🏽",
    "👨\u{200d}👩\u{200d}👧",
    "\u{200d}",
    "🇺🇸",
    "🇬",
    "🇧",
    // Hangul, composed and as jamo.
    "한",
    "\u{1100}\u{1161}\u{11a8}",
    "\u{1100}",
    "\u{1161}",
    // Devanagari: a conjunct and a vowel sign.
    "क\u{94d}ष",
    "ि",
    "न",
    // Prepend and spacing marks, and a lone variation selector.
    "\u{600}",
    "\u{903}",
    "\u{fe0f}",
];

fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(prop::sample::select(PIECES), 0..60).prop_map(|p| p.concat())
}

fn check<const N: usize>(text: &str) -> Result<(), TestCaseError> {
    let rope: Rope<Plain, N> = Rope::from(text);
    let mut bounds: Vec<usize> = text.grapheme_indices(true).map(|(i, _)| i).collect();
    bounds.push(text.len());
    for (at, _) in text.char_indices().chain([(text.len(), ' ')]) {
        let is = bounds.binary_search(&at).is_ok();
        prop_assert_eq!(rope.is_grapheme_boundary(at), is, "is_boundary at {}", at);
        let next = bounds.iter().copied().find(|&b| b > at);
        prop_assert_eq!(rope.next_grapheme_boundary(at), next, "next from {}", at);
        let prev = bounds.iter().copied().rev().find(|&b| b < at);
        prop_assert_eq!(rope.prev_grapheme_boundary(at), prev, "prev from {}", at);
    }
    // The chunks, walked by chunk_at, are the text.
    let mut at = 0;
    let mut walked = String::new();
    while at < text.len() {
        let (start, chunk) = rope.chunk_at(at).expect("not empty");
        prop_assert_eq!(start, at);
        walked.push_str(chunk);
        at += chunk.len();
    }
    prop_assert_eq!(walked, text);
    Ok(())
}

proptest! {
    #[test]
    fn tiny_chunks(t in text()) { check::<4>(&t)?; }

    #[test]
    fn small_chunks(t in text()) { check::<9>(&t)?; }

    #[test]
    fn default_chunks(t in text()) { check::<512>(&t)?; }
}

#[test]
fn an_empty_rope_has_only_its_ends() {
    let rope: Rope = Rope::new();
    assert!(rope.is_grapheme_boundary(0));
    assert_eq!(rope.next_grapheme_boundary(0), None);
    assert_eq!(rope.prev_grapheme_boundary(0), None);
    assert_eq!(rope.chunk_at(0), None);
}

#[test]
fn a_family_is_one_cluster_across_chunks() {
    let family = "👨\u{200d}👩\u{200d}👧";
    let text = format!("a{family}b");
    let rope: Rope<Plain, 4> = Rope::from(text.as_str());
    assert_eq!(rope.next_grapheme_boundary(1), Some(1 + family.len()));
    assert_eq!(rope.prev_grapheme_boundary(1 + family.len()), Some(1));
    assert!(!rope.is_grapheme_boundary(5));
}

#[test]
#[should_panic(expected = "not a char boundary")]
fn inside_a_char_panics() {
    let rope: Rope = Rope::from("日本");
    rope.next_grapheme_boundary(1);
}

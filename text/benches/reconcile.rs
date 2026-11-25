//! Reconciling the 4 MB book (the rope's section bench's) with a copy
//! another tool edited (#35): words changed in a few paragraphs, a
//! paragraph added, a chapter moved.
//!
//! `cargo bench -p mamre-text --bench reconcile`

use std::time::Instant;

use mamre_text::{Edit, Text};

fn book() -> String {
    let mut out = String::new();
    let para = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ";
    for c in 1..=40 {
        out.push_str(&format!("# Chapter {c}\n\n"));
        for s in 1..=25 {
            out.push_str(&format!("## Section {c}.{s}\n\n"));
            for p in 0..4 {
                // Each paragraph its own, as a book's are.
                out.push_str(&format!("({c}.{s}.{p}) "));
                out.push_str(&para.repeat(8));
                out.push_str("\n\n");
            }
        }
    }
    out
}

fn main() {
    let text = book();
    let mut t = Text::new("bench");
    t.insert(0, &text).unwrap();
    // Edited elsewhere: a word in three paragraphs, a new paragraph, and
    // chapter 10 moved to the end.
    let mut edited = text
        .replacen("(3.4.1) Lorem", "(3.4.1) Lorum", 1)
        .replacen("(17.2.0) Lorem ipsum", "(17.2.0) Ipsum lorem", 1)
        .replacen("(30.9.3) Lorem", "(30.9.3) Dolorem", 1)
        .replacen(
            "## Section 5.5\n\n",
            "## Section 5.5\n\nA new paragraph.\n\n",
            1,
        );
    let ch10 = edited.find("# Chapter 10\n").unwrap();
    let ch11 = edited.find("# Chapter 11\n").unwrap();
    let chapter: String = edited[ch10..ch11].to_string();
    edited.replace_range(ch10..ch11, "");
    edited.push_str(&chapter);
    let start = Instant::now();
    let edits = t.reconcile(&edited);
    let took = start.elapsed();
    assert_eq!(t.to_string(), edited);
    let moves = edits
        .iter()
        .filter(|e| matches!(e, Edit::Move { .. }))
        .count();
    let typed: usize = edits
        .iter()
        .map(|e| match e {
            Edit::Insert { text, .. } => text.len(),
            _ => 0,
        })
        .sum();
    println!(
        "book: {:.1} MB; reconciled in {took:.2?}: {} edits, {moves} moves, {typed} bytes typed",
        text.len() as f64 / 1e6,
        edits.len()
    );
}

//! Paragraph kinds: a mark on the line break that starts a paragraph.

use mamre_text::{Kind, Text, TextError};

fn text(s: &str) -> Text {
    let mut t = Text::new("a");
    t.insert(0, s).unwrap();
    t
}

fn sync(a: &mut Text, b: &mut Text) {
    let mut to_b = vec![a.hello()];
    let mut to_a = vec![b.hello()];
    while !to_a.is_empty() || !to_b.is_empty() {
        let mut next_a = Vec::new();
        for m in to_b.drain(..) {
            next_a.extend(b.receive(&m).unwrap().reply);
        }
        for m in to_a.drain(..) {
            to_b.extend(a.receive(&m).unwrap().reply);
        }
        to_a = next_a;
    }
}

/// Each paragraph's kind and text.
fn shape(t: &Text) -> Vec<(Kind, String)> {
    let s = t.to_string();
    t.paragraphs()
        .into_iter()
        .map(|p| (p.kind, s[p.range].to_string()))
        .collect()
}

fn para(kind: Kind, s: &str) -> (Kind, String) {
    (kind, s.to_string())
}

use Kind::{Code, Equation, Heading, Quote, Text as Plain};

#[test]
fn a_kind_is_not_text_and_stays_with_its_paragraph() {
    let mut t = text("\nTitle\nbody");
    t.set_kind(3, Heading(1)).unwrap();
    assert_eq!(t.to_string(), "\nTitle\nbody");
    assert_eq!(
        shape(&t),
        [
            para(Plain, ""),
            para(Heading(1), "Title"),
            para(Plain, "body")
        ]
    );
    assert_eq!(
        (t.kind_at(1), t.kind_at(6), t.kind_at(7), t.kind_at(11)),
        (Heading(1), Heading(1), Plain, Plain)
    );

    // Typed into, at its start and before it: still the heading.
    t.insert(1, "The ").unwrap();
    t.insert(0, "x").unwrap();
    assert_eq!(
        shape(&t),
        [
            para(Plain, "x"),
            para(Heading(1), "The Title"),
            para(Plain, "body")
        ]
    );

    // Set again: the last one stands. Text takes the mark away.
    t.set_kind(5, Quote).unwrap();
    assert_eq!(t.kind_at(5), Quote);
    t.set_kind(5, Plain).unwrap();
    assert_eq!(t.kind_at(5), Plain);
    assert!(t.paragraphs().iter().all(|p| p.id.is_none()));
}

#[test]
fn an_empty_paragraph_has_a_kind_before_anything_is_typed() {
    let mut t = text("\none\n");
    t.set_kind(5, Code).unwrap();
    assert_eq!(
        shape(&t),
        [para(Plain, ""), para(Plain, "one"), para(Code, "")]
    );
    t.insert(5, "let x;").unwrap();
    assert_eq!(shape(&t)[2], para(Code, "let x;"));
}

#[test]
fn text_before_the_first_break_takes_no_kind() {
    let mut t = text("opening\nnext");
    assert_eq!(
        t.set_kind(3, Quote).unwrap_err(),
        TextError::NoBreak { at: 3 }
    );
    assert_eq!(t.kind_at(3), Plain);
    // The cursor before a break is in the paragraph that break ends.
    assert_eq!(
        t.set_kind(7, Quote).unwrap_err(),
        TextError::NoBreak { at: 7 }
    );
    t.set_kind(8, Quote).unwrap();
    assert_eq!(shape(&t), [para(Plain, "opening"), para(Quote, "next")]);
}

#[test]
fn return_and_backspace_do_what_a_writer_expects() {
    let mut t = text("\nHeading\nbody");
    t.set_kind(1, Heading(2)).unwrap();

    // Return at the heading's end: a plain paragraph under it.
    t.insert(8, "\n").unwrap();
    assert_eq!(
        shape(&t),
        [
            para(Plain, ""),
            para(Heading(2), "Heading"),
            para(Plain, ""),
            para(Plain, "body")
        ]
    );

    // Return in its middle: the first half is still the heading.
    t.insert(5, "\n").unwrap();
    assert_eq!(
        shape(&t)[1..3],
        [para(Heading(2), "Head"), para(Plain, "ing")]
    );

    // Backspace at the start of the second half: joined, a heading again.
    t.delete(5..6).unwrap();
    assert_eq!(shape(&t)[1], para(Heading(2), "Heading"));

    // Backspace at the start of the heading: it joins what's above, and
    // its kind goes with its break.
    t.insert(0, "above").unwrap();
    t.delete(5..6).unwrap();
    assert_eq!(shape(&t)[0], para(Plain, "aboveHeading"));
}

#[test]
fn a_section_moves_whole_with_the_kinds_in_it() {
    let mut t = text("\nOne\nplain\nquoted\nTwo\n2+2");
    let s = t.to_string();
    let at = |w: &str| s.find(w).unwrap();
    let (one, _) = t.set_kind(at("One"), Heading(1)).unwrap();
    t.set_kind(at("quoted"), Quote).unwrap();
    let (two, _) = t.set_kind(at("Two"), Heading(1)).unwrap();
    t.set_kind(at("2+2"), Equation).unwrap();

    // A section starts at its break and a quote in it doesn't end it.
    assert_eq!(t.section_range(one), Some(at("One") - 1..at("Two") - 1));
    assert_eq!(t.section_range(two), Some(at("Two") - 1..s.len()));

    t.move_section(two, 0).unwrap();
    assert_eq!(t.to_string(), "\nTwo\n2+2\nOne\nplain\nquoted");
    assert_eq!(
        shape(&t),
        [
            para(Plain, ""),
            para(Heading(1), "Two"),
            para(Equation, "2+2"),
            para(Heading(1), "One"),
            para(Plain, "plain"),
            para(Quote, "quoted"),
        ]
    );
}

#[test]
fn two_writers_agree_and_a_file_keeps_kinds() {
    let mut a = text("\nfirst\nsecond");
    let mut b = Text::new("b");
    sync(&mut a, &mut b);

    // Both set the same paragraph at once; one types in the other.
    a.set_kind(1, Heading(1)).unwrap();
    b.set_kind(1, Quote).unwrap();
    b.insert(13, "!").unwrap();
    a.set_kind(8, Code).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(shape(&a), shape(&b));
    assert!(matches!(a.kind_at(1), Heading(1) | Quote));
    assert_eq!(shape(&a)[2], para(Code, "second!"));

    let mut file = Vec::new();
    a.save(&mut file);
    let back = Text::open("c", &file).unwrap();
    assert_eq!(shape(&back), shape(&a));
}

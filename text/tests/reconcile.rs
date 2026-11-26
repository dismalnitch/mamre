//! Edits made outside mamre (#35): reconciling a text with what another
//! tool made of it keeps the characters that didn't change, with their
//! anchors and sections.

use mamre_text::{Edit, Text};
use proptest::prelude::*;

fn text(s: &str) -> Text {
    let mut t = Text::new("me");
    t.insert(0, s).unwrap();
    t
}

#[test]
fn edits_at_the_ends_keep_the_middle() {
    let mut t = text("Intro\nMiddle para\nEnd\n");
    let at = t.to_string().find("para").unwrap();
    let a = t.anchor(at).unwrap();
    let (sec, _) = t
        .mark_section(t.to_string().find("Middle").unwrap(), 1)
        .unwrap();
    t.reconcile("A new first line\nIntro\nMiddle para\nEnd, changed\n");
    assert_eq!(
        t.to_string(),
        "A new first line\nIntro\nMiddle para\nEnd, changed\n"
    );
    assert_eq!(t.position(&a), Some(t.to_string().find("para").unwrap()));
    let sections = t.sections();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].id, sec);
    assert_eq!(sections[0].at, t.to_string().find("Middle").unwrap());
}

#[test]
fn a_moved_paragraph_is_a_move() {
    let mut t = text("One.\nTwo, which moves.\nThree.\nFour.\n");
    let (two, _) = t
        .mark_section(t.to_string().find("Two").unwrap(), 1)
        .unwrap();
    let a = t.anchor(t.to_string().find("moves").unwrap()).unwrap();
    let edits = t.reconcile("One.\nThree.\nFour.\nTwo, which moves.\n");
    assert_eq!(t.to_string(), "One.\nThree.\nFour.\nTwo, which moves.\n");
    assert!(
        edits.iter().any(|e| matches!(e, Edit::Move { .. })),
        "{edits:?}"
    );
    assert!(
        !edits.iter().any(|e| matches!(e, Edit::Insert { .. })),
        "{edits:?}"
    );
    assert_eq!(t.sections()[0].id, two);
    assert_eq!(t.sections()[0].at, t.to_string().find("Two").unwrap());
    assert_eq!(t.position(&a), Some(t.to_string().find("moves").unwrap()));
}

#[test]
fn repeated_paragraphs_are_pinned_by_their_neighbours() {
    let mut t = text("A\nsame\nB\nsame\nC\n");
    // An anchor in the second "same".
    let second = t.to_string().rfind("same").unwrap() + 2;
    let a = t.anchor(second).unwrap();
    t.reconcile("A\nsame\nB\nsame\nnew\nC\n");
    assert_eq!(t.to_string(), "A\nsame\nB\nsame\nnew\nC\n");
    assert_eq!(
        t.position(&a),
        Some(t.to_string().rfind("same").unwrap() + 2)
    );
}

#[test]
fn a_typo_fixed_inside_a_paragraph_keeps_the_rest() {
    let mut t = text("It was teh best of times.\n");
    let a = t.anchor(t.to_string().find("best").unwrap()).unwrap();
    let edits = t.reconcile("It was the best of times.\n");
    assert_eq!(t.to_string(), "It was the best of times.\n");
    assert_eq!(t.position(&a), Some(t.to_string().find("best").unwrap()));
    // A character or two, not the line.
    let typed: usize = edits
        .iter()
        .map(|e| match e {
            Edit::Insert { text, .. } => text.len(),
            _ => 0,
        })
        .sum();
    assert!(typed <= 2, "{edits:?}");
}

#[test]
fn a_whole_file_replaced_keeps_nothing() {
    let mut t = text("all of\nthis\n");
    let (sec, _) = t.mark_section(0, 1).unwrap();
    t.reconcile("something\nelse entirely\n");
    assert_eq!(t.to_string(), "something\nelse entirely\n");
    assert!(t.sections().iter().all(|s| s.id != sec));
}

#[test]
fn reopening_a_file_edited_elsewhere() {
    let mut t = text("# Title\nfirst paragraph\nsecond paragraph\n");
    let (title, _) = t.mark_section(0, 1).unwrap();
    let mut file = Vec::new();
    t.save(&mut file);
    // Another tool edits the plain text.
    let edited = "# Title\nsecond paragraph\nfirst paragraph, longer now\n";
    let mut back = Text::open("me", &file).unwrap();
    back.reconcile(edited);
    assert_eq!(back.to_string(), edited);
    assert_eq!(back.sections()[0].id, title);
}

// ---- Any edits ----

#[derive(Clone, Debug)]
enum LineEdit {
    Insert(prop::sample::Index, String),
    Delete(prop::sample::Index),
    Move(prop::sample::Index, prop::sample::Index),
    Retype(prop::sample::Index, prop::sample::Index, char),
    Duplicate(prop::sample::Index),
}

fn line() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec!['a', 'b', 'c', ' ', 'é', '.']),
        0..12,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

fn line_edit() -> impl Strategy<Value = LineEdit> {
    let i = || any::<prop::sample::Index>();
    prop_oneof![
        2 => (i(), line()).prop_map(|(p, l)| LineEdit::Insert(p, l)),
        2 => i().prop_map(LineEdit::Delete),
        2 => (i(), i()).prop_map(|(a, b)| LineEdit::Move(a, b)),
        2 => (i(), i(), prop::char::range('x', 'z')).prop_map(|(l, p, c)| LineEdit::Retype(l, p, c)),
        1 => i().prop_map(LineEdit::Duplicate),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Whatever another tool did, reconciling makes the text it made, and
    /// a line it didn't touch, unique on both sides, keeps its characters.
    #[test]
    fn reconciling_makes_the_incoming_text(
        start in prop::collection::vec(line(), 0..12),
        edits in prop::collection::vec(line_edit(), 0..8),
    ) {
        let mut lines: Vec<String> = start.iter().map(|l| format!("{l}\n")).collect();
        let old: String = lines.concat();
        let mut touched = vec![false; lines.len()];
        for e in edits {
            let n = lines.len();
            match e {
                LineEdit::Insert(p, l) => {
                    let at = p.index(n + 1);
                    lines.insert(at, format!("{l}\n"));
                    touched.insert(at, true);
                }
                LineEdit::Delete(p) if n > 0 => {
                    let at = p.index(n);
                    lines.remove(at);
                    touched.remove(at);
                }
                LineEdit::Move(a, b) if n > 0 => {
                    let l = lines.remove(a.index(n));
                    let t = touched.remove(a.index(n));
                    let to = b.index(n);
                    lines.insert(to, l);
                    touched.insert(to, t);
                }
                LineEdit::Retype(l, p, c) if n > 0 => {
                    let k = l.index(n);
                    let mut cs: Vec<char> = lines[k].chars().collect();
                    let at = p.index(cs.len());
                    cs.insert(at, c);
                    lines[k] = cs.into_iter().collect();
                    touched[k] = true;
                }
                LineEdit::Duplicate(p) if n > 0 => {
                    let k = p.index(n);
                    lines.insert(k, lines[k].clone());
                    touched.insert(k, true);
                }
                _ => {}
            }
        }
        let new: String = lines.concat();
        let mut t = text(&old);
        // An anchor at the start of each old line.
        let mut anchors = Vec::new();
        let mut at = 0;
        for l in old.split_inclusive('\n') {
            anchors.push((l.to_string(), t.anchor(at).unwrap()));
            at += l.len();
        }
        t.reconcile(&new);
        prop_assert_eq!(t.to_string(), new.clone());
        // An untouched line unique on both sides: its anchor is at its start.
        let count = |s: &str, l: &str| s.split_inclusive('\n').filter(|x| *x == l).count();
        for (l, a) in &anchors {
            if l.len() > 1 && count(&old, l) == 1 && count(&new, l) == 1 {
                let k = lines.iter().position(|x| x == l).unwrap();
                if !touched[k] {
                    let want: usize = lines[..k].iter().map(|x| x.len()).sum();
                    prop_assert_eq!(t.position(a), Some(want), "line {:?} in {:?} -> {:?}", l, old, new);
                }
            }
        }
    }
}

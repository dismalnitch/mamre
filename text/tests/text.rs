//! mamre-text, as an editor uses it: local edits in bytes, replicas
//! talking in messages, anchors, sections, files.
//!
//! Each replica here has an "editor": a plain `String` kept up to date
//! only from the edits the replica hands back, as an editor's layout
//! would be. It must always read as the text does.

use mamre_text::{Anchor, BadMessage, Edit, Text, TextError};
use proptest::prelude::*;

/// A replica and its editor's copy of the text.
struct Replica {
    text: Text,
    editor: String,
}

impl Replica {
    fn new(name: &str) -> Self {
        Replica {
            text: Text::new(name),
            editor: String::new(),
        }
    }

    fn apply(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            apply(&mut self.editor, &edit);
        }
        assert_eq!(
            self.editor,
            self.text.to_string(),
            "the edits are the text's"
        );
    }
}

fn apply(s: &mut String, edit: &Edit) {
    match edit {
        Edit::Insert { at, text } => s.insert_str(*at, text),
        Edit::Delete { range } => {
            s.replace_range(range.clone(), "");
        }
        Edit::Move { range, to } => {
            let moved: String = s[range.clone()].to_owned();
            s.replace_range(range.clone(), "");
            let at = if *to > range.start {
                to - range.len()
            } else {
                *to
            };
            s.insert_str(at, &moved);
        }
        Edit::Section { .. } | Edit::Isolates | Edit::Lost { .. } => {}
    }
}

/// Sends everything each has to the other until neither has more to say.
fn sync(a: &mut Replica, b: &mut Replica) {
    let mut to_b = vec![a.text.hello()];
    let mut to_a = vec![b.text.hello()];
    for _ in 0..20 {
        if to_a.is_empty() && to_b.is_empty() {
            return;
        }
        for m in std::mem::take(&mut to_b) {
            let got = b.text.receive(&m).unwrap();
            to_a.extend(got.reply);
            b.apply(got.edits);
        }
        for m in std::mem::take(&mut to_a) {
            let got = a.text.receive(&m).unwrap();
            to_b.extend(got.reply);
            a.apply(got.edits);
        }
    }
    panic!("they kept talking");
}

#[test]
fn edits_are_in_bytes() {
    let mut r = Replica::new("me");
    let e = r.text.insert(0, "héllo").unwrap();
    r.apply(e);
    let e = r.text.insert(6, " wörld 🦀").unwrap();
    r.apply(e);
    assert_eq!(r.text.to_string(), "héllo wörld 🦀");
    let e = r.text.replace(1..3, "e").unwrap();
    r.apply(e);
    assert_eq!(r.text.to_string(), "hello wörld 🦀");
    assert_eq!(
        r.text.insert(8, "x"),
        Err(TextError::NotCharBoundary { at: 8 }),
        "inside ö"
    );
    assert_eq!(
        r.text.delete(13..15),
        Err(TextError::NotCharBoundary { at: 15 })
    );
    let len = r.text.len();
    assert_eq!(
        r.text.insert(len + 1, "x"),
        Err(TextError::OutOfBounds { at: len + 1, len })
    );
    assert_eq!(
        r.text.to_string(),
        "hello wörld 🦀",
        "a refused edit changes nothing"
    );
    let e = r.text.move_text(0..6, len).unwrap();
    r.apply(e);
    assert_eq!(r.text.to_string(), "wörld 🦀hello ");
}

#[test]
fn two_writers_meet() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "the cat sat").unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    assert_eq!(b.text.to_string(), "the cat sat");
    // At once: a renames the cat, b adds where it sat.
    let e = a.text.replace(4..7, "dog").unwrap();
    a.apply(e);
    let e = b.text.insert(11, " on the mat").unwrap();
    b.apply(e);
    sync(&mut a, &mut b);
    assert_eq!(a.text.to_string(), "the dog sat on the mat");
    assert_eq!(b.text.to_string(), a.text.to_string());
}

/// Typing in a paragraph while someone else moves it: the typing goes
/// where the paragraph went.
#[test]
fn typing_follows_a_moved_paragraph() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "One.\nTwo.\n").unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    let e = a.text.move_text(5..10, 0).unwrap();
    a.apply(e);
    assert_eq!(a.text.to_string(), "Two.\nOne.\n");
    let e = b.text.insert(8, "!!").unwrap();
    b.apply(e);
    assert_eq!(b.text.to_string(), "One.\nTwo!!.\n");
    sync(&mut a, &mut b);
    assert_eq!(a.text.to_string(), "Two!!.\nOne.\n");
    assert_eq!(b.text.to_string(), a.text.to_string());
}

#[test]
fn anchors_stay_put() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "abcdef").unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    // a's caret between "c" and "d"; a selection end at the end.
    let caret: Anchor = a.text.anchor(3).unwrap();
    let end = a.text.anchor(6).unwrap();
    let start = a.text.anchor(0).unwrap();
    let e = b.text.insert(0, "XYZ").unwrap();
    b.apply(e);
    let e = b.text.move_text(3..6, 9).unwrap();
    b.apply(e);
    sync(&mut a, &mut b);
    assert_eq!(a.text.to_string(), "XYZdefabc");
    assert_eq!(
        a.text.position(&caret),
        Some(3),
        "before d, wherever it went"
    );
    assert_eq!(a.text.position(&end), Some(6), "after f");
    // b deletes "d": the caret goes to just after "c".
    let e = b.text.delete(3..4).unwrap();
    b.apply(e);
    sync(&mut a, &mut b);
    assert_eq!(a.text.to_string(), "XYZefabc");
    assert_eq!(a.text.position(&caret), Some(8), "after c, at the end");
    assert_eq!(a.text.position(&start), Some(5), "before a");
    a.text.release(caret);
    a.text.release(end);
    a.text.release(start);
}

#[test]
fn sections_are_marks() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "Intro\nbody\nMore\nend\n").unwrap();
    a.apply(e);
    let (intro, e) = a.text.mark_section(0, 1).unwrap();
    a.apply(e);
    let (more, e) = a.text.mark_section(11, 1).unwrap();
    a.apply(e);
    assert_eq!(a.text.section_range(intro), Some(0..11));
    assert_eq!(a.text.section_range(more), Some(11..20));
    sync(&mut a, &mut b);
    let levels: Vec<(usize, u8)> = b.text.sections().iter().map(|s| (s.at, s.level)).collect();
    assert_eq!(levels, vec![(0, 1), (11, 1)]);
    // b moves "More" to the top; a's ids still name the same sections.
    let more_there = b.text.sections()[1].id;
    let e = b.text.move_section(more_there, 0).unwrap();
    b.apply(e);
    sync(&mut a, &mut b);
    assert_eq!(a.text.to_string(), "More\nend\nIntro\nbody\n");
    assert_eq!(a.text.section_range(more), Some(0..9));
    assert_eq!(a.text.section_range(intro), Some(9..20));
}

/// A section marked elsewhere arrives as an edit.
#[test]
fn a_remote_section_arrives_as_an_edit() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "Title\ntext").unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    a.text.mark_section(0, 2).unwrap();
    let got = b
        .text
        .receive(&a.text.receive(&b.text.hello()).unwrap().reply[0])
        .unwrap();
    assert!(
        got.edits.iter().any(|e| matches!(
            e,
            Edit::Section {
                at: Some(0),
                level: 2,
                ..
            }
        )),
        "{:?}",
        got.edits
    );
}

#[test]
fn a_bad_message_changes_nothing() {
    let mut a = Replica::new("a");
    let e = a.text.insert(0, "hi").unwrap();
    a.apply(e);
    let mut m = a.text.hello();
    m[2] ^= 0xff;
    assert_eq!(a.text.receive(&m), Err(BadMessage));
    assert_eq!(a.text.receive(b"nonsense"), Err(BadMessage));
    assert_eq!(a.text.to_string(), "hi");
}

#[test]
fn files_keep_everything() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "first draft").unwrap();
    a.apply(e);
    let mut file = Vec::new();
    a.text.save(&mut file);
    let e = a.text.replace(0..5, "second").unwrap();
    a.apply(e);
    let (title, _) = a.text.mark_section(0, 1).unwrap();
    a.text.save(&mut file);
    // Opened again, on another day: the text, its sections, its history.
    let mut again = Replica::new("a");
    again.text = Text::open("a", &file).unwrap();
    again.editor = again.text.to_string();
    assert_eq!(again.text.to_string(), "second draft");
    assert_eq!(again.text.sections().len(), 1);
    assert_eq!(again.text.section_range(title), Some(0..12));
    // And it syncs, with all of it.
    sync(&mut again, &mut b);
    assert_eq!(b.text.to_string(), "second draft");
    let e = again.text.insert(12, "!").unwrap();
    again.apply(e);
    again.text.save(&mut file);
    assert_eq!(Text::open("a", &file).unwrap().to_string(), "second draft!");
    assert!(Text::open("a", b"not a file").is_err());
}

// ---- Many writers, a bad network ----

#[derive(Clone, Debug)]
enum Act {
    Insert(usize, prop::sample::Index, String),
    Delete(usize, prop::sample::Index, prop::sample::Index),
    Move(
        usize,
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Mark(usize, prop::sample::Index, u8),
    /// A replica says hello to another.
    Hello(usize, usize),
    /// A message in flight arrives (or arrives again, if `keep`).
    Deliver(prop::sample::Index, bool),
    /// A message in flight is lost.
    Drop(prop::sample::Index),
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let text = prop::collection::vec(
        prop::sample::select(vec!["a", "b", "é", "🦀", "\n", " "]),
        1..4,
    )
    .prop_map(|v| v.concat());
    let act = prop_oneof![
        4 => (0..3usize, any::<prop::sample::Index>(), text).prop_map(|(r, p, t)| Act::Insert(r, p, t)),
        2 => (0..3usize, any::<prop::sample::Index>(), any::<prop::sample::Index>()).prop_map(|(r, a, b)| Act::Delete(r, a, b)),
        2 => (0..3usize, any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(r, a, b, t)| Act::Move(r, a, b, t)),
        1 => (0..3usize, any::<prop::sample::Index>(), 0u8..3).prop_map(|(r, p, l)| Act::Mark(r, p, l)),
        3 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Hello(a, b)),
        6 => (any::<prop::sample::Index>(), any::<bool>()).prop_map(|(i, k)| Act::Deliver(i, k)),
        1 => any::<prop::sample::Index>().prop_map(Act::Drop),
    ];
    prop::collection::vec(act, 1..80)
}

/// The char boundaries of `s`: every byte offset an edit can name.
fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices().map(|(i, _)| i).chain([s.len()]).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Three replicas editing at once over a network that drops,
    /// duplicates and reorders: each editor's copy, kept only from the
    /// edits handed back, always reads as its text; and once everything's
    /// delivered, all three read the same.
    #[test]
    fn replicas_converge(acts in acts()) {
        let mut rs: Vec<Replica> = ["a", "b", "c"].iter().map(|n| Replica::new(n)).collect();
        // In flight: (to, message).
        let mut wire: Vec<(usize, Vec<u8>)> = Vec::new();
        for act in acts {
            match act {
                Act::Insert(r, p, t) => {
                    let at = *p.get(&boundaries(&rs[r].editor));
                    let e = rs[r].text.insert(at, &t).unwrap();
                    rs[r].apply(e);
                }
                Act::Delete(r, a, b) => {
                    let bs = boundaries(&rs[r].editor);
                    let (a, b) = (*a.get(&bs), *b.get(&bs));
                    let e = rs[r].text.delete(a.min(b)..a.max(b)).unwrap();
                    rs[r].apply(e);
                }
                Act::Move(r, a, b, t) => {
                    let bs = boundaries(&rs[r].editor);
                    let (a, b, t) = (*a.get(&bs), *b.get(&bs), *t.get(&bs));
                    let e = rs[r].text.move_text(a.min(b)..a.max(b), t).unwrap();
                    rs[r].apply(e);
                }
                Act::Mark(r, p, level) => {
                    let bs = boundaries(&rs[r].editor);
                    if bs.len() > 1 {
                        let at = *p.get(&bs[..bs.len() - 1]);
                        let (_, e) = rs[r].text.mark_section(at, level).unwrap();
                        rs[r].apply(e);
                    }
                }
                Act::Hello(a, b) if a != b => wire.push((b, rs[a].text.hello())),
                Act::Hello(..) => {}
                Act::Deliver(i, keep) if !wire.is_empty() => {
                    let k = i.index(wire.len());
                    let (to, m) = if keep { wire[k].clone() } else { wire.remove(k) };
                    // Who it's from isn't on the wire: answers go to everyone.
                    let got = rs[to].text.receive(&m).unwrap();
                    rs[to].apply(got.edits);
                    for reply in got.reply {
                        for other in (0..3).filter(|&o| o != to) {
                            wire.push((other, reply.clone()));
                        }
                    }
                }
                Act::Drop(i) if !wire.is_empty() => {
                    let k = i.index(wire.len());
                    wire.remove(k);
                }
                _ => {}
            }
        }
        // The network heals.
        for a in 0..3 {
            for b in 0..3 {
                if a != b {
                    let (x, y) = if a < b {
                        let (l, r) = rs.split_at_mut(b);
                        (&mut l[a], &mut r[0])
                    } else {
                        let (l, r) = rs.split_at_mut(a);
                        (&mut r[0], &mut l[b])
                    };
                    sync(x, y);
                }
            }
        }
        let texts: Vec<String> = rs.iter().map(|r| r.text.to_string()).collect();
        prop_assert_eq!(&texts[0], &texts[1]);
        prop_assert_eq!(&texts[1], &texts[2]);
        let sections: Vec<Vec<(usize, u8)>> = rs
            .iter()
            .map(|r| r.text.sections().iter().map(|s| (s.at, s.level)).collect())
            .collect();
        prop_assert_eq!(&sections[0], &sections[1]);
        prop_assert_eq!(&sections[1], &sections[2]);
    }
}

/// One hello is enough: the side that's behind asks for the rest itself.
#[test]
fn one_hello_is_enough() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "from a").unwrap();
    a.apply(e);
    let e = b.text.insert(0, "from b ").unwrap();
    b.apply(e);
    // Only a speaks first; everything after is answers.
    let mut to_b = vec![a.text.hello()];
    let mut to_a: Vec<Vec<u8>> = Vec::new();
    for _ in 0..10 {
        for m in std::mem::take(&mut to_b) {
            let got = b.text.receive(&m).unwrap();
            to_a.extend(got.reply);
            b.apply(got.edits);
        }
        for m in std::mem::take(&mut to_a) {
            let got = a.text.receive(&m).unwrap();
            to_b.extend(got.reply);
            a.apply(got.edits);
        }
    }
    assert_eq!(a.text.to_string(), b.text.to_string());
    assert!(a.text.to_string().contains("from a") && a.text.to_string().contains("from b"));
}

/// A message that arrives before what it builds on changes nothing, and
/// asks again; the answer brings everything.
#[test]
fn an_early_message_asks_again() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "one").unwrap();
    a.apply(e);
    let first = a.text.receive(&b.text.hello()).unwrap().reply;
    let e = a.text.insert(3, " two").unwrap();
    a.apply(e);
    // What b would have been missing had it got the first: just " two".
    let mut c = Replica::new("c");
    let got = c.text.receive(&first[0]).unwrap();
    c.apply(got.edits);
    let second = a.text.receive(&c.text.hello()).unwrap().reply;
    // b gets the second before the first.
    let got = b.text.receive(&second[0]).unwrap();
    assert!(got.edits.is_empty());
    assert_eq!(got.reply.len(), 1, "it asks again");
    let answer = a.text.receive(&got.reply[0]).unwrap();
    let got = b.text.receive(&answer.reply[0]).unwrap();
    b.apply(got.edits);
    assert_eq!(b.text.to_string(), "one two");
}

/// The editor's outline, kept only from the edits, agrees with the text's.
#[test]
fn section_edits_keep_an_outline() {
    use std::collections::BTreeMap;
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    let e = a.text.insert(0, "A\nB\nC\n").unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    let mut outline: BTreeMap<mamre_text::SectionId, u8> = BTreeMap::new();
    for (at, level) in [(0, 1), (2, 2), (4, 1), (2, 0)] {
        let (_, edits) = a.text.mark_section(at, level).unwrap();
        a.apply(edits);
    }
    let got = b
        .text
        .receive(&a.text.receive(&b.text.hello()).unwrap().reply[0])
        .unwrap();
    for e in &got.edits {
        if let Edit::Section { section, level, .. } = e {
            if *level == 0 {
                outline.remove(section);
            } else {
                outline.insert(*section, *level);
            }
        }
    }
    b.apply(got.edits);
    let want: BTreeMap<_, _> = b.text.sections().iter().map(|s| (s.id, s.level)).collect();
    assert_eq!(outline, want);
    assert_eq!(want.len(), 2);
}

/// A move, and an insert made inside what it moves at the same time,
/// panic the merge: "origins in a loop", in egg-walker's `copies.rs`.
/// Found by the isolates' two-writer test; nothing here is an isolate.
#[test]
#[ignore = "panics in egg-walker's moves: origins in a loop"]
fn a_move_and_an_insert_at_once() {
    fn talk(a: &mut Text, b: &mut Text) {
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
    let mut a = Text::new("me");
    a.insert(0, ", בגaaaaaaבגaaaa23aa, bca, aaaבג, aaa")
        .unwrap();
    let mut b = Text::new("you");
    talk(&mut a, &mut b);
    b.delete(20..28).unwrap();
    b.insert(0, " ").unwrap();
    b.insert(15, "]").unwrap();
    b.insert(0, "[").unwrap();
    b.insert(0, "12").unwrap();
    talk(&mut a, &mut b);
    assert_eq!(a.to_string(), "12[ , בגaaaaaaב]גaaaaa, aaaבג, aaa");
    a.move_text(0..19, 21).unwrap();
    b.insert(18, " ").unwrap();
    talk(&mut a, &mut b);
    assert_eq!(a.to_string(), b.to_string());
}

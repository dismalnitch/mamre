//! Undo and redo, as an editor uses them: a step at a time, each call that
//! edits one step unless the editor groups several, and only this writer's
//! own effects reversed.
//!
//! As in `text.rs`, each replica keeps an "editor": a `String` kept only
//! from the edits handed back, which must always read as the text does.

use mamre_text::{Direction, Edit, Text};
use proptest::prelude::*;

struct Replica {
    text: Text,
    editor: String,
    /// The moves hinted: where from, and the text that arrived.
    moved: Vec<(usize, String)>,
}

impl Replica {
    fn new(name: &str) -> Self {
        Replica {
            text: Text::new(name),
            editor: String::new(),
            moved: Vec::new(),
        }
    }

    fn apply(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            match edit {
                Edit::Insert { at, text } => self.editor.insert_str(at, &text),
                Edit::Delete { range } => self.editor.replace_range(range, ""),
                Edit::Moved { from, to } => {
                    assert!(from <= self.editor.len() && to.end <= self.editor.len());
                    self.moved.push((from, self.editor[to].to_string()));
                }
                Edit::Section { .. } | Edit::Isolates | Edit::Lost { .. } => {}
            }
        }
        assert_eq!(
            self.editor,
            self.text.to_string(),
            "the edits are the text's"
        );
    }

    fn type_(&mut self, at: usize, text: &str) {
        let e = self.text.insert(at, text).unwrap();
        self.apply(e);
    }

    /// Undoes, keeping the editor up to date; whether there was a step.
    fn undo(&mut self) -> bool {
        self.text.undo().map(|e| self.apply(e)).is_some()
    }

    fn redo(&mut self) -> bool {
        self.text.redo().map(|e| self.apply(e)).is_some()
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
fn each_edit_is_a_step() {
    let mut a = Replica::new("a");
    a.type_(0, "ab");
    a.type_(2, "c");
    assert!(a.undo());
    assert_eq!(a.editor, "ab");
    assert!(a.undo());
    assert_eq!(a.editor, "");
    assert!(!a.undo(), "nothing left to undo");
    assert!(a.redo());
    assert_eq!(a.editor, "ab");
    assert!(a.redo());
    assert_eq!(a.editor, "abc");
    assert!(!a.redo(), "nothing left to redo");
}

#[test]
fn a_group_is_one_step() {
    let mut a = Replica::new("a");
    a.type_(0, "one ");
    // Typed a key at a time, until a pause: one step.
    a.text.begin_group();
    assert!(a.text.in_group());
    for (i, c) in "two".chars().enumerate() {
        a.type_(4 + i, &c.to_string());
    }
    let e = a.text.delete(0..4).unwrap();
    a.apply(e);
    a.text.end_group();
    assert!(!a.text.in_group());
    assert_eq!(a.editor, "two");
    assert!(a.undo());
    assert_eq!(a.editor, "one ");
}

#[test]
fn groups_nest() {
    let mut a = Replica::new("a");
    a.text.begin_group();
    a.type_(0, "a");
    a.text.begin_group();
    a.type_(1, "b");
    a.text.end_group();
    a.type_(2, "c");
    a.text.end_group();
    assert!(a.undo());
    assert_eq!(a.editor, "", "only the outermost group counts");
}

#[test]
fn a_replace_is_one_step() {
    let mut a = Replica::new("a");
    a.type_(0, "hello world");
    let e = a.text.replace(0..5, "goodbye").unwrap();
    a.apply(e);
    assert!(a.undo());
    assert_eq!(a.editor, "hello world");
    assert!(a.redo());
    assert_eq!(a.editor, "goodbye world");
}

#[test]
fn a_section_move_is_one_step() {
    let mut a = Replica::new("a");
    a.type_(0, "Intro\nbody\nLater\nmore\n");
    let (_, e) = a.text.mark_section(0, 1).unwrap();
    a.apply(e);
    let (later, e) = a.text.mark_section(11, 1).unwrap();
    a.apply(e);
    let e = a.text.move_section(later, 0).unwrap();
    a.apply(e);
    assert_eq!(a.editor, "Later\nmore\nIntro\nbody\n");
    assert!(a.undo());
    assert_eq!(a.editor, "Intro\nbody\nLater\nmore\n");
    // The section went back with its id.
    assert_eq!(a.text.section_range(later), Some(11..22));
    assert!(a.redo());
    assert_eq!(a.editor, "Later\nmore\nIntro\nbody\n");
    assert_eq!(a.text.section_range(later), Some(0..11));
}

#[test]
fn a_section_mark_is_undone() {
    let mut a = Replica::new("a");
    a.type_(0, "Title\ntext");
    let (title, e) = a.text.mark_section(0, 1).unwrap();
    a.apply(e);
    assert_eq!(a.text.sections().len(), 1);
    assert!(a.undo());
    assert!(a.text.sections().is_empty());
    assert_eq!(a.text.section_range(title), None);
    assert_eq!(a.editor, "Title\ntext", "marks don't change the text");
}

#[test]
fn an_isolate_is_undone() {
    let mut a = Replica::new("a");
    a.type_(0, "say שלום now");
    let start = "say ".len();
    let end = start + "שלום".len();
    let (_, e) = a
        .text
        .mark_isolate(start..end, Direction::RightToLeft)
        .unwrap();
    a.apply(e);
    assert_eq!(a.text.isolates().len(), 1);
    assert!(a.undo());
    assert!(a.text.isolates().is_empty());
    assert!(a.redo());
    assert_eq!(a.text.isolates().len(), 1);
    assert_eq!(a.text.isolates()[0].range, start..end);
}

#[test]
fn a_new_edit_ends_redo() {
    let mut a = Replica::new("a");
    a.type_(0, "ab");
    assert!(a.undo());
    a.type_(0, "x");
    assert!(!a.redo(), "a new step since: nothing to redo");
    assert_eq!(a.editor, "x");
}

/// Undo reverses only this writer's step: what another did at the same
/// time stays.
#[test]
fn others_edits_stay() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "hello world");
    sync(&mut a, &mut b);
    // a deletes "world" while b adds to the end.
    let e = a.text.delete(5..11).unwrap();
    a.apply(e);
    b.type_(11, "!");
    sync(&mut a, &mut b);
    assert_eq!(a.editor, "hello!");
    assert!(a.undo());
    assert_eq!(a.editor, "hello world!");
    sync(&mut a, &mut b);
    assert_eq!(b.editor, "hello world!", "the undo is sent like any edit");
    // a's typing undone: b's "!" stays.
    assert!(a.undo());
    assert_eq!(a.editor, "!");
    sync(&mut a, &mut b);
    assert_eq!(b.editor, "!");
}

/// Each writer undoes only its own steps.
#[test]
fn undo_is_this_writers() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "a's ");
    sync(&mut a, &mut b);
    b.type_(4, "b's");
    sync(&mut a, &mut b);
    assert!(a.undo());
    assert_eq!(a.editor, "b's");
    assert!(!a.undo());
    // b hasn't heard of a's undo yet.
    assert!(b.undo());
    assert_eq!(b.editor, "a's ");
    sync(&mut a, &mut b);
    assert_eq!(a.editor, "");
    assert_eq!(b.editor, "");
}

#[test]
fn a_moved_text_goes_back_while_another_types_in_it() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    sync(&mut a, &mut b);
    // a moves "two " to the end; b types inside it, where it was. A move
    // is a delete and an insert (#68): b's typing stays at the old spot.
    let e = a.text.move_text(4..8, 13).unwrap();
    a.apply(e);
    b.type_(6, "o");
    sync(&mut a, &mut b);
    assert_eq!(a.editor, "one othreetwo ");
    assert!(a.undo());
    assert_eq!(a.editor, "one twoo three", "it goes back round b's typing");
    sync(&mut a, &mut b);
    assert_eq!(b.editor, a.editor);
}

/// The stacks are the replica's: a history opened again has nothing to
/// undo, but its steps are all there. (Which were undos isn't saved, so
/// undoing one could undo an undo.)
#[test]
fn an_opened_text_starts_with_nothing_to_undo() {
    let mut a = Replica::new("a");
    a.type_(0, "kept");
    let mut file = Vec::new();
    a.text.save(&mut file);
    let mut again = Replica::new("a");
    again.text = Text::open("a", &file).unwrap();
    again.editor = again.text.to_string();
    assert!(!again.undo());
    again.type_(4, "!");
    assert!(again.undo());
    assert_eq!(again.editor, "kept");
}

#[test]
fn an_opened_undo_isnt_undone() {
    let mut a = Replica::new("a");
    a.type_(0, "gone");
    assert!(a.undo());
    let mut file = Vec::new();
    a.text.save(&mut file);
    let mut again = Replica::new("a");
    again.text = Text::open("a", &file).unwrap();
    again.editor = again.text.to_string();
    assert!(!again.undo());
    assert!(!again.redo());
    assert_eq!(again.editor, "");
}

#[test]
fn an_undo_is_saved() {
    let mut a = Replica::new("a");
    a.type_(0, "draft");
    assert!(a.undo());
    let mut file = Vec::new();
    a.text.save(&mut file);
    assert_eq!(Text::open("a", &file).unwrap().to_string(), "");
}

#[test]
#[should_panic(expected = "an undo inside a group")]
fn an_undo_inside_a_group_panics() {
    let mut a = Replica::new("a");
    a.text.begin_group();
    a.type_(0, "x");
    a.text.undo();
}

#[test]
fn a_failed_edit_makes_no_step() {
    let mut a = Replica::new("a");
    a.type_(0, "ab");
    assert!(a.text.insert(99, "x").is_err());
    assert!(a.undo());
    assert_eq!(a.editor, "");
}

// ---- Many writers undoing, a bad network ----

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
    /// A replica's next few acts, if edits, are one step.
    Group(usize, u8),
    Undo(usize),
    Redo(usize),
    Hello(usize, usize),
    Deliver(prop::sample::Index, bool),
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
        1 => (0..3usize, 1u8..4).prop_map(|(r, n)| Act::Group(r, n)),
        3 => (0..3usize).prop_map(Act::Undo),
        1 => (0..3usize).prop_map(Act::Redo),
        3 => (0..3usize, 0..3usize).prop_map(|(a, b)| Act::Hello(a, b)),
        6 => (any::<prop::sample::Index>(), any::<bool>()).prop_map(|(i, k)| Act::Deliver(i, k)),
        1 => any::<prop::sample::Index>().prop_map(Act::Drop),
    ];
    prop::collection::vec(act, 1..80)
}

fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices().map(|(i, _)| i).chain([s.len()]).collect()
}

proptest! {
    // 128 cases, or PROPTEST_CASES for a run at volume.
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(128)
    ))]

    /// Three replicas editing, grouping and undoing at once over a network
    /// that drops, duplicates and reorders: each editor's copy, kept only
    /// from the edits handed back, reads as its text; once everything's
    /// delivered, all three read the same.
    #[test]
    fn replicas_converge_through_undos(acts in acts()) {
        let mut rs: Vec<Replica> = ["a", "b", "c"].iter().map(|n| Replica::new(n)).collect();
        // How many more edits each replica's open group takes.
        let mut grouped = [0u8; 3];
        let mut wire: Vec<(usize, Vec<u8>)> = Vec::new();
        for act in acts {
            let edited = match act {
                Act::Insert(r, p, ref t) => {
                    let at = *p.get(&boundaries(&rs[r].editor));
                    rs[r].type_(at, t);
                    Some(r)
                }
                Act::Delete(r, a, b) => {
                    let bs = boundaries(&rs[r].editor);
                    let (a, b) = (*a.get(&bs), *b.get(&bs));
                    let e = rs[r].text.delete(a.min(b)..a.max(b)).unwrap();
                    rs[r].apply(e);
                    Some(r)
                }
                Act::Move(r, a, b, t) => {
                    let bs = boundaries(&rs[r].editor);
                    let (a, b, t) = (*a.get(&bs), *b.get(&bs), *t.get(&bs));
                    let e = rs[r].text.move_text(a.min(b)..a.max(b), t).unwrap();
                    rs[r].apply(e);
                    Some(r)
                }
                Act::Mark(r, p, level) => {
                    let bs = boundaries(&rs[r].editor);
                    if bs.len() > 1 {
                        let at = *p.get(&bs[..bs.len() - 1]);
                        let (_, e) = rs[r].text.mark_section(at, level).unwrap();
                        rs[r].apply(e);
                    }
                    Some(r)
                }
                Act::Group(r, n) => {
                    if grouped[r] == 0 {
                        rs[r].text.begin_group();
                        grouped[r] = n;
                    }
                    None
                }
                Act::Undo(r) => {
                    if grouped[r] == 0 {
                        rs[r].undo();
                    }
                    None
                }
                Act::Redo(r) => {
                    if grouped[r] == 0 {
                        rs[r].redo();
                    }
                    None
                }
                Act::Hello(a, b) if a != b => {
                    wire.push((b, rs[a].text.hello()));
                    None
                }
                Act::Deliver(i, keep) if !wire.is_empty() => {
                    let k = i.index(wire.len());
                    let (to, m) = if keep { wire[k].clone() } else { wire.remove(k) };
                    let got = rs[to].text.receive(&m).unwrap();
                    rs[to].apply(got.edits);
                    for reply in got.reply {
                        for other in (0..3).filter(|&o| o != to) {
                            wire.push((other, reply.clone()));
                        }
                    }
                    None
                }
                Act::Drop(i) if !wire.is_empty() => {
                    wire.remove(i.index(wire.len()));
                    None
                }
                _ => None,
            };
            if let Some(r) = edited
                && grouped[r] > 0
            {
                grouped[r] -= 1;
                if grouped[r] == 0 {
                    rs[r].text.end_group();
                }
            }
        }
        for (r, g) in rs.iter_mut().zip(grouped) {
            if g > 0 {
                r.text.end_group();
            }
        }
        for a in 0..3 {
            for b in 0..3 {
                if a < b {
                    let (l, r) = rs.split_at_mut(b);
                    sync(&mut l[a], &mut r[0]);
                }
            }
        }
        for a in 0..3 {
            for b in 0..3 {
                if a < b {
                    let (l, r) = rs.split_at_mut(b);
                    sync(&mut l[a], &mut r[0]);
                }
            }
        }
        let texts: Vec<String> = rs.iter().map(|r| r.text.to_string()).collect();
        prop_assert_eq!(&texts[0], &texts[1]);
        prop_assert_eq!(&texts[1], &texts[2]);
    }
}

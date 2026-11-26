//! Undo groups (#14): events that are one step, undone and redone as one,
//! selectively, with a timeline that steps a group at a time.

use std::collections::BTreeSet;

use mamre_egg_walker::copies;
use mamre_egg_walker::{Accountant, AgentId, Change, Lv, Op, OpLog};
use proptest::prelude::*;

/// A writer: its books and document, at its version of the shared log.
struct Writer {
    me: AgentId,
    books: Accountant,
    doc: Vec<char>,
}

impl Writer {
    fn new(log: &mut OpLog<char>, name: &str) -> Self {
        Writer {
            me: log.agent(name),
            books: Accountant::new(),
            doc: Vec::new(),
        }
    }

    fn text(&self) -> String {
        self.doc.iter().collect()
    }

    fn apply(&mut self, changes: Vec<Change<'_, char>>) {
        for c in changes {
            c.apply(&mut self.doc);
        }
    }

    fn merge(&mut self, log: &OpLog<char>, to: &[Lv]) {
        let changes = self.books.merge(log, to);
        self.apply(changes);
    }

    /// Types `text` at `pos`, a character an event, as the editor would.
    fn type_at(&mut self, log: &mut OpLog<char>, pos: usize, text: &str) {
        for (k, c) in text.chars().enumerate() {
            let v = log.add_insert(self.me, self.books.version(), pos + k, [c]);
            self.merge(log, &v);
        }
    }

    fn delete(&mut self, log: &mut OpLog<char>, range: std::ops::Range<usize>) {
        let v = log.add_delete(self.me, self.books.version(), range);
        self.merge(log, &v);
    }

    fn move_range(&mut self, log: &mut OpLog<char>, range: std::ops::Range<usize>, to: usize) {
        let v = log.add_move(self.me, self.books.version(), range, to);
        self.merge(log, &v);
    }

    fn undo(&mut self, log: &mut OpLog<char>) -> bool {
        match self.books.undo_group(log, self.me) {
            Some(changes) => {
                let changes: Vec<Change<'_, char>> = changes;
                for c in changes {
                    c.apply(&mut self.doc);
                }
                true
            }
            None => false,
        }
    }

    fn redo(&mut self, log: &mut OpLog<char>) -> bool {
        match self.books.redo_group(log, self.me) {
            Some(changes) => {
                for c in changes {
                    c.apply(&mut self.doc);
                }
                true
            }
            None => false,
        }
    }
}

#[test]
fn a_typed_word_is_one_step() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "Hello");
    log.end_group(w.me);
    log.begin_group(w.me);
    w.type_at(&mut log, 5, " world");
    log.end_group(w.me);
    assert_eq!(w.text(), "Hello world");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "Hello");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "");
    assert!(!w.undo(&mut log), "nothing left to undo");
    assert!(w.redo(&mut log));
    assert_eq!(w.text(), "Hello");
    assert!(w.redo(&mut log));
    assert_eq!(w.text(), "Hello world");
    assert!(!w.redo(&mut log));
    // And the reference agrees with the replica throughout.
    assert_eq!(copies::checkout(&log, w.books.version()), w.doc);
}

#[test]
fn a_paste_over_a_selection_is_one_step() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "the big dog");
    log.end_group(w.me);
    // Select "big", paste "small": a delete and an insert, one step.
    log.begin_group(w.me);
    w.delete(&mut log, 4..7);
    let v = log.add_insert(w.me, w.books.version(), 4, "small".chars());
    w.merge(&log, &v);
    log.end_group(w.me);
    assert_eq!(w.text(), "the small dog");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "the big dog");
    // The same "big": its own items, back.
    let ids = copies::identities(&log, w.books.version());
    assert_eq!(&ids[4..7], &[4, 5, 6]);
    assert!(w.redo(&mut log));
    assert_eq!(w.text(), "the small dog");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "the big dog");
}

#[test]
fn a_two_event_section_move_is_one_step() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "AaaBbbCcc");
    log.end_group(w.me);
    // Turning a section: two moves, seen as one step.
    log.begin_group(w.me);
    w.move_range(&mut log, 3..6, 9);
    w.move_range(&mut log, 0..3, 9);
    log.end_group(w.me);
    assert_eq!(w.text(), "CccBbbAaa");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "AaaBbbCcc");
    assert!(w.redo(&mut log));
    assert_eq!(w.text(), "CccBbbAaa");
    assert_eq!(copies::checkout(&log, w.books.version()), w.doc);
}

#[test]
fn a_step_that_undoes_itself_inside_reverses_to_nothing() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "abcdef");
    log.end_group(w.me);
    // A move and its undo, a delete and its undelete: one step.
    log.begin_group(w.me);
    w.move_range(&mut log, 0..2, 6);
    let m = log.version()[0];
    let v = log.add_undo(w.me, w.books.version(), [m]);
    w.merge(&log, &v);
    w.delete(&mut log, 1..2);
    let d = log.version()[0];
    let v = log.add_undo(w.me, w.books.version(), [d]);
    w.merge(&log, &v);
    w.type_at(&mut log, 6, "!");
    log.end_group(w.me);
    assert_eq!(w.text(), "abcdef!");
    assert!(w.undo(&mut log));
    assert_eq!(w.text(), "abcdef");
}

#[test]
fn others_typing_survives_an_undo() {
    let mut log = OpLog::new();
    let mut a = Writer::new(&mut log, "a");
    let mut b = Writer::new(&mut log, "b");
    log.begin_group(a.me);
    a.type_at(&mut log, 0, "one two");
    log.end_group(a.me);
    b.merge(&log, a.books.version());
    // B types inside A's words; A, not seeing it yet, undoes them.
    b.type_at(&mut log, 3, "!");
    assert!(a.undo(&mut log));
    assert_eq!(a.text(), "");
    let all = log.union(a.books.version(), b.books.version());
    a.merge(&log, &all);
    b.merge(&log, &all);
    assert_eq!(a.text(), "!");
    assert_eq!(b.text(), "!");
    // A's redo brings its words back around B's.
    assert!(a.redo(&mut log));
    assert_eq!(a.text(), "one! two");
}

#[test]
fn a_mark_goes_back_to_its_old_level() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "Title");
    let v = log.add_mark(w.me, w.books.version(), 0, 1);
    w.merge(&log, &v);
    log.end_group(w.me);
    log.begin_group(w.me);
    let v = log.add_mark(w.me, w.books.version(), 0, 2);
    w.merge(&log, &v);
    log.end_group(w.me);
    assert_eq!(w.books.level(0), 2);
    assert!(w.undo(&mut log));
    assert_eq!(w.books.level(0), 1);
    assert!(w.redo(&mut log));
    assert_eq!(w.books.level(0), 2);
}

#[test]
fn a_new_edit_clears_the_redo() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "ab");
    log.end_group(w.me);
    assert!(w.undo(&mut log));
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "c");
    log.end_group(w.me);
    assert!(!w.redo(&mut log));
    assert_eq!(w.text(), "c");
}

#[test]
fn the_timeline_steps_a_group_at_a_time() {
    let mut log = OpLog::new();
    let mut a = Writer::new(&mut log, "a");
    let mut b = Writer::new(&mut log, "b");
    log.begin_group(a.me);
    a.type_at(&mut log, 0, "abc");
    log.end_group(a.me);
    log.begin_group(b.me);
    b.type_at(&mut log, 0, "xy");
    // B's step is open while A's next one ends.
    log.begin_group(a.me);
    a.type_at(&mut log, 3, "de");
    log.end_group(a.me);
    log.end_group(b.me);
    // A typing step left open shows nowhere.
    a.type_at(&mut log, 5, "f");
    let timeline = log.timeline();
    let texts: Vec<String> = timeline
        .iter()
        .map(|v| copies::checkout(&log, v).into_iter().collect())
        .collect();
    // Steps in the order of their last events: B's "xy" (typed before
    // A's "de") comes first, though B ended it later. B typed it without
    // seeing "abc", so ties put it after.
    assert_eq!(texts, ["abc", "abcxy", "abcdexy"]);
}

#[test]
fn step_ends_round_trip_in_chunks() {
    let mut log = OpLog::new();
    let mut w = Writer::new(&mut log, "w");
    log.begin_group(w.me);
    w.type_at(&mut log, 0, "ab");
    log.end_group(w.me);
    log.begin_group(w.me);
    w.type_at(&mut log, 2, "cd");
    // Saved with the step open...
    let mut first = Vec::new();
    log.encode_chunk(0..log.len(), &mut first);
    log.end_group(w.me);
    w.type_at(&mut log, 4, "e");
    // ...and closed by the next chunk.
    let mut second = Vec::new();
    log.encode_chunk(4..log.len(), &mut second);
    // Readable as they're written: each agent's step ends, by name.
    assert!(first.ends_with(br#"{"steps":{"w":[1]}}"#));
    assert!(second.ends_with(br#"{"steps":{"w":[3]}}"#));
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&first), Ok(4));
    let b = back.agent("w");
    assert_eq!(back.steps(b), vec![0..2]);
    assert_eq!(back.decode_chunk(&second), Ok(1));
    assert_eq!(back.steps(b), vec![0..2, 2..4]);
    assert_eq!(back.steps(b), log.steps(w.me));
    // A chunk with no step ends is as before: nothing after the runs.
    let mut plain = OpLog::<char>::new();
    let p = plain.agent("p");
    plain.add_insert(p, &[], 0, "x".chars());
    let mut bytes = Vec::new();
    plain.encode_chunk(0..1, &mut bytes);
    assert_eq!(bytes, [0, 1, 1, b'p', 1, 0, 0, 0, 0, 0, 1, 1, b'x']);
    // Step ends go beside the runs as JSON: a format byte (1), then the
    // JSON's length and text. One past the agent's events is refused.
    let with = |json: &str| {
        let mut out = bytes.clone();
        out.push(1);
        out.push(json.len() as u8);
        out.extend(json.bytes());
        out
    };
    assert!(OpLog::<char>::new().decode_chunk(&with(r#"{"steps":{"p":[5]}}"#)).is_err());
    assert!(OpLog::<char>::new().decode_chunk(&with(r#"{"steps":{"q":[0]}}"#)).is_err());
    let good = with(r#"{"steps":{"p":[0]}}"#);
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&good), Ok(1));
    let p = back.agent("p");
    assert_eq!(back.steps(p), vec![0..1]);
}

// ---- Many writers ----

#[derive(Clone, Debug)]
enum Edit {
    Type(prop::sample::Index, char, usize),
    Delete(prop::sample::Index, usize),
    Move(
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Mark(prop::sample::Index, u8),
}

#[derive(Clone, Debug)]
enum Action {
    Group(usize, Vec<Edit>),
    Undo(usize),
    Redo(usize),
    Sync(usize, usize),
}

fn edit() -> impl Strategy<Value = Edit> {
    let i = || any::<prop::sample::Index>();
    prop_oneof![
        4 => (i(), prop::char::range('a', 'z'), 1usize..4).prop_map(|(p, c, n)| Edit::Type(p, c, n)),
        2 => (i(), 1usize..3).prop_map(|(p, n)| Edit::Delete(p, n)),
        1 => (i(), i(), i()).prop_map(|(a, b, t)| Edit::Move(a, b, t)),
        1 => (i(), 0u8..3).prop_map(|(p, l)| Edit::Mark(p, l)),
    ]
}

fn actions() -> impl Strategy<Value = Vec<Action>> {
    let action = prop_oneof![
        4 => (0..3usize, prop::collection::vec(edit(), 1..4)).prop_map(|(w, e)| Action::Group(w, e)),
        2 => (0..3usize).prop_map(Action::Undo),
        1 => (0..3usize).prop_map(Action::Redo),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Action::Sync(a, b)),
    ];
    prop::collection::vec(action, 1..30)
}

fn describe(log: &OpLog<char>) -> String {
    (0..log.len())
        .map(|lv| {
            let e = log.event(lv);
            format!(
                "{lv}: {:?} {:?} after {:?}\n",
                log.agent_name(e.agent),
                e.op,
                e.parents
            )
        })
        .collect()
}

/// The items live at `v`.
fn live(log: &OpLog<char>, v: &[Lv]) -> BTreeSet<Lv> {
    copies::identities(log, v).into_iter().collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Three writers grouping their edits, undoing, redoing and syncing:
    /// every replica is what the reference says at its version; an undo
    /// changes only what its step touched (everyone else's edits survive);
    /// and once all have everything, they agree.
    #[test]
    fn undo_is_selective_and_replicas_converge(actions in actions()) {
        let mut log = OpLog::new();
        let mut ws: Vec<Writer> = ["a", "b", "c"].iter().map(|n| Writer::new(&mut log, n)).collect();
        for action in actions {
            match action {
                Action::Group(w, edits) => {
                    let me = ws[w].me;
                    log.begin_group(me);
                    for e in edits {
                        let len = ws[w].doc.len();
                        match e {
                            Edit::Type(p, c, n) => {
                                let s: String = std::iter::repeat_n(c, n).collect();
                                ws[w].type_at(&mut log, p.index(len + 1), &s);
                            }
                            Edit::Delete(p, n) if len > 0 => {
                                let at = p.index(len);
                                ws[w].delete(&mut log, at..(at + n).min(len));
                            }
                            Edit::Move(a, b, t) if len > 0 => {
                                let (a, b) = (a.index(len), b.index(len));
                                let (a, b) = (a.min(b), a.max(b) + 1);
                                let to = t.index(len + 1);
                                if to <= a || to >= b {
                                    ws[w].move_range(&mut log, a..b, to);
                                }
                            }
                            Edit::Mark(p, l) if len > 0 => {
                                let v = log.add_mark(me, ws[w].books.version(), p.index(len), l);
                                ws[w].merge(&log, &v);
                            }
                            _ => {}
                        }
                    }
                    log.end_group(me);
                }
                Action::Undo(w) | Action::Redo(w) => {
                    let me = ws[w].me;
                    let before = ws[w].books.version().to_vec();
                    let alive = live(&log, &before);
                    let undo = matches!(action, Action::Undo(_));
                    let done = if undo { ws[w].undo(&mut log) } else { ws[w].redo(&mut log) };
                    if !done {
                        continue;
                    }
                    // What the step it reversed touched: its inserts, and
                    // what its deletes and undeletes took or brought back.
                    let after = ws[w].books.version().to_vec();
                    let made: Vec<Lv> = (0..log.len())
                        .filter(|&lv| log.contains(&after, lv) && !log.contains(&before, lv))
                        .collect();
                    let mut touched: BTreeSet<Lv> = BTreeSet::new();
                    for &lv in &made {
                        let e = log.event(lv);
                        prop_assert_eq!(e.agent, me);
                        match e.op {
                            Op::Delete { pos } => {
                                touched.insert(copies::identities(&log, &e.parents)[pos]);
                            }
                            Op::Undelete { target } => {
                                let d = log.event(target);
                                let Op::Delete { pos } = d.op else { unreachable!() };
                                touched.insert(copies::identities(&log, &d.parents)[pos]);
                            }
                            _ => {}
                        }
                    }
                    let now = live(&log, &after);
                    for item in alive.symmetric_difference(&now) {
                        prop_assert!(touched.contains(item), "{} changed, untouched by the undo\n{}", item, describe(&log));
                    }
                }
                Action::Sync(a, b) => {
                    let to = ws[b].books.version().to_vec();
                    ws[a].merge(&log, &to);
                }
            }
            for w in &ws {
                let v = w.books.version();
                prop_assert_eq!(&w.doc, &copies::checkout(&log, v), "{} at {:?}\n{}", log.agent_name(w.me), v, describe(&log));
            }
        }
        let all = log.version().to_vec();
        for w in &mut ws {
            w.merge(&log, &all);
        }
        prop_assert_eq!(&ws[0].doc, &ws[1].doc);
        prop_assert_eq!(&ws[1].doc, &ws[2].doc);
        // Every timeline point holds whole steps.
        for v in log.timeline() {
            for w in &ws {
                let mine: Vec<u64> = (0..log.len())
                    .filter(|&lv| log.contains(&v, lv) && log.event(lv).agent == w.me)
                    .map(|lv| log.event(lv).seq)
                    .collect();
                if let Some(&last) = mine.last() {
                    prop_assert!(log.steps(w.me).iter().any(|s| s.end == last + 1), "half a step at {:?}", v);
                }
            }
        }
    }
}

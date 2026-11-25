//! The reference for moves as copies (#20), checked three ways:
//!
//! - one writer: against a plain list, where a move is a splice;
//! - many writers: against the portal reference, on every history where
//!   that one doesn't refuse a move the copies take (a range cutting into
//!   a moved block);
//! - always: every live item shows exactly once.
//!
//! And the rules for what wins, and what's reported lost, one test each.

use std::collections::{BTreeSet, HashMap, HashSet};

use mamre_egg_walker::copies::{self, Loss};
use mamre_egg_walker::runs;
use mamre_egg_walker::{Lv, Op, OpLog, reference};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Edit {
    Insert(prop::sample::Index, char),
    Delete(prop::sample::Index),
    Move(
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
    Undo(prop::sample::Index),
    Mark(prop::sample::Index, u8),
}

#[derive(Clone, Debug)]
enum Action {
    Edit(usize, Edit),
    Merge(usize, usize),
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        4 => (any::<prop::sample::Index>(), prop::char::range('a', 'z')).prop_map(|(p, c)| Edit::Insert(p, c)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Delete),
        3 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b, t)| Edit::Move(a, b, t)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Undo),
        1 => (any::<prop::sample::Index>(), 0u8..4).prop_map(|(p, l)| Edit::Mark(p, l)),
    ]
}

fn actions(writers: usize, n: usize) -> impl Strategy<Value = Vec<Action>> {
    let action = prop_oneof![
        4 => (0..writers, edit()).prop_map(|(w, e)| Action::Edit(w, e)),
        1 => (0..writers, 0..writers).prop_map(|(a, b)| Action::Merge(a, b)),
    ];
    prop::collection::vec(action, 1..n)
}

/// The history the actions make, each writer's version after it, and
/// every version an event was made at or a writer reached.
fn history(actions: &[Action], writers: usize) -> (OpLog<char>, Vec<Vec<Lv>>) {
    let mut log: OpLog<char> = OpLog::new();
    let names = ["a", "b", "c"];
    let agents: Vec<_> = names[..writers].iter().map(|n| log.agent(n)).collect();
    let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); writers];
    let mut seen = Vec::new();
    for action in actions.iter().cloned() {
        match action {
            Action::Merge(a, b) => versions[a] = log.union(&versions[a], &versions[b]),
            Action::Edit(w, edit) => {
                let v = versions[w].clone();
                let len = copies::checkout(&log, &v).len();
                let agent = agents[w];
                versions[w] = match edit {
                    Edit::Insert(p, c) => log.add_insert(agent, &v, p.index(len + 1), [c]),
                    Edit::Delete(p) if len > 0 => {
                        let pos = p.index(len);
                        log.add_delete(agent, &v, pos..pos + 1)
                    }
                    Edit::Move(a, b, t) if len > 0 => {
                        let (a, b) = (a.index(len), b.index(len));
                        let (a, b) = (a.min(b), a.max(b) + 1);
                        let to = t.index(len + 1);
                        if to > a && to < b {
                            continue;
                        }
                        log.add_move(agent, &v, a..b, to)
                    }
                    Edit::Undo(k) => {
                        let deletes: Vec<Lv> = (0..log.len())
                            .filter(|&lv| {
                                log.contains(&v, lv)
                                    && matches!(log.event(lv).op, Op::Delete { .. })
                            })
                            .collect();
                        if deletes.is_empty() {
                            continue;
                        }
                        log.add_undelete(agent, &v, [deletes[k.index(deletes.len())]])
                    }
                    Edit::Mark(p, level) if len > 0 => log.add_mark(agent, &v, p.index(len), level),
                    _ => continue,
                };
            }
        }
        seen.extend(versions.iter().cloned());
    }
    (log, seen)
}

fn describe(log: &OpLog<char>) -> String {
    (0..log.len())
        .map(|lv| {
            let e = log.event(lv);
            format!("{lv}: {:?} after {:?}\n", e.op, e.parents)
        })
        .collect()
}

// ---- One writer: a move is a splice ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// One writer, any edits, any moves, ranges cutting moved blocks and
    /// all: the copies reference draws what a plain list does.
    #[test]
    fn one_writer_is_a_splice(edits in prop::collection::vec(edit(), 1..40)) {
        let mut log: OpLog<char> = OpLog::new();
        let me = log.agent("me");
        let mut v: Vec<Lv> = Vec::new();
        let mut doc: Vec<char> = Vec::new();
        // Each delete, and what it took out, and where it came back.
        let mut deletes: Vec<(Lv, char, usize)> = Vec::new();
        for edit in edits {
            let len = doc.len();
            match edit {
                Edit::Insert(p, c) => {
                    let at = p.index(len + 1);
                    v = log.add_insert(me, &v, at, [c]);
                    doc.insert(at, c);
                }
                Edit::Delete(p) if len > 0 => {
                    let at = p.index(len);
                    v = log.add_delete(me, &v, at..at + 1);
                    deletes.push((v[0], doc.remove(at), at));
                }
                Edit::Move(a, b, t) if len > 0 => {
                    let (a, b) = (a.index(len), b.index(len));
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    let to = t.index(len + 1);
                    if to > a && to < b {
                        continue;
                    }
                    v = log.add_move(me, &v, a..b, to);
                    if to < a || to > b {
                        let taken: Vec<char> = doc.drain(a..b).collect();
                        let to = if to > b { to - (b - a) } else { to };
                        doc.splice(to..to, taken);
                    }
                }
                _ => continue,
            }
            let got: String = copies::checkout(&log, &v).into_iter().collect();
            let want: String = doc.iter().collect();
            prop_assert_eq!(got, want, "\n{}", describe(&log));
        }
        let _ = deletes;
    }
}

// ---- Many writers: as the portals, where they can ----

/// The live items at `version`: inserted, and not deleted (or deleted and
/// undone), as copies' own identities name what each delete took.
fn live(log: &OpLog<char>, version: &[Lv]) -> BTreeSet<Lv> {
    let at: HashSet<Lv> = (0..log.len())
        .filter(|&lv| log.contains(version, lv))
        .collect();
    let mut took: HashMap<Lv, Lv> = HashMap::new();
    let mut undone = HashSet::new();
    let mut inserted = BTreeSet::new();
    for &lv in &at {
        let e = log.event(lv);
        match e.op {
            Op::Insert { .. } => {
                inserted.insert(lv);
            }
            Op::Delete { pos } => {
                took.insert(lv, copies::identities(log, &e.parents)[pos]);
            }
            Op::Undelete { target } => {
                undone.insert(target);
            }
            _ => {}
        }
    }
    let deleted: HashSet<Lv> = took
        .iter()
        .filter(|(d, _)| !undone.contains(*d))
        .map(|(_, &r)| r)
        .collect();
    inserted
        .into_iter()
        .filter(|r| !deleted.contains(r))
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, max_global_rejects: 1 << 20, ..ProptestConfig::default() })]

    /// Three writers: every version shows each live item exactly once.
    #[test]
    fn nothing_lost_or_copied(actions in actions(3, 40)) {
        let (log, versions) = history(&actions, 3);
        for v in &versions {
            let ids = copies::identities(&log, v);
            let shown: BTreeSet<Lv> = ids.iter().copied().collect();
            prop_assert_eq!(shown.len(), ids.len(), "an item shows twice\n{}", describe(&log));
            prop_assert_eq!(shown, live(&log, v), "\n{}", describe(&log));
        }
    }

    /// Three writers, on histories where the portals refuse nothing the
    /// copies take, and with no edge ties (which the two decide
    /// differently, on purpose): the same document, items and outline at
    /// every version.
    #[test]
    fn as_the_portals_where_they_can(actions in actions(3, 40)) {
        let (log, versions) = history(&actions, 3);
        let ours: HashSet<Lv> = copies::inert(&log).into_iter().collect();
        let theirs: HashSet<Lv> = reference::inert(&log).into_iter().collect();
        prop_assume!(ours == theirs && !copies::edge_ties(&log));
        // Typing in a move dropped for a loop: mamre#27, to decide.
        prop_assume!(versions.iter().all(|v| !copies::loops(&log, v)));
        for v in &versions {
            prop_assert_eq!(
                copies::checkout(&log, v),
                reference::checkout(&log, v),
                "at {:?}\n{}", v, describe(&log)
            );
            prop_assert_eq!(
                copies::identities(&log, v),
                reference::identities(&log, v),
                "at {:?}\n{}", v, describe(&log)
            );
            prop_assert_eq!(copies::levels(&log, v), reference::levels(&log, v));
        }
    }
}

// ---- A window, from where everything after saw it ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Replayed from a start every later event saw, flatly where it can:
    /// each flat step is as rebuilt, and the end is as the reference has
    /// it. The start is the last version that everything after included.
    #[test]
    fn a_window_is_the_reference(actions in actions(3, 40)) {
        let (log, versions) = history(&actions, 3);
        let Some(end) = versions.last() else { return Ok(()) };
        let all: Vec<Lv> = (0..log.len()).filter(|&lv| log.contains(end, lv)).collect();
        // Candidates: each event's own closure, latest first; the first
        // that every later event (in the end version) includes.
        let start = all
            .iter()
            .rev()
            .map(|&s| vec![s])
            .find(|s| all.iter().filter(|&&lv| !log.contains(s, lv)).all(|&lv| log.contains(&[lv], s[0])))
            .unwrap_or_default();
        for start in [Vec::new(), start] {
            if let Some(got) = copies::from_start(&log, &start, end) {
                prop_assert_eq!(got, copies::identities(&log, end), "from {:?}\n{}", start, describe(&log));
            }
            // And on runs (#22).
            if let Some(got) = runs::from_start(&log, &start, end) {
                prop_assert_eq!(got, copies::identities(&log, end), "runs from {:?}\n{}", start, describe(&log));
            }
        }
    }
}

// ---- The walk, against the reference ----

fn text(v: &[char]) -> String {
    v.iter().collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The fast walk as the reference has it: every checkout; the changes
    /// between any two versions, applied; and replicas keeping books,
    /// merging as they go, their documents, identities, outline and
    /// losses.
    #[test]
    fn the_walk_is_the_reference(actions in actions(3, 40)) {
        let (log, versions) = history(&actions, 3);
        for v in &versions {
            prop_assert_eq!(text(&log.checkout(v)), text(&copies::checkout(&log, v)), "checkout at {:?}\n{}", v, describe(&log));
        }
        for (a, b) in versions.iter().zip(versions.iter().skip(1)) {
            let mut doc = copies::checkout(&log, a);
            for c in log.changes(a, b) {
                c.apply(&mut doc);
            }
            let both = log.union(a, b);
            prop_assert_eq!(text(&doc), text(&copies::checkout(&log, &both)), "{:?} to {:?}\n{}", a, b, describe(&log));
        }
        // A replica keeping books, merging each version in turn.
        let mut books = mamre_egg_walker::Accountant::new();
        let mut doc: Vec<char> = Vec::new();
        let mut lost: Vec<Lv> = Vec::new();
        struct Doc<'a>(&'a mut Vec<char>, &'a mut Vec<Lv>);
        impl mamre_egg_walker::Sequence<char> for Doc<'_> {
            fn insert(&mut self, pos: usize, items: &[char]) { self.0.insert(pos, items[0]); for (k, &c) in items.iter().enumerate().skip(1) { self.0.insert(pos + k, c); } }
            fn delete(&mut self, range: std::ops::Range<usize>) { self.0.drain(range); }
            fn move_range(&mut self, range: std::ops::Range<usize>, to: usize) { mamre_egg_walker::Sequence::move_range(self.0, range, to) }
            fn lost(&mut self, op: Lv, _: Option<Lv>, _: &[std::ops::Range<usize>]) { self.1.push(op); }
        }
        for v in &versions {
            for c in books.merge(&log, v) {
                c.apply(&mut Doc(&mut doc, &mut lost));
            }
            let at = books.version().to_vec();
            prop_assert_eq!(text(&doc), text(&copies::checkout(&log, &at)), "replica at {:?}\n{}", at, describe(&log));
            let ids: Vec<Lv> = (0..books.len()).map(|p| books.identities().item(p)).collect();
            prop_assert_eq!(ids, copies::identities(&log, &at), "replica ids at {:?}\n{}", at, describe(&log));
            let levels: std::collections::BTreeMap<Lv, u8> = books.sections().iter().map(|s| (s.item, s.level)).collect();
            let want: std::collections::BTreeMap<Lv, u8> = copies::levels(&log, &at)
                .into_iter()
                .filter(|(item, _)| copies::identities(&log, &at).contains(item))
                .collect();
            prop_assert_eq!(levels, want, "outline at {:?}\n{}", at, describe(&log));
        }
        // Everything lost, reported once each, at some merge.
        let end = books.version().to_vec();
        let mut want: Vec<Lv> = copies::losses(&log, &end).into_iter().map(|l| l.op).collect();
        want.sort_unstable();
        lost.sort_unstable();
        lost.dedup();
        for op in &want {
            prop_assert!(lost.contains(op), "{} lost but never reported\n{}", op, describe(&log));
        }
    }
}

// ---- The rules ----

fn text_at(log: &OpLog<char>, v: &[Lv]) -> String {
    copies::checkout(log, v).into_iter().collect()
}

fn typed(log: &mut OpLog<char>, who: &str, text: &str) -> Vec<Lv> {
    let me = log.agent(who);
    log.add_insert(me, &[], 0, text.chars())
}

#[test]
fn a_move_is_cut_and_paste() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "abcdef");
    let a = log.agent("a");
    let v = log.add_move(a, &v, 1..3, 5);
    assert_eq!(text_at(&log, &v), "adebcf");
}

/// What the portals refused: a range cutting into a moved block moves.
#[test]
fn a_range_cutting_into_a_moved_block_moves() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let a = log.agent("a");
    let v = log.add_move(a, &base, 3..6, 0);
    assert_eq!(text_at(&log, &v), "BB.AA.CC.");
    // "B.A": half in the moved block, half out.
    let w = log.add_move(a, &v, 1..4, 9);
    assert_eq!(text_at(&log, &w), "BA.CC.B.A");
    assert!(copies::losses(&log, &w).is_empty());
}

#[test]
fn concurrent_typing_follows_the_move() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let moved = log.add_move(a, &base, 4..8, 0);
    let typing = log.add_insert(b, &base, 5, "xy".chars());
    assert_eq!(text_at(&log, &log.union(&moved, &typing)), "BxyBB.AAA.");
}

#[test]
fn typing_at_the_edge_stays() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.CCC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let moved = log.add_move(a, &base, 4..8, 0);
    let typing = log.add_insert(b, &base, 8, "x".chars());
    assert_eq!(text_at(&log, &log.union(&moved, &typing)), "BBB.AAA.xCCC.");
}

/// The same block moved twice at once: the later wins, and the earlier is
/// reported lost to it, with where its items are now.
#[test]
fn the_same_block_moved_twice_reports_the_loser() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let to_front = log.add_move(a, &base, 3..6, 0);
    let to_end = log.add_move(b, &base, 3..6, 9);
    let both = log.union(&to_front, &to_end);
    assert_eq!(text_at(&log, &both), "AA.CC.BB.");
    assert_eq!(
        copies::losses(&log, &both),
        vec![Loss {
            op: to_front[0],
            lost_to: Some(to_end[0]),
            ranges: vec![6..9]
        }]
    );
}

/// Typing into a block by someone who saw it moved, while someone else
/// moved it elsewhere and won: the typing goes where the block went.
#[test]
fn typing_in_a_losing_move_follows_the_winner() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let to_front = log.add_move(a, &base, 3..6, 0);
    let typing = log.add_insert(a, &to_front, 1, ['x']);
    assert_eq!(text_at(&log, &typing), "BxB.AA.CC.");
    let to_end = log.add_move(b, &base, 3..6, 9);
    assert_eq!(text_at(&log, &log.union(&typing, &to_end)), "AA.CC.BxB.");
}

#[test]
fn a_moved_block_moves_again() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let first = log.add_move(a, &base, 3..6, 0);
    let second = log.add_move(b, &first, 0..3, 9);
    assert_eq!(text_at(&log, &second), "AA.CC.BB.");
}

#[test]
fn moves_into_each_other_drop_the_later() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAAA.BBBB.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let a_into_b = log.add_move(a, &base, 0..5, 7);
    let b_into_a = log.add_move(b, &base, 5..10, 2);
    let both = log.union(&a_into_b, &b_into_a);
    assert_eq!(text_at(&log, &both), "BBAAAA.BB.");
    let lost = copies::losses(&log, &both);
    assert_eq!(lost.len(), 1);
    assert_eq!(
        (lost[0].op, lost[0].lost_to),
        (b_into_a[0], Some(a_into_b[0]))
    );
}

#[test]
fn crossing_moves_drop_the_later() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.CCC.DDD.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let first = log.add_move(a, &base, 0..12, 16);
    let last = log.add_move(b, &base, 8..16, 0);
    let both = log.union(&first, &last);
    assert_eq!(text_at(&log, &both), "DDD.AAA.BBB.CCC.");
    assert_eq!(
        copies::losses(&log, &both),
        vec![Loss {
            op: last[0],
            lost_to: Some(first[0]),
            ranges: vec![0..4, 12..16]
        }]
    );
}

#[test]
fn a_part_moved_out_of_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let whole = log.add_move(u1, &base, 0..6, 8);
    let part = log.add_move(u2, &base, 2..4, 0);
    assert_eq!(text_at(&log, &log.union(&whole, &part)), "CDGHABEF");
}

#[test]
fn something_moved_into_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let block = log.add_move(u1, &base, 0..4, 8);
    let into = log.add_move(u2, &base, 6..8, 2);
    assert_eq!(text_at(&log, &log.union(&block, &into)), "EFABGHCD");
}

/// An undo brings an item back where it was moved to.
#[test]
fn an_undo_in_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let deleted = log.add_delete(u2, &base, 2..3);
    let moved = log.add_move(u1, &deleted, 0..5, 7);
    assert_eq!(text_at(&log, &moved), "GHABDEF");
    let undone = log.add_undelete(u2, &deleted, [deleted[0]]);
    assert_eq!(text_at(&log, &log.union(&moved, &undone)), "GHABCDEF");
}

/// A move of a block someone deleted at the same time: the delete wins,
/// and the move is reported lost to it.
#[test]
fn a_delete_beats_a_move() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEF");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let moved = log.add_move(u1, &base, 0..2, 6);
    let deleted = log.add_delete(u2, &base, 0..2);
    let both = log.union(&moved, &deleted);
    assert_eq!(text_at(&log, &both), "CDEF");
    let lost = copies::losses(&log, &both);
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0].op, moved[0]);
    assert!(deleted.contains(&lost[0].lost_to.unwrap()));
    assert!(lost[0].ranges.is_empty());
}

/// Two marks on one item at once: the later wins, the other is lost.
#[test]
fn a_mark_overruled_is_lost() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABC");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let one = log.add_mark(u1, &base, 1, 1);
    let two = log.add_mark(u2, &base, 1, 2);
    let both = log.union(&one, &two);
    assert_eq!(copies::levels(&log, &both).get(&1), Some(&2));
    assert_eq!(
        copies::losses(&log, &both),
        vec![Loss {
            op: one[0],
            lost_to: Some(two[0]),
            ranges: vec![1..2]
        }]
    );
    // Marks replaced by one that saw them aren't lost: they were changed.
    let three = log.add_mark(u1, &both, 1, 3);
    assert!(copies::losses(&log, &three).is_empty());
}

/// A move of nothing to anywhere, or of something to where it is: inert,
/// and lost to nothing.
#[test]
fn an_inert_move_is_lost_to_nothing() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABC");
    let u = log.agent("u");
    let v = log.add_move(u, &base, 0..2, 2);
    assert_eq!(text_at(&log, &v), "ABC");
    assert_eq!(
        copies::losses(&log, &v),
        vec![Loss {
            op: v[0],
            lost_to: None,
            ranges: vec![]
        }]
    );
}

/// Moved nested deep, then something done at the same time as all of it:
/// nothing nests, so it's cheap. (The portals take seconds here.)
#[test]
fn deep_moves_stay_flat() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let mut v = log.add_insert(a, &[], 0, ['x']);
    let base = v.clone();
    for _ in 0..30 {
        let len = copies::checkout(&log, &v).len();
        v = log.add_insert(a, &v, len, ['y']);
        v = log.add_move(a, &v, 0..len, len + 1);
    }
    let b = log.agent("b");
    let typed = log.add_insert(b, &base, 0, ['z']);
    let both = log.union(&v, &typed);
    let got = copies::checkout(&log, &both);
    assert_eq!(got.len(), 32);
    assert_eq!(got.iter().filter(|&&c| c == 'z').count(), 1);
    assert_eq!(got, reference::checkout(&log, &both));
}

/// At the document's start, beside text someone typed and moved at the
/// same time: the author's left was the start, which didn't move, so it
/// stays, even though ties put it among the moved text. (The portals
/// carried it along.)
#[test]
fn an_edge_is_decided_by_what_its_author_saw() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let base = log.add_insert(a, &[], 0, "ZQ".chars());
    let mut v = log.add_insert(a, &base, 0, "xyz".chars());
    v = log.add_move(a, &v, 0..4, 5);
    let typed = log.add_insert(b, &base, 0, ['B']);
    assert_eq!(text_at(&log, &v), "QxyzZ");
    let both = log.union(&v, &typed);
    assert_eq!(text_at(&log, &both), "BQxyzZ");
    assert_ne!(
        reference::checkout(&log, &both).iter().collect::<String>(),
        "BQxyzZ"
    );
}

/// How much of the portal comparison the filter leaves: of many histories,
/// how many it keeps, and how many of those have moves at the same time as
/// each other, or as typing.
#[test]
#[ignore]
fn portal_filter_coverage() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let (mut kept, mut move_move, mut move_type) = (0, 0, 0);
    let n = 2000;
    for _ in 0..n {
        let actions = actions(3, 40).new_tree(&mut runner).unwrap().current();
        let (log, _) = history(&actions, 3);
        let ours: HashSet<Lv> = copies::inert(&log).into_iter().collect();
        let theirs: HashSet<Lv> = reference::inert(&log).into_iter().collect();
        if ours != theirs || copies::edge_ties(&log) {
            continue;
        }
        kept += 1;
        let moves: Vec<Lv> = (0..log.len())
            .filter(|&lv| matches!(log.event(lv).op, Op::Move { .. }))
            .collect();
        let concurrent = |a: Lv, b: Lv| a != b && !log.contains(&[a], b) && !log.contains(&[b], a);
        if moves
            .iter()
            .any(|&a| moves.iter().any(|&b| concurrent(a, b)))
        {
            move_move += 1;
        }
        if moves.iter().any(|&m| {
            (0..log.len()).any(|x| matches!(log.event(x).op, Op::Insert { .. }) && concurrent(m, x))
        }) {
            move_type += 1;
        }
    }
    eprintln!(
        "of {n}: kept {kept}; moves at once {move_move}; a move and typing at once {move_type}"
    );
}

/// Found by the proptest: a moves "BC", b moves just "C" at the same time
/// (inner, so C goes with b), and a, not having seen b, moves its copy of
/// C again. C shows once, with b; a's second move moves nothing, and is
/// reported lost to b's.
/// b moves BCD to the front, then moves AEF in between its copies of B
/// and C. a, not seeing either, moves BC (inside b's first block) to just
/// before F. a's move is inner, so it wins B and C, and b's copies of them
/// give way: laid out, not drawn. AEF stays where b put it among those
/// copies, before D, with a's BC among them (a put BC before F, which b
/// moved). The portals put D first: a difference like an edge tie, as
/// what's decided is a neighbour another move carried off.
/// b moves EF between A and B, then types X between E and F; a, not
/// seeing it, moves AB between E and F. The two moves each go with the
/// other, so b's (the later) is dropped. X should stay between E and F,
/// as the portals have it; copies sends it to b's move's origin, by the
/// original A (mamre#27).
#[test]
#[ignore = "mamre#27: a rule to decide"]
fn a_dropped_moves_typing_stays_between_its_neighbours() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let e = log.add_insert(a, &[], 0, ['E']);
    let f = log.add_insert(b, &[], 0, ['F']);
    let v = log.add_insert(a, &e, 0, ['D']);
    let v = log.add_delete(a, &v, 0..1);
    let v = log.add_insert(a, &v, 0, ['B']);
    let v = log.add_insert(a, &v, 0, ['A']);
    let base = log.union(&f, &v);
    assert_eq!(text_at(&log, &base), "ABEF");
    let nothing = log.add_move(b, &base, 0..1, 0);
    let ef = log.add_move(b, &nothing, 2..4, 1);
    assert_eq!(text_at(&log, &ef), "AEFB");
    let x = log.add_insert(b, &ef, 2, ['X']);
    assert_eq!(text_at(&log, &x), "AEXFB");
    let ab = log.add_move(a, &base, 0..2, 3);
    assert_eq!(text_at(&log, &ab), "EABF");
    let all = log.union(&x, &ab);
    assert_eq!(text_at(&log, &all), "EABXF");
}

#[test]
fn typing_among_copies_that_gave_way_stays_among_them() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let mut v = Vec::new();
    for c in "EDCBA".chars() {
        v = log.add_insert(a, &v, 0, [c]);
    }
    let f = log.add_insert(b, &[], 0, ['F']);
    let base = log.union(&v, &f);
    assert_eq!(text_at(&log, &base), "ABCDEF");
    let first = log.add_move(b, &base, 1..4, 0);
    assert_eq!(text_at(&log, &first), "BCDAEF");
    let second = log.add_move(b, &first, 3..6, 1);
    assert_eq!(text_at(&log, &second), "BAEFCD");
    let x = log.add_insert(b, &second, 0, ['X']);
    let inner = log.add_move(a, &base, 1..3, 5);
    assert_eq!(text_at(&log, &inner), "ADEBCF");
    let all = log.union(&x, &inner);
    assert_eq!(text_at(&log, &all), "XAEBCFD");
    assert!(copies::losses(&log, &all).is_empty());
    assert!(copies::edge_ties(&log), "the portals may differ here");
    assert_eq!(
        log.checkout(&all).into_iter().collect::<String>(),
        "XAEBCFD"
    );
}

#[test]
fn a_copy_that_gave_way_moved_again_moves_nothing() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let abc = log.add_insert(a, &[], 0, "ABC".chars());
    let first = log.add_move(a, &abc, 1..3, 0);
    assert_eq!(text_at(&log, &first), "BCA");
    let inner = log.add_move(b, &abc, 2..3, 0);
    assert_eq!(text_at(&log, &inner), "CAB");
    let again = log.add_move(a, &first, 1..2, 0);
    assert_eq!(text_at(&log, &again), "CBA");
    let all = log.union(&again, &inner);
    // Both sent their text to the front: a's B sorts before b's C.
    assert_eq!(text_at(&log, &all), "BCA");
    let lost = copies::losses(&log, &all);
    assert_eq!(lost.len(), 1);
    assert_eq!((lost[0].op, lost[0].lost_to), (again[0], Some(inner[0])));
}

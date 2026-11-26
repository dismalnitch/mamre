//! Moves: the walk against the reference for moves as copies (#20), from
//! any version to any other, with replicas keeping books; the rules for
//! what wins, one test each; and the histories that once made the walk
//! and a reference disagree, kept. (One writer, and the reference itself,
//! are checked in `copies.rs`.)

// The reference for moves as copies (#20).
use mamre_egg_walker::{Lv, OpLog, copies as reference};
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
    /// Undo one of the deletes this writer's version holds.
    Undo(prop::sample::Index),
    /// Mark the item there at a level (0 unmarks).
    Mark(prop::sample::Index, u8),
}

// ---- Many writers: nothing lost, nothing copied ----

#[derive(Clone, Debug)]
enum Action {
    Edit(usize, Edit),
    /// One writer takes in everything another has.
    Merge(usize, usize),
}

fn actions() -> impl Strategy<Value = Vec<Action>> {
    let edit = prop_oneof![
        4 => (any::<prop::sample::Index>(), prop::char::range('a', 'z')).prop_map(|(p, c)| Edit::Insert(p, c)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Delete),
        3 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b, t)| Edit::Move(a, b, t)),
    ];
    let edit = prop_oneof![
        8 => edit,
        1 => any::<prop::sample::Index>().prop_map(Edit::Undo),
        2 => (any::<prop::sample::Index>(), 0u8..4).prop_map(|(p, l)| Edit::Mark(p, l)),
    ];
    let action = prop_oneof![
        4 => (0..3usize, edit).prop_map(|(w, e)| Action::Edit(w, e)),
        1 => (0..3usize, 0..3usize).prop_map(|(a, b)| Action::Merge(a, b)),
    ];
    prop::collection::vec(action, 1..50)
}

/// Every item a version has inserted and not deleted (or deleted and
/// undone: there are no undeletes here), once each.
fn live(log: &OpLog<char>, version: &[Lv]) -> Vec<Lv> {
    let mut at = vec![false; log.len()];
    let mut stack = version.to_vec();
    while let Some(lv) = stack.pop() {
        if !at[lv] {
            at[lv] = true;
            stack.extend(log.event(lv).parents);
        }
    }
    let mut deleted = std::collections::HashSet::new();
    for lv in (0..log.len()).filter(|&lv| at[lv]) {
        if let mamre_egg_walker::Op::Delete { .. } = log.event(lv).op {
            // Which item it deleted: the reference knows; ask it for the
            // items just before and after.
            let before = reference::identities(log, &log.event(lv).parents);
            let after = reference::identities(log, &[lv]);
            let gone: Vec<&Lv> = before.iter().filter(|id| !after.contains(id)).collect();
            assert_eq!(gone.len(), 1, "a delete removes one item");
            deleted.insert(*gone[0]);
        }
    }
    let mut ids: Vec<Lv> = (0..log.len())
        .filter(|&lv| {
            at[lv]
                && matches!(log.event(lv).op, mamre_egg_walker::Op::Insert { .. })
                && !deleted.contains(&lv)
        })
        .collect();
    ids.sort_unstable();
    ids
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Three writers editing and moving at once, merging now and then:
    /// every version shows each live item exactly once.
    #[test]
    fn moves_never_lose_or_copy(actions in actions()) {
        let mut log: OpLog<char> = OpLog::new();
        let agents: Vec<_> = ["a", "b", "c"].iter().map(|n| log.agent(n)).collect();
        let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); 3];
        for action in actions {
            match action {
                Action::Merge(a, b) => {
                    versions[a] = log.union(&versions[a], &versions[b]);
                }
                Action::Edit(w, edit) => {
                    let v = versions[w].clone();
                    let len = reference::checkout(&log, &v).len();
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
                        _ => continue,
                    };
                }
            }
            for v in &versions {
                let mut shown = reference::identities(&log, v);
                shown.sort_unstable();
                prop_assert_eq!(shown, live(&log, v));
            }
        }
    }
}

// ---- The rules, one each ----

fn text_at(log: &OpLog<char>, v: &[Lv]) -> String {
    reference::checkout(log, v).into_iter().collect()
}

fn typed(log: &mut OpLog<char>, who: &str, text: &str) -> Vec<Lv> {
    let me = log.agent(who);
    log.add_insert(me, &[], 0, text.chars())
}

fn merged(log: &OpLog<char>, a: &[Lv], b: &[Lv]) -> Vec<Lv> {
    log.union(a, b)
}

#[test]
fn a_move_is_cut_and_paste() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "abcdef");
    let a = log.agent("a");
    let v = log.add_move(a, &v, 1..3, 5);
    assert_eq!(text_at(&log, &v), "adebcf");
}

/// Typing inside a section while someone moves it: the typing goes with
/// the section.
#[test]
fn concurrent_typing_follows_the_move() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    // a moves "BBB." to the front; b types inside "BBB", not having seen.
    let moved = log.add_move(a, &base, 4..8, 0);
    let typing = log.add_insert(b, &base, 5, "xy".chars());
    assert_eq!(text_at(&log, &moved), "BBB.AAA.");
    assert_eq!(text_at(&log, &typing), "AAA.BxyBB.");
    assert_eq!(text_at(&log, &merged(&log, &moved, &typing)), "BxyBB.AAA.");
}

/// Typing just after a moved section, at its old place: the edge stays.
#[test]
fn typing_at_the_edge_stays() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.CCC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let moved = log.add_move(a, &base, 4..8, 0);
    // Right after "BBB.", before "CCC.".
    let typing = log.add_insert(b, &base, 8, "x".chars());
    assert_eq!(
        text_at(&log, &merged(&log, &moved, &typing)),
        "BBB.AAA.xCCC."
    );
}

/// Two writers move the same block at once: the later move wins, and the
/// block is shown once.
#[test]
fn the_same_block_moved_twice_shows_once() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let to_front = log.add_move(a, &base, 3..6, 0);
    let to_end = log.add_move(b, &base, 3..6, 9);
    let both = merged(&log, &to_front, &to_end);
    let got = text_at(&log, &both);
    assert!(
        got == "BB.AA.CC." || got == "AA.CC.BB.",
        "one move wins, whole: {got}"
    );
    // "b" sorts after "a" at the same Lamport time: its move is later.
    assert_eq!(got, "AA.CC.BB.");
}

/// A block moved, then moved again by someone who saw the first move.
#[test]
fn a_moved_block_moves_again() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let first = log.add_move(a, &base, 3..6, 0);
    assert_eq!(text_at(&log, &first), "BB.AA.CC.");
    let second = log.add_move(b, &first, 0..3, 9);
    assert_eq!(text_at(&log, &second), "AA.CC.BB.");
}

/// A moved into B while B is moved into A: a loop, and one move is
/// dropped, so both blocks still show once.
#[test]
fn moves_into_each_other_drop_one() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAAA.BBBB.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    // A ("AAAA.") into the middle of B; B into the middle of A.
    let a_into_b = log.add_move(a, &base, 0..5, 7);
    let b_into_a = log.add_move(b, &base, 5..10, 2);
    assert_eq!(text_at(&log, &a_into_b), "BBAAAA.BB.");
    assert_eq!(text_at(&log, &b_into_a), "AABBBB.AA.");
    let got = text_at(&log, &merged(&log, &a_into_b, &b_into_a));
    // b's move is later, and makes the loop: it's dropped.
    assert_eq!(got, "BBAAAA.BB.");
}

#[test]
fn crossing_moves_drop_the_later() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AAA.BBB.CCC.DDD.");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let first_three_to_end = log.add_move(a, &base, 0..12, 16);
    let last_two_to_front = log.add_move(b, &base, 8..16, 0);
    assert_eq!(text_at(&log, &first_three_to_end), "DDD.AAA.BBB.CCC.");
    assert_eq!(text_at(&log, &last_two_to_front), "CCC.DDD.AAA.BBB.");
    let both = merged(&log, &first_three_to_end, &last_two_to_front);
    assert_eq!(text_at(&log, &both), "DDD.AAA.BBB.CCC.");
}

// ---- The simple walk, against the reference ----

/// Applies `changes` to a document and its identities.
fn apply(changes: &[mamre_egg_walker::Change<'_, char>], doc: &mut Vec<char>, ids: &mut Vec<Lv>) {
    use mamre_egg_walker::Change;
    for c in changes {
        c.apply(doc);
        match c {
            Change::Insert { pos, items, first } => {
                ids.splice(*pos..*pos, *first..*first + items.len());
            }
            Change::Delete { range } => {
                ids.drain(range.clone());
            }
            Change::Mark { .. } | Change::Lost { .. } => {}
            Change::Move { range, to } => {
                let moved: Vec<Lv> = ids.drain(range.clone()).collect();
                let at = if *to > range.start {
                    to - range.len()
                } else {
                    *to
                };
                ids.splice(at..at, moved);
            }
        }
    }
}

/// Applies the mark changes among `changes` to `levels`, checking each
/// position names its item in the document as changed so far.
fn apply_marks(
    changes: &[mamre_egg_walker::Change<'_, char>],
    ids: &mut Vec<Lv>,
    levels: &mut std::collections::BTreeMap<Lv, u8>,
) {
    use mamre_egg_walker::Change;
    let mut doc: Vec<char> = vec!['?'; ids.len()];
    for c in changes {
        if let Change::Mark { item, pos, level } = c {
            if let Some(p) = pos {
                assert_eq!(ids[*p], *item, "a mark's position names its item");
            } else {
                assert!(
                    !ids.contains(item),
                    "a mark with no position is of a hidden item"
                );
            }
            if *level == 0 {
                levels.remove(item);
            } else {
                levels.insert(*item, *level);
            }
        }
        apply(std::slice::from_ref(c), &mut doc, ids);
    }
}

/// Every event in `log`, a line each: for failure messages.
fn describe(log: &OpLog<char>) -> String {
    (0..log.len())
        .map(|lv| {
            let e = log.event(lv);
            format!(
                "{lv}: {} {:?} after {:?}\n",
                log.agent_name(e.agent),
                e.op,
                e.parents
            )
        })
        .collect()
}

/// Builds a three-writer history from `actions`, returning each writer's
/// version.
fn history(actions: Vec<Action>) -> (OpLog<char>, Vec<Vec<Lv>>) {
    let mut log: OpLog<char> = OpLog::new();
    let agents: Vec<_> = ["a", "b", "c"].iter().map(|n| log.agent(n)).collect();
    let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); 3];
    for action in actions {
        match action {
            Action::Merge(a, b) => versions[a] = log.union(&versions[a], &versions[b]),
            Action::Edit(w, edit) => {
                let v = versions[w].clone();
                let len = reference::checkout(&log, &v).len();
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
                                    && matches!(
                                        log.event(lv).op,
                                        mamre_egg_walker::Op::Delete { .. }
                                    )
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
    }
    (log, versions)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// From any writer's version to any other's: the walk's changes
    /// make the reference's merged document, item for item.
    #[test]
    fn the_walk_merges_like_the_reference(actions in actions()) {
        let (log, versions) = history(actions);
        for from in &versions {
            for to in &versions {
                let mut doc = reference::checkout(&log, from);
                let mut ids = reference::identities(&log, from);
                let changes = log.changes(from, to);
                apply(&changes, &mut doc, &mut ids);
                let both = log.union(from, to);
                let mut levels = reference::levels(&log, from);
                apply_marks(&changes, &mut reference::identities(&log, from), &mut levels);
                prop_assert_eq!(
                    &levels, &reference::levels(&log, &both),
                    "levels from {:?} to {:?}, in\n{}", from, to, describe(&log)
                );
                prop_assert_eq!(
                    doc.iter().collect::<String>(),
                    reference::checkout(&log, &both).iter().collect::<String>(),
                    "from {:?} to {:?}, in\n{}", from, to, describe(&log)
                );
                prop_assert_eq!(
                    ids, reference::identities(&log, &both),
                    "from {:?} to {:?}, in\n{}", from, to, describe(&log)
                );
            }
        }
    }
}

// ---- Regressions: histories that once made the walk and the reference disagree ----

/// Every pair of `versions`: the walk's changes make the
/// reference's merged document, characters and identities.
fn merges_agree(log: &OpLog<char>, versions: &[Vec<Lv>]) {
    for from in versions {
        for to in versions {
            let mut doc = reference::checkout(log, from);
            let mut ids = reference::identities(log, from);
            apply(&log.changes(from, to), &mut doc, &mut ids);
            let both = log.union(from, to);
            assert_eq!(
                doc,
                reference::checkout(log, &both),
                "from {from:?} to {to:?}"
            );
            assert_eq!(
                ids,
                reference::identities(log, &both),
                "from {from:?} to {to:?}"
            );
        }
    }
}

/// Two moved blocks starting at the same item, one inside the other:
/// finding the next element must stop at the outer block's start.
#[test]
fn nested_blocks_sharing_a_start() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b, c) = (log.agent("a"), log.agent("b"), log.agent("c"));
    let v1 = log.add_insert(b, &[], 0, ['a']);
    let v0 = log.add_insert(a, &[], 0, ['a']);
    let v1 = log.add_insert(b, &v1, 1, ['a']);
    let v0 = log.union(&v0, &v1);
    let v0 = log.add_insert(a, &v0, 2, ['a']);
    let v1 = log.add_insert(b, &v1, 0, ['a']);
    let v1 = log.add_insert(b, &v1, 0, ['a']);
    let v2 = log.add_insert(c, &[], 0, ['a']);
    let v0 = log.add_move(a, &v0, 0..2, 3);
    let v0 = log.add_move(a, &v0, 0..1, 2);
    let v0 = log.add_move(a, &v0, 0..1, 4);
    let v0 = log.add_insert(a, &v0, 0, ['u']);
    merges_agree(&log, &[v0, v1, v2]);
}

/// A move whose range starts inside a moved block and ends outside it:
/// splitting out one end mustn't lose track of the other.
#[test]
fn a_moves_ends_are_found_by_id() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, c) = (log.agent("a"), log.agent("c"));
    let mut v = Vec::new();
    for _ in 0..4 {
        v = log.add_insert(a, &v, 0, ['a']);
    }
    let v = log.add_move(a, &v, 0..2, 0);
    let mine = log.add_insert(a, &v, 0, ['a']);
    let theirs = log.add_move(c, &v, 3..4, 0);
    let theirs = log.add_move(c, &theirs, 0..2, 3);
    merges_agree(&log, &[mine, theirs]);
}

/// Typing at the end of a moved block whose last item was deleted: inside
/// or out is decided by what shows, so the base (which keeps no tombstones)
/// decides it the same way.
#[test]
fn a_blocks_edge_is_decided_by_what_shows() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, c) = (log.agent("a"), log.agent("c"));
    let c0 = log.add_insert(c, &[], 0, ['a']);
    let a1 = log.add_insert(a, &[], 0, ['a']);
    let a2 = log.add_insert(a, &a1, 0, ['a']);
    let a3 = log.add_insert(a, &a2, 0, ['a']);
    let c4 = log.add_insert(c, &c0, 0, ['a']);
    let c5 = log.add_insert(c, &c4, 0, ['a']);
    let a6 = log.add_insert(a, &a3, 0, ['a']);
    let c7 = log.add_insert(c, &c5, 0, ['a']);
    let a8 = log.add_insert(a, &a6, 0, ['a']);
    let c9 = log.add_move(c, &c7, 0..3, 4);
    let both = log.union(&a8, &c9);
    let a10 = log.add_delete(a, &both, 8..9);
    let c11 = log.add_insert(c, &a10, 8, ['a']);
    let a12 = log.add_move(a, &a10, 1..8, 0);
    merges_agree(&log, &[a12, c11]);
}

// ---- The fast walk, against the reference ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The fast walk, every way it's used: a merge from any writer's version
    /// to any other's, by `changes` and by `changes_with` from a replica's
    /// identities, and checking out each version from nothing.
    #[test]
    fn the_fast_walk_merges_like_the_reference(actions in actions()) {
        let (log, versions) = history(actions);
        for from in &versions {
            prop_assert_eq!(
                log.checkout(from),
                reference::checkout(&log, from),
                "checkout {:?}, in\n{}", from, describe(&log)
            );
            for to in &versions {
                let both = log.union(from, to);
                let want = reference::checkout(&log, &both);
                let want_ids = reference::identities(&log, &both);

                let mut doc = reference::checkout(&log, from);
                let mut ids = reference::identities(&log, from);
                let changes = log.changes(from, to);
                apply(&changes, &mut doc, &mut ids);
                prop_assert_eq!(&doc, &want, "changes {:?} to {:?}, in\n{}", from, to, describe(&log));
                let mut levels = reference::levels(&log, from);
                apply_marks(&changes, &mut reference::identities(&log, from), &mut levels);
                prop_assert_eq!(&levels, &reference::levels(&log, &both), "levels {:?} to {:?}, in\n{}", from, to, describe(&log));
                prop_assert_eq!(&ids, &want_ids, "changes {:?} to {:?}, in\n{}", from, to, describe(&log));

                // A replica that keeps its identities (and so its blocks).
                let mut kept = mamre_egg_walker::Identities::new();
                let mut doc = Vec::new();
                for c in log.changes_with(&[], from, &mut kept) {
                    c.apply(&mut doc);
                }
                for c in log.changes_with(from, to, &mut kept) {
                    c.apply(&mut doc);
                }
                let kept_ids: Vec<Lv> = (0..kept.len()).map(|i| kept.item(i)).collect();

                // An accountant, which keeps the blocks too: to `from`, then
                // to the merge.
                let mut books = mamre_egg_walker::Accountant::new();
                let mut kept_doc = Vec::new();
                for c in books.merge(&log, from) {
                    c.apply(&mut kept_doc);
                }
                for c in books.merge(&log, to) {
                    c.apply(&mut kept_doc);
                }
                let books_ids: Vec<Lv> =
                    (0..books.len()).map(|i| books.identities().item(i)).collect();
                prop_assert_eq!(&kept_doc, &want, "accountant {:?} to {:?}, in\n{}", from, to, describe(&log));
                prop_assert_eq!(&books_ids, &want_ids, "accountant {:?} to {:?}, in\n{}", from, to, describe(&log));
                // Its outline: every marked item's level, shown or not, and
                // the sections it shows, in order, where they are.
                let want_levels = reference::levels(&log, &both);
                for item in 0..log.len() {
                    let want = want_levels.get(&item).copied().unwrap_or(0);
                    prop_assert_eq!(books.level(item), want, "level of {}, in\n{}", item, describe(&log));
                }
                let want_sections: Vec<(usize, Lv, u8)> = want_ids
                    .iter()
                    .enumerate()
                    .filter_map(|(pos, id)| want_levels.get(id).map(|&l| (pos, *id, l)))
                    .collect();
                let got_sections: Vec<(usize, Lv, u8)> =
                    books.sections().iter().map(|s| (s.pos, s.item, s.level)).collect();
                prop_assert_eq!(got_sections, want_sections, "sections, in\n{}", describe(&log));
                prop_assert_eq!(&doc, &want, "changes_with {:?} to {:?}, in\n{}", from, to, describe(&log));
                prop_assert_eq!(&kept_ids, &want_ids, "changes_with {:?} to {:?}, in\n{}", from, to, describe(&log));
            }
        }
    }
}

/// U1 moves a run to the end while U2 deletes its last two items: each
/// takes in the other's, and both end the same. The deleted items are
/// still the run's end, as tombstones, so the move needs nothing from them.
#[test]
fn a_move_whose_end_is_deleted_meanwhile() {
    let mut log = OpLog::new();
    // a1 a2 a3 b1 b2 b3 c1 c2, one char each.
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let moved = log.add_move(u1, &base, 0..6, 8);
    let deleted = log.add_delete(u2, &base, 4..6);
    assert_eq!(text_at(&log, &moved), "GHABCDEF");
    assert_eq!(text_at(&log, &deleted), "ABCDGH");
    let both = merged(&log, &moved, &deleted);
    assert_eq!(text_at(&log, &both), "GHABCD");
    // And each replica gets there by taking in the other's edit.
    let mut doc = reference::checkout(&log, &moved);
    for c in log.changes(&moved, &deleted) {
        c.apply(&mut doc);
    }
    assert_eq!(doc.iter().collect::<String>(), "GHABCD");
    let mut doc = reference::checkout(&log, &deleted);
    for c in log.changes(&deleted, &moved) {
        c.apply(&mut doc);
    }
    assert_eq!(doc.iter().collect::<String>(), "GHABCD");
}

// ---- The middle of a moved block, while it moves ----

/// Both merge orders, and the merged version, show `want`.
fn settles(log: &OpLog<char>, a: &[Lv], b: &[Lv], want: &str) {
    assert_eq!(text_at(log, &merged(log, a, b)), want, "merged");
    for (from, to) in [(a, b), (b, a)] {
        let mut doc = reference::checkout(log, from);
        for c in log.changes(from, to) {
            c.apply(&mut doc);
        }
        assert_eq!(doc.iter().collect::<String>(), want, "from {from:?}");
    }
}

/// Deleting in the middle: hidden wherever the block goes.
#[test]
fn a_delete_in_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let moved = log.add_move(u1, &base, 0..6, 8);
    let deleted = log.add_delete(u2, &base, 2..4);
    settles(&log, &moved, &deleted, "GHABEF");
}

/// Undoing a delete in the middle, after the block moved: the same item
/// comes back, inside the block, at its new place.
#[test]
fn an_undo_in_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let deleted = log.add_delete(u2, &base, 2..3);
    let moved = log.add_move(u1, &deleted, 0..5, 7);
    assert_eq!(text_at(&log, &moved), "GHABDEF");
    let undone = log.add_undelete(u2, &deleted, [deleted[0]]);
    assert_eq!(text_at(&log, &undone), "ABCDEFGH");
    settles(&log, &moved, &undone, "GHABCDEF");
}

/// Moving part of the middle out, while the whole moves: both happen. The
/// block goes, without the part, and the part goes where it was sent.
#[test]
fn a_part_moved_out_of_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let whole = log.add_move(u1, &base, 0..6, 8);
    let part = log.add_move(u2, &base, 2..4, 0);
    assert_eq!(text_at(&log, &whole), "GHABCDEF");
    assert_eq!(text_at(&log, &part), "CDABEFGH");
    settles(&log, &whole, &part, "CDGHABEF");
}

/// Moving something into the middle, while the block moves: it lands
/// inside, and goes with the block.
#[test]
fn something_moved_into_the_middle() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let block = log.add_move(u1, &base, 0..4, 8);
    let into = log.add_move(u2, &base, 6..8, 2);
    assert_eq!(text_at(&log, &block), "EFGHABCD");
    assert_eq!(text_at(&log, &into), "ABGHCDEF");
    settles(&log, &block, &into, "EFABGHCD");
}

/// Deleting everything in the middle, then typing where it was: nothing
/// shows inside the block, so the typing is outside it. When the block
/// moves, it goes without the typing.
#[test]
fn an_emptied_block_takes_no_typing() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEFGH");
    let u1 = log.agent("u1");
    let first = log.add_move(u1, &base, 2..4, 8);
    assert_eq!(text_at(&log, &first), "ABEFGHCD");
    let emptied = log.add_delete(u1, &first, 6..8);
    let typed_there = log.add_insert(u1, &emptied, 6, "xy".chars());
    assert_eq!(text_at(&log, &typed_there), "ABEFGHxy");
    // Moving "the block" again moves nothing visible; "xy" stays.
    let again = log.add_move(u1, &typed_there, 0..2, 8);
    assert_eq!(text_at(&log, &again), "EFGHxyAB");
}

// ---- Marks ----

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Marks in a log with no moves: the plain fast path, against the
    /// reference.
    #[test]
    fn marks_without_moves(actions in actions()) {
        let actions: Vec<Action> = actions
            .into_iter()
            .filter(|a| !matches!(a, Action::Edit(_, Edit::Move(..))))
            .collect();
        let (log, versions) = history(actions);
        for from in &versions {
            for to in &versions {
                let both = log.union(from, to);
                let changes = log.changes(from, to);
                let mut doc = reference::checkout(&log, from);
                let mut ids = reference::identities(&log, from);
                apply(&changes, &mut doc, &mut ids);
                prop_assert_eq!(&doc, &reference::checkout(&log, &both), "in\n{}", describe(&log));
                let mut levels = reference::levels(&log, from);
                apply_marks(&changes, &mut reference::identities(&log, from), &mut levels);
                prop_assert_eq!(&levels, &reference::levels(&log, &both), "levels {:?} to {:?}, in\n{}", from, to, describe(&log));
            }
        }
    }
}

/// Levels of `version`'s visible items, in order, as text: `.` for none.
fn outline_of(log: &OpLog<char>, version: &[Lv]) -> String {
    let levels = reference::levels(log, version);
    reference::identities(log, version)
        .iter()
        .map(|id| levels.get(id).map_or('.', |l| char::from(b'0' + l)))
        .collect()
}

/// Two writers mark the same item at once: the later mark wins, on every
/// replica.
#[test]
fn concurrent_marks_keep_the_latest() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCD");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let one = log.add_mark(a, &base, 1, 1);
    let two = log.add_mark(b, &base, 1, 2);
    // Same Lamport time; "b" orders after "a".
    assert_eq!(outline_of(&log, &merged(&log, &one, &two)), ".2..");
}

/// A mark is its item's: it goes where the item goes.
#[test]
fn a_mark_goes_with_its_item() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCDEF");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let marked = log.add_mark(a, &base, 2, 1);
    let moved = log.add_move(b, &base, 2..4, 6);
    let both = merged(&log, &marked, &moved);
    assert_eq!(text_at(&log, &both), "ABEFCD");
    assert_eq!(outline_of(&log, &both), "....1.");
}

/// Marking an item while someone deletes it: the mark stays the item's,
/// and comes back with it when the delete is undone.
#[test]
fn a_mark_waits_out_a_delete() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "u", "ABCD");
    let (a, b) = (log.agent("a"), log.agent("b"));
    let marked = log.add_mark(a, &base, 2, 3);
    let deleted = log.add_delete(b, &base, 2..3);
    let both = merged(&log, &marked, &deleted);
    assert_eq!(text_at(&log, &both), "ABD");
    let undone = log.add_undelete(b, &both, [deleted[0]]);
    assert_eq!(text_at(&log, &undone), "ABCD");
    assert_eq!(outline_of(&log, &undone), "..3.");
}

/// The outline at work: three chapters, marked; one moved by its section
/// range while someone types in it and renames its heading. The section is
/// the same one throughout, by identity.
#[test]
fn moving_a_marked_chapter() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let me = log.agent("me");
    let mut books = Accountant::new();
    let mut doc: Vec<char> = Vec::new();
    let text = "One\nfirst\nTwo\nsecond\nThree\nthird\n";
    let v = log.add_insert(me, &[], 0, text.chars());
    for c in books.merge(&log, &v) {
        c.apply(&mut doc);
    }
    for pos in [0, 10, 21] {
        let v = log.add_mark(me, books.version(), pos, 1);
        books.merge(&log, &v);
    }
    let sections = books.sections();
    assert_eq!(
        sections.iter().map(|s| s.pos).collect::<Vec<_>>(),
        [0, 10, 21]
    );
    let two = sections[1].item;
    assert_eq!(books.section_range(two), Some(10..21));

    // Someone else, at the same time, types inside chapter Two and renames
    // its heading (deleting "Two", typing "2").
    let them = log.agent("them");
    let base = books.version().to_vec();
    let t = log.add_insert(them, &base, 14, "!".chars());
    let t = log.add_delete(them, &t, 11..13);
    let t = log.add_insert(them, &t, 11, "2".chars());

    // I move chapter Two to the end, by its section range.
    let range = books.section_range(two).unwrap();
    let v = log.add_move(me, &base, range, doc.len());
    for c in books.merge(&log, &v) {
        c.apply(&mut doc);
    }
    assert_eq!(
        doc.iter().collect::<String>(),
        "One\nfirst\nThree\nthird\nTwo\nsecond\n"
    );
    for c in books.merge(&log, &t) {
        c.apply(&mut doc);
    }
    assert_eq!(
        doc.iter().collect::<String>(),
        "One\nfirst\nThree\nthird\nT2\n!second\n"
    );
    // Still three sections; chapter Two is still the same section, at the
    // end, with its range following it.
    let sections = books.sections();
    assert_eq!(sections.len(), 3);
    assert_eq!(sections[2].item, two);
    assert_eq!(books.section_range(two), Some(sections[2].pos..doc.len()));
}

// ---- Live replicas: an accountant each, kept edit by edit ----

/// Three writers, each a replica with its own document and accountant, as
/// `examples/peer.rs` keeps them: typing and deleting are made in the
/// document and recorded; moves, undos and marks, and everything from
/// another writer, are merged. After every action, the writer's document
/// and identities are the reference's at its version. Merges here start
/// from bases behind the writer's own unsent edits, so their books come
/// from the accountant's recent ones.
///
/// A merge takes in the other writer's version as it is now, or as it was
/// some actions ago (`back`), as a message that was slow to arrive brings:
/// then the base is often a version neither writer had.
fn replicas_agree(actions: Vec<(Action, usize)>) -> Result<(), TestCaseError> {
    use mamre_egg_walker::Accountant;
    let mut log: OpLog<char> = OpLog::new();
    let agents: Vec<_> = ["a", "b", "c"].iter().map(|n| log.agent(n)).collect();
    let mut books: Vec<Accountant> = (0..3).map(|_| Accountant::new()).collect();
    let mut docs: Vec<Vec<char>> = vec![Vec::new(); 3];
    // Each writer's versions so far.
    let mut had: Vec<Vec<Vec<Lv>>> = vec![vec![Vec::new()]; 3];
    for (action, back) in actions {
        let w = match action {
            Action::Merge(a, b) => {
                let past = &had[b];
                let theirs = past[past.len() - 1 - back % past.len()].clone();
                for c in books[a].merge(&log, &theirs) {
                    c.apply(&mut docs[a]);
                }
                a
            }
            Action::Edit(w, edit) => {
                let v = books[w].version().to_vec();
                let len = docs[w].len();
                let agent = agents[w];
                let (doc, book) = (&mut docs[w], &mut books[w]);
                match edit {
                    Edit::Insert(p, c) => {
                        let at = p.index(len + 1);
                        doc.insert(at, c);
                        let v = log.add_insert(agent, &v, at, [c]);
                        book.record(&log, &v);
                    }
                    Edit::Delete(p) if len > 0 => {
                        let pos = p.index(len);
                        doc.remove(pos);
                        let v = log.add_delete(agent, &v, pos..pos + 1);
                        book.record(&log, &v);
                    }
                    Edit::Move(a, b, t) if len > 0 => {
                        let (a, b) = (a.index(len), b.index(len));
                        let (a, b) = (a.min(b), a.max(b) + 1);
                        let to = t.index(len + 1);
                        if to > a && to < b {
                            continue;
                        }
                        let v = log.add_move(agent, &v, a..b, to);
                        for c in book.merge(&log, &v) {
                            c.apply(doc);
                        }
                    }
                    Edit::Undo(k) => {
                        let deletes: Vec<Lv> = (0..log.len())
                            .filter(|&lv| {
                                log.contains(&v, lv)
                                    && matches!(
                                        log.event(lv).op,
                                        mamre_egg_walker::Op::Delete { .. }
                                    )
                            })
                            .collect();
                        if deletes.is_empty() {
                            continue;
                        }
                        let v = log.add_undelete(agent, &v, [deletes[k.index(deletes.len())]]);
                        for c in book.merge(&log, &v) {
                            c.apply(doc);
                        }
                    }
                    Edit::Mark(p, level) if len > 0 => {
                        let v = log.add_mark(agent, &v, p.index(len), level);
                        for c in book.merge(&log, &v) {
                            c.apply(doc);
                        }
                    }
                    _ => continue,
                }
                w
            }
        };
        let v = books[w].version().to_vec();
        had[w].push(v.clone());
        prop_assert_eq!(
            &docs[w],
            &reference::checkout(&log, &v),
            "writer {} at {:?}, in\n{}",
            w,
            &v,
            describe(&log)
        );
        let ids: Vec<Lv> = (0..books[w].len())
            .map(|i| books[w].identities().item(i))
            .collect();
        prop_assert_eq!(
            ids,
            reference::identities(&log, &v),
            "writer {}'s identities at {:?}, in\n{}",
            w,
            &v,
            describe(&log)
        );
        // Its blocks: those of the document at its version, found from
        // nothing.
        let mut fresh = Accountant::new();
        fresh.merge(&log, &v);
        let levels = reference::levels(&log, &v);
        for item in 0..log.len() {
            let want = levels.get(&item).copied().unwrap_or(0);
            prop_assert_eq!(
                books[w].level(item),
                want,
                "level of {}, in\n{}",
                item,
                describe(&log)
            );
        }
    }
    Ok(())
}

/// As `actions`, merging more often, for longer.
fn replica_actions() -> impl Strategy<Value = Vec<Action>> {
    let edit = prop_oneof![
        4 => (any::<prop::sample::Index>(), prop::char::range('a', 'z')).prop_map(|(p, c)| Edit::Insert(p, c)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Delete),
        2 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b, t)| Edit::Move(a, b, t)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Undo),
        1 => (any::<prop::sample::Index>(), 0u8..4).prop_map(|(p, l)| Edit::Mark(p, l)),
    ];
    let action = prop_oneof![
        3 => (0..3usize, edit).prop_map(|(w, e)| Action::Edit(w, e)),
        2 => (0..3usize, 0..3usize).prop_map(|(a, b)| Action::Merge(a, b)),
    ];
    prop::collection::vec(action, 1..80)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn live_replicas_keep_their_books(
        actions in replica_actions().prop_flat_map(|a| {
            let n = a.len();
            (Just(a), prop::collection::vec(prop_oneof![1 => Just(0usize), 1 => 0..20usize], n))
        })
    ) {
        let actions = actions.0.into_iter().zip(actions.1).collect();
        replicas_agree(actions)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// An accountant that passed through an earlier version on its way to
    /// a writer's: merging another writer's from there starts from a base
    /// it may never have been at, found by walking forward from the
    /// earlier one. The merge must still be the reference's.
    #[test]
    fn a_merge_from_books_kept_earlier(actions in actions(), picks in prop::collection::vec(any::<prop::sample::Index>(), 6)) {
        use mamre_egg_walker::Accountant;
        let (log, versions) = history(actions);
        for from in &versions {
            let before: Vec<Lv> = (0..log.len()).filter(|&lv| log.contains(from, lv)).collect();
            if before.is_empty() {
                continue;
            }
            for pick in picks.chunks(2) {
                // One event's version, or two's together.
                let earlier = log.union(
                    &[before[pick[0].index(before.len())]],
                    &[before[pick[1].index(before.len())]],
                );
                for to in &versions {
                    let mut books = Accountant::new();
                    let mut doc = Vec::new();
                    for v in [&earlier[..], from, to] {
                        for c in books.merge(&log, v) {
                            c.apply(&mut doc);
                        }
                    }
                    let both = log.union(from, to);
                    prop_assert_eq!(&doc, &reference::checkout(&log, &both), "via {:?}, {:?} to {:?}, in\n{}", earlier, from, to, describe(&log));
                    let ids: Vec<Lv> = (0..books.len()).map(|i| books.identities().item(i)).collect();
                    prop_assert_eq!(ids, reference::identities(&log, &both), "via {:?}, {:?} to {:?}, in\n{}", earlier, from, to, describe(&log));
                    let mut fresh = Accountant::new();
                    fresh.merge(&log, &both);
                }
            }
        }
    }
}

/// The books, as a replica keeps them, against the reference and against
/// an accountant that found them from nothing.
fn books_agree(log: &OpLog<char>, books: &mamre_egg_walker::Accountant, doc: &[char]) {
    let v = books.version().to_vec();
    assert_eq!(doc, &reference::checkout(log, &v)[..], "document");
    let ids: Vec<Lv> = (0..books.len())
        .map(|i| books.identities().item(i))
        .collect();
    assert_eq!(ids, reference::identities(log, &v), "identities");
    let mut fresh = mamre_egg_walker::Accountant::new();
    fresh.merge(log, &v);
}

/// A replica that took in two of b's edits at once merges c's, made after
/// only the first: the base is between them, where the replica never was.
/// Its books there are found by walking forward from the version before,
/// blocks and all: c's typing lands inside a moved block.
#[test]
fn a_base_the_replica_never_was_at() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let (a, b, c) = (log.agent("a"), log.agent("b"), log.agent("c"));
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    // "cdefab", with "ab" a moved block.
    let k1 = log.add_move(a, &v, 0..2, 6);
    let b1 = log.add_insert(b, &k1, 0, ['x']);
    let b2 = log.add_insert(b, &b1, 0, ['y']);
    // c sees "xcdefab" and types between a and b.
    let c1 = log.add_insert(c, &b1, 6, ['w']);
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    for v in [&k1, &b2] {
        for ch in books.merge(&log, v) {
            ch.apply(&mut doc);
        }
    }
    // An unsent edit of a's own.
    doc.insert(3, 'z');
    let a1 = log.add_insert(a, books.version(), 3, ['z']);
    books.record(&log, &a1);
    for ch in books.merge(&log, &c1) {
        ch.apply(&mut doc);
    }
    books_agree(&log, &books, &doc);
}

/// A moved block's last item deleted while the replica types elsewhere:
/// the block then ends at a run of the base's items, which ends where the
/// run does.
#[test]
fn a_block_ending_in_a_run() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let (a, b) = (log.agent("a"), log.agent("b"));
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    // "defabc", with "abc" a moved block.
    let k1 = log.add_move(a, &v, 0..3, 6);
    let b1 = log.add_delete(b, &k1, 5..6);
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    for ch in books.merge(&log, &k1) {
        ch.apply(&mut doc);
    }
    doc.insert(0, 'z');
    let a1 = log.add_insert(a, books.version(), 0, ['z']);
    books.record(&log, &a1);
    for ch in books.merge(&log, &b1) {
        ch.apply(&mut doc);
    }
    books_agree(&log, &books, &doc);
    // "zdefab": the block is what's left of it, "ab", worked out by hand,
    // its tail hidden: its last record, "c", shows nothing.
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// The union of any writers' versions, and of single events in them:
    /// every event either includes, with none another includes.
    #[test]
    fn a_union_keeps_only_the_latest(actions in actions(), picks in prop::collection::vec(any::<prop::sample::Index>(), 4)) {
        let (log, versions) = history(actions);
        if log.len() == 0 {
            return Ok(());
        }
        let mut sets: Vec<Vec<Lv>> = versions.clone();
        sets.extend(picks.iter().map(|p| vec![p.index(log.len())]));
        for a in &sets {
            for b in &sets {
                let mut all: Vec<Lv> = a.iter().chain(b).copied().collect();
                all.sort_unstable();
                all.dedup();
                let want: Vec<Lv> = all
                    .iter()
                    .copied()
                    .filter(|&lv| !all.iter().any(|&o| o != lv && log.contains(&[o], lv)))
                    .collect();
                prop_assert_eq!(log.union(a, b), want, "{:?} with {:?}", a, b);
            }
        }
    }
}

/// Two nested moved blocks that show the same items: move 10's interval
/// holds move 9's and a deleted item besides. The books keep blocks by
/// what they show, so a merge whose base has them laid both over the same
/// base items, and move 10 reads as a move of exactly move 9's stretch,
/// replacing it. A walk from nothing, which sees the deleted item, keeps
/// both. (Found by `live_replicas_keep_their_books` at 1,000 cases.)
#[test]
fn blocks_showing_the_same_items_stay_two() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let (a, b, c) = (log.agent("a"), log.agent("b"), log.agent("c"));
    log.add_insert(a, &[], 0, ['f']);
    log.add_insert(a, &[0], 1, ['w']);
    log.add_insert(a, &[1], 1, ['d']);
    log.add_insert(a, &[2], 3, ['n']);
    log.add_insert(a, &[3], 4, ['n']);
    log.add_insert(a, &[4], 0, ['d']);
    log.add_delete(b, &[5], 2..3);
    log.add_insert(a, &[5], 2, ['u']);
    log.add_delete(b, &[6], 3..4);
    log.add_move(a, &[7], 0..3, 5);
    log.add_move(a, &[9], 2..7, 0);
    log.add_delete(b, &[8], 3..4);
    log.add_mark(b, &[11], 1, 1);
    log.add_move(a, &[10], 0..3, 3);
    log.add_insert(c, &[4], 1, ['b']);
    log.add_delete(b, &[12, 13], 0..1);
    log.add_undelete(a, &[12, 13, 14], [8]);
    let v = log.add_undelete(b, &[15], [15]);
    assert_eq!(v, [17]);
    // A replica at 15 takes in the undo of 15.
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    for to in [&[15][..], &[17]] {
        for ch in books.merge(&log, to) {
            ch.apply(&mut doc);
        }
    }
    assert_eq!(doc, reference::checkout(&log, &[17]));
    let mut fresh = Accountant::new();
    fresh.merge(&log, &[17]);
}

/// A base block whose last item a concurrent move carries away: a later
/// move of a range ending with what's left of the block is still whole
/// elements, as the block's end, cut out, draws nothing where it lies.
#[test]
fn a_block_whose_end_was_moved_away() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let (a, x, y) = (log.agent("a"), log.agent("x"), log.agent("y"));
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    // "aefbcd", with "bcd" a moved block.
    let k = log.add_move(a, &v, 1..4, 6);
    // x moves the block's last item to the front: "daefbc"; then "fbc",
    // what's before and what's left of the block, to the front too.
    let x1 = log.add_move(x, &k, 5..6, 0);
    let x2 = log.add_move(x, &x1, 3..6, 0);
    // y, meanwhile, types.
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    for ch in books.merge(&log, &k) {
        ch.apply(&mut doc);
    }
    doc.insert(0, 'y');
    let y1 = log.add_insert(y, books.version(), 0, ['y']);
    books.record(&log, &y1);
    for ch in books.merge(&log, &x2) {
        ch.apply(&mut doc);
    }
    books_agree(&log, &books, &doc);
    assert_eq!(
        reference::checkout(&log, &x2).iter().collect::<String>(),
        "fbcdae"
    );
}

/// Two nested blocks starting at the same item, each with a hidden head:
/// the outer one's first item deleted, then the inner one's. Their
/// markers go outer first, so the merge's base nests them as they are,
/// and typing at their front from two places lands outside both.
#[test]
fn nested_blocks_both_hiding_their_heads() {
    use mamre_egg_walker::Accountant;
    let mut log = OpLog::new();
    let (a, y, z) = (log.agent("a"), log.agent("y"), log.agent("z"));
    let v = log.add_insert(a, &[], 0, "abcdef".chars());
    // "abefcd", with "cd" moved; then "fcdabe", with "fcd" moved around it.
    let v = log.add_move(a, &v, 2..4, 6);
    let v = log.add_move(a, &v, 3..6, 0);
    // "cdabe", then "dabe": both blocks show "d", and hide their heads.
    let v = log.add_delete(a, &v, 0..1);
    let base = log.add_delete(a, &v, 0..1);
    let mut books = Accountant::new();
    let mut doc = Vec::new();
    for ch in books.merge(&log, &base) {
        ch.apply(&mut doc);
    }
    books_agree(&log, &books, &doc);
    // y types at the front, unsent, as z types there too.
    doc.insert(0, 'y');
    let y1 = log.add_insert(y, books.version(), 0, ['y']);
    books.record(&log, &y1);
    let z1 = log.add_insert(z, &base, 0, ['z']);
    let z2 = log.add_insert(z, &z1, 2, ['!']);
    for ch in books.merge(&log, &z2) {
        ch.apply(&mut doc);
    }
    books_agree(&log, &books, &doc);
}

/// Typing on after a merge: a types X, merges b's Y (which lands after
/// X), and types Z after X, whose right is now Y, not X's. c, from just X,
/// types W after it. Z and W both go after X, with different rights: Z
/// isn't one record with X (#22), or the tie with W goes wrong.
#[test]
fn typing_on_after_a_merge_has_its_own_right() {
    // c's name on either side of a's, so the tie goes both ways.
    for name in ["0", "c"] {
        typing_on_after_a_merge(name);
    }
}

fn typing_on_after_a_merge(name: &str) {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b, c) = (log.agent("a"), log.agent("b"), log.agent(name));
    let y = log.add_insert(b, &[], 0, ['Y']);
    let x = log.add_insert(a, &[], 0, ['X']);
    let both = log.union(&x, &y);
    let at = reference::identities(&log, &both)
        .iter()
        .position(|&i| i == x[0])
        .unwrap()
        + 1;
    let z = log.add_insert(a, &both, at, ['Z']);
    assert_eq!(z[0], x[0] + 1, "Z carries on X's run");
    let w = log.add_insert(c, &x, 1, ['W']);
    // A move somewhere, so the walk opens a window.
    let m = log.add_move(c, &w, 0..1, 2);
    let all = log.union(&z, &m);
    assert_eq!(log.checkout(&all), reference::checkout(&log, &all));
    for (from, to) in [(z.clone(), all.clone()), (m.clone(), all.clone())] {
        let mut doc = log.checkout(&from);
        for ch in log.changes(&from, &to) {
            ch.apply(&mut doc);
        }
        assert_eq!(doc, reference::checkout(&log, &to));
    }
}

/// Found by `the_walk_merges_like_the_reference` (seed kept): typing
/// beside the copies of a move that lost, after merging it, got origins
/// out of order (#27's loop rule maps a copy to what it copied, here an
/// item far to the left), and the window and the reference placed it
/// differently. A right origin before the left now says nothing, in both.
#[test]
fn typing_beside_a_lost_moves_copies_lands_alike_everywhere() {
    let mut log: OpLog<char> = OpLog::new();
    let (a, b, c) = (log.agent("a"), log.agent("b"), log.agent("c"));
    let e0 = log.add_insert(a, &[], 0, ['a']);
    let e1 = log.add_insert(c, &e0, 0, ['a']);
    let e2 = log.add_insert(a, &e1, 0, ['a']);
    let e3 = log.add_insert(b, &e0, 0, ['a']);
    let e4 = log.add_insert(c, &e1, 1, ['a']);
    let e5 = log.add_insert(a, &e2, 0, ['a']);
    let e6 = log.add_move(a, &e5, 0..1, 0);
    let v = log.union(&log.union(&e2, &e3), &e4);
    let e7 = log.add_move(c, &v, 2..5, 1);
    let e8 = log.add_move(a, &log.union(&e3, &e6), 0..3, 4);
    let both = log.union(&e7, &e8);
    let e9 = log.add_insert(a, &both, 3, ['z']);
    let want = mamre_egg_walker::copies::identities(&log, &e9);
    assert_eq!(want, vec![3, 1, 4, 9, 0, 5, 2]);
    // Every replica, merging however it does, gets there: the window is
    // checked against the reference in debug builds.
    for from in [&e7, &e8, &both, &e2] {
        let mut books = mamre_egg_walker::Accountant::new();
        let mut doc = Vec::new();
        for c in books.merge(&log, from) {
            c.apply(&mut doc);
        }
        for c in books.merge(&log, &e9) {
            c.apply(&mut doc);
        }
        let ids: Vec<Lv> = (0..books.len())
            .map(|i| books.identities().item(i))
            .collect();
        assert_eq!(ids, want, "from {from:?}");
    }
}

//! Moves in the reference: what a move does, checked two ways.
//!
//! One writer: against a second model built differently, a tree of
//! blocks, where a move cuts a run of siblings out into a block of its own
//! and puts the block somewhere else. The reference keeps one home order
//! and draws portals; the two must draw the same document.
//!
//! Many writers: a move never loses or copies an item, whatever else
//! happens at the same time; and the rules for what wins, one test each.

use mamre_egg_walker::{Lv, OpLog, reference};
use proptest::prelude::*;

// ---- The tree model, for one writer ----

#[derive(Clone, Debug)]
enum Node {
    Item { ch: char, visible: bool },
    Block(Vec<Node>),
}

/// An item as drawn: where it is in the tree, and whether it shows.
#[derive(Clone, Debug)]
struct Drawn {
    /// Child indices from the root down to the item.
    path: Vec<usize>,
    visible: bool,
}

fn draw(nodes: &[Node], prefix: &mut Vec<usize>, out: &mut Vec<Drawn>) {
    for (i, node) in nodes.iter().enumerate() {
        prefix.push(i);
        match node {
            Node::Item { visible, .. } => out.push(Drawn {
                path: prefix.clone(),
                visible: *visible,
            }),
            Node::Block(children) => draw(children, prefix, out),
        }
        prefix.pop();
    }
}

fn text(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Item { ch, visible: true } => out.push(*ch),
            Node::Item { .. } => {}
            Node::Block(children) => text(children, out),
        }
    }
}

fn children<'a>(root: &'a mut Vec<Node>, path: &[usize]) -> &'a mut Vec<Node> {
    let mut nodes = root;
    for &i in path {
        nodes = match &mut nodes[i] {
            Node::Block(children) => children,
            Node::Item { .. } => unreachable!("a path to a block"),
        };
    }
    nodes
}

fn node_at<'a>(root: &'a mut Vec<Node>, path: &[usize]) -> &'a mut Node {
    let (last, parent) = path.split_last().unwrap();
    &mut children(root, parent)[*last]
}

struct Tree {
    root: Vec<Node>,
}

impl Tree {
    fn drawn(&self) -> Vec<Drawn> {
        let mut out = Vec::new();
        draw(&self.root, &mut Vec::new(), &mut out);
        out
    }

    fn visible(&self) -> Vec<Drawn> {
        self.drawn().into_iter().filter(|d| d.visible).collect()
    }

    /// Where something put in at visible position `pos` goes: the block
    /// (by path) and the index in it, just after the element that draws
    /// the item before it, in their lowest common block with the one after.
    fn slot(&self, pos: usize) -> (Vec<usize>, usize) {
        let drawn = self.drawn();
        let visible: Vec<usize> = (0..drawn.len()).filter(|&k| drawn[k].visible).collect();
        let Some(&left) = pos.checked_sub(1).map(|p| &visible[p]) else {
            return (Vec::new(), 0);
        };
        let lp = &drawn[left].path;
        // Decided by what shows: the next visible item.
        let common = match visible.get(pos).map(|&k| &drawn[k]) {
            None => 0,
            Some(right) => lp
                .iter()
                .zip(&right.path)
                .take_while(|(a, b)| a == b)
                .count()
                .min(lp.len() - 1),
        };
        (lp[..common].to_vec(), lp[common] + 1)
    }

    fn insert(&mut self, pos: usize, ch: char) {
        let (block, i) = self.slot(pos);
        children(&mut self.root, &block).insert(i, Node::Item { ch, visible: true });
    }

    fn delete(&mut self, pos: usize) {
        let path = self.visible()[pos].path.clone();
        if let Node::Item { visible, .. } = node_at(&mut self.root, &path) {
            *visible = false;
        }
    }

    /// Moves the `len` visible items at `pos` to `to`, if the range is a
    /// run of whole siblings: cut out into a block, put in at `to`.
    fn move_(&mut self, pos: usize, len: usize, to: usize) {
        if (pos..=pos + len).contains(&to) {
            return;
        }
        let visible = self.visible();
        if len == 0 || pos + len > visible.len() {
            return;
        }
        let (f, l) = (&visible[pos].path, &visible[pos + len - 1].path);
        let common = f.iter().zip(l).take_while(|(a, b)| a == b).count();
        // Both are items, so their paths differ below the common block.
        let common = common.min(f.len() - 1).min(l.len() - 1);
        let block = f[..common].to_vec();
        let (i, j) = (f[common], l[common]);
        // The run must draw exactly the range.
        let covered: Vec<usize> = (0..visible.len())
            .filter(|&k| {
                let p = &visible[k].path;
                p.len() > common && p[..common] == block[..] && (i..=j).contains(&p[common])
            })
            .collect();
        if covered.len() != len || covered.first() != Some(&pos) {
            return;
        }
        let (dest, at) = self.slot(to);
        // A placeholder where the block goes, so cutting the run out can't
        // lose track of it.
        children(&mut self.root, &dest).insert(at, Node::Block(Vec::new()));
        let mut marker = dest.clone();
        marker.push(at);
        // The run's block may have shifted, if the placeholder went in
        // before it in a block on its path.
        let shift = |path: &mut Vec<usize>| {
            if path.len() > dest.len() && path[..dest.len()] == dest[..] && path[dest.len()] >= at {
                path[dest.len()] += 1;
            }
        };
        let mut run_block = block.clone();
        shift(&mut run_block);
        let (mut i, mut j) = (i, j);
        if run_block == dest && i >= at {
            i += 1;
            j += 1;
        }
        let run: Vec<Node> = children(&mut self.root, &run_block).drain(i..=j).collect();
        // The placeholder may have shifted back, if the run was before it in
        // a block on its path.
        let n = j - i + 1;
        if marker.len() > run_block.len()
            && marker[..run_block.len()] == run_block[..]
            && marker[run_block.len()] > j
        {
            marker[run_block.len()] -= n;
        }
        *node_at(&mut self.root, &marker) = Node::Block(run);
    }
}

#[derive(Clone, Debug)]
enum Edit {
    Insert(prop::sample::Index, char),
    Delete(prop::sample::Index),
    Move(
        prop::sample::Index,
        prop::sample::Index,
        prop::sample::Index,
    ),
}

fn edits() -> impl Strategy<Value = Vec<Edit>> {
    let edit = prop_oneof![
        4 => (any::<prop::sample::Index>(), prop::char::range('a', 'z')).prop_map(|(p, c)| Edit::Insert(p, c)),
        1 => any::<prop::sample::Index>().prop_map(Edit::Delete),
        2 => (any::<prop::sample::Index>(), any::<prop::sample::Index>(), any::<prop::sample::Index>())
            .prop_map(|(a, b, t)| Edit::Move(a, b, t)),
    ];
    prop::collection::vec(edit, 1..60)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// One writer's edits, moves among them: the reference draws what the
    /// tree model does, after every edit.
    #[test]
    fn one_writer_matches_the_tree(edits in edits()) {
        let mut log: OpLog<char> = OpLog::new();
        let me = log.agent("me");
        let mut v: Vec<Lv> = Vec::new();
        let mut tree = Tree { root: Vec::new() };
        for edit in edits {
            let len = tree.visible().len();
            match edit {
                Edit::Insert(p, c) => {
                    let pos = p.index(len + 1);
                    v = log.add_insert(me, &v, pos, [c]);
                    tree.insert(pos, c);
                }
                Edit::Delete(p) => {
                    if len == 0 {
                        continue;
                    }
                    let pos = p.index(len);
                    v = log.add_delete(me, &v, pos..pos + 1);
                    tree.delete(pos);
                }
                Edit::Move(a, b, t) => {
                    if len == 0 {
                        continue;
                    }
                    let (a, b) = (a.index(len), b.index(len));
                    let (a, b) = (a.min(b), a.max(b) + 1);
                    let to = t.index(len + 1);
                    if to > a && to < b {
                        continue;
                    }
                    v = log.add_move(me, &v, a..b, to);
                    tree.move_(a, b - a, to);
                }
            }
            let mut want = String::new();
            text(&tree.root, &mut want);
            let got: String = reference::checkout(&log, &v).into_iter().collect();
            prop_assert_eq!(got, want);
        }
    }
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

/// A range that cuts into a moved block, at the version that sees the
/// block moved: refused, so it does nothing.
#[test]
fn a_range_cutting_into_a_moved_block_does_nothing() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "AA.BB.CC.");
    let a = log.agent("a");
    let v = log.add_move(a, &base, 3..6, 0);
    assert_eq!(text_at(&log, &v), "BB.AA.CC.");
    // "B.A": half in the moved block, half out.
    let w = log.add_move(a, &v, 1..4, 9);
    assert_eq!(text_at(&log, &w), "BB.AA.CC.");
}

/// Two moves at once whose ranges cross: honouring both would tear one
/// section in two, so the later is dropped. (Sizes differ, so neither
/// can win the shared part by a tie.)
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
                    _ => continue,
                };
            }
        }
    }
    (log, versions)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// From any writer's version to any other's: the simple walk's changes
    /// make the reference's merged document, item for item.
    #[test]
    fn the_simple_walk_merges_like_the_reference(actions in actions()) {
        let (log, versions) = history(actions);
        for from in &versions {
            for to in &versions {
                let mut doc = reference::checkout(&log, from);
                let mut ids = reference::identities(&log, from);
                apply(&log.changes_simple(from, to), &mut doc, &mut ids);
                let both = log.union(from, to);
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

/// Every pair of `versions`: the simple walk's changes make the
/// reference's merged document, characters and identities.
fn merges_agree(log: &OpLog<char>, versions: &[Vec<Lv>]) {
    for from in versions {
        for to in versions {
            let mut doc = reference::checkout(log, from);
            let mut ids = reference::identities(log, from);
            apply(&log.changes_simple(from, to), &mut doc, &mut ids);
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

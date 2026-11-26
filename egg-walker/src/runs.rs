//! A merge's window on moves as copies, with its records as runs (#22).
//!
//! The same rules as `copies.rs`, which stays the reference, replayed on
//! runs of keys rather than one record per item: typed text is one
//! record, a stretch of the window's starting document is one record, and
//! a move's copies of a run are one run. So a move costs the runs it
//! covers, not the items, and each step costs the window's runs, not the
//! document's items.
//!
//! # Keys and records
//!
//! Every item has the same key it has in `copies.rs`: an insert's own
//! event, the starting document's roots, and past the log's length, a
//! copy. A move's copies are one stretch of keys, `first_copy..` as long as
//! what it covered, its sources in drawn order. Its sources are runs of
//! keys too, so copying a copy is one run, however much it holds: nesting
//! stays flat.
//!
//! A *record* is a run of keys that every rule treats alike, so asking
//! about its first key answers for all of it:
//! - laid out as one: each item after the one before, with the same right
//!   origin (typing, a copied run, a stretch of the start);
//! - taken by the same moves (a move's sources start and end records);
//! - drawn alike: shown, or not (an item someone deleted is a record of
//!   its own, in every copy);
//! - on its item's path, or not, and laid out, or not, the same way.
//!
//! Nothing else names an item inside a record: an origin ends one (on the
//! left) or starts one (on the right). Each layout splits its runs where
//! that's needed and nowhere else.
//!
//! In debug builds the merge replays each window with `copies.rs` too and
//! checks they agree (unless built with the `perf` feature).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::copies::Loss;
use crate::oplog::{Lv, Op, OpLog};
use crate::walk::tie_break;

/// An item's key: see `copies.rs`.
type Key = usize;

/// An event, numbered in the replay.
type Ix = usize;

/// A run of keys.
type Keys = Range<Key>;

/// Where an insert, or a move's copies, go: see `copies.rs`.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Spot {
    left: Option<Key>,
    right: Option<Key>,
    before: Option<Key>,
    after: Option<Key>,
}

#[derive(Clone)]
struct Move {
    /// Every item it covers, in drawn order, tombstones too, as runs.
    sources: Vec<Keys>,
    /// Where each source run's copies start, less `first_copy`.
    offsets: Vec<usize>,
    /// The source runs by first key: (key, index in `sources`).
    by_key: Vec<(Key, usize)>,
    /// The same keys as a set: sorted, apart, merged.
    set: Vec<Keys>,
    /// Of those, the items that showed.
    shown: Vec<Keys>,
    shown_len: usize,
    /// How many items it covers.
    len: usize,
    /// Its copies are `first_copy..first_copy + len`.
    first_copy: Key,
    to: Spot,
}

impl Move {
    fn new(sources: Vec<Keys>, shown: Vec<Keys>, first_copy: Key, to: Spot) -> Move {
        let mut offsets = Vec::with_capacity(sources.len());
        let mut len = 0;
        for s in &sources {
            offsets.push(len);
            len += s.len();
        }
        let mut by_key: Vec<(Key, usize)> = sources
            .iter()
            .enumerate()
            .map(|(i, s)| (s.start, i))
            .collect();
        by_key.sort_unstable();
        let set = merged(sources.clone());
        let shown_len = shown.iter().map(|s| s.len()).sum();
        Move {
            sources,
            offsets,
            by_key,
            set,
            shown,
            shown_len,
            len,
            first_copy,
            to,
        }
    }

    fn contains(&self, k: Key) -> bool {
        contains(&self.set, k)
    }

    /// The copy of `k`, if it's among the sources.
    fn copy_of(&self, k: Key) -> Option<Key> {
        let i = self
            .by_key
            .partition_point(|&(s, _)| s <= k)
            .checked_sub(1)?;
        let (s, j) = self.by_key[i];
        (k < self.sources[j].end).then(|| self.first_copy + self.offsets[j] + (k - s))
    }

    /// What copy `c` copied.
    fn source_of(&self, c: Key) -> Key {
        let off = c - self.first_copy;
        let j = self.offsets.partition_point(|&o| o <= off) - 1;
        self.sources[j].start + (off - self.offsets[j])
    }

    /// What copies `copies` copied, piece by piece: (first copy, sources).
    fn pieces(&self, copies: Keys) -> Vec<(Key, Keys)> {
        let (lo, hi) = (copies.start - self.first_copy, copies.end - self.first_copy);
        let mut out = Vec::new();
        let mut j = self.offsets.partition_point(|&o| o <= lo) - 1;
        let mut off = lo;
        while off < hi {
            let s = &self.sources[j];
            let from = off - self.offsets[j];
            let n = (s.len() - from).min(hi - off);
            out.push((self.first_copy + off, s.start + from..s.start + from + n));
            off += n;
            j += 1;
        }
        out
    }
}

/// `ranges` as a set: sorted and merged.
fn merged(mut ranges: Vec<Keys>) -> Vec<Keys> {
    ranges.sort_unstable_by_key(|r| r.start);
    let mut out: Vec<Keys> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if last.end >= r.start => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

fn contains(set: &[Keys], k: Key) -> bool {
    let i = set.partition_point(|r| r.start <= k);
    i > 0 && k < set[i - 1].end
}

fn disjoint(a: &[Keys], b: &[Keys]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].end <= b[j].start {
            i += 1;
        } else if b[j].end <= a[i].start {
            j += 1;
        } else {
            return false;
        }
    }
    true
}

/// Whether every key of `a` is in `b`.
fn subset(a: &[Keys], b: &[Keys]) -> bool {
    a.iter().all(|r| {
        let i = b.partition_point(|s| s.start <= r.start);
        i > 0 && r.end <= b[i - 1].end
    })
}

#[derive(Clone)]
enum Info {
    Insert(Spot),
    Delete { root: Lv },
    Undelete { target: Ix },
    Mark { root: Lv, level: u8 },
    Move(Move),
    Inert,
}

/// The moves that stand, in order, and which they are.
struct Active {
    list: Vec<Ix>,
    on: Vec<bool>,
    /// Where each is in `list`.
    rank: Vec<usize>,
    /// Whether any move there doesn't stand: if none, everything's laid
    /// out.
    some_off: bool,
    /// The moves dropped for loops, sorted.
    looped: Vec<Ix>,
}

impl Active {
    fn new(list: Vec<Ix>, n: usize, moves: usize) -> Active {
        let mut on = vec![false; n];
        let mut rank = vec![usize::MAX; n];
        for (i, &m) in list.iter().enumerate() {
            on[m] = true;
            rank[m] = i;
        }
        let some_off = list.len() < moves;
        Active {
            list,
            on,
            rank,
            some_off,
            looped: Vec::new(),
        }
    }
}

/// A record laid out, and what the rules say of it.
#[derive(Clone, Copy, Debug)]
struct Atom {
    end: Key,
    /// On its item's path (see `copies.rs`).
    on_path: bool,
    /// The innermost standing move that took it.
    winner: Option<Ix>,
}

/// A layout: the records in order, and each by its first key.
struct Layout {
    order: Vec<Key>,
    atoms: BTreeMap<Key, Atom>,
}

impl Layout {
    fn atom(&self, k: Key) -> (Key, &Atom) {
        let (&s, a) = self.atoms.range(..=k).next_back().expect("a laid-out key");
        debug_assert!(k < a.end, "{k} is laid out");
        (s, a)
    }
}

const NIL: usize = usize::MAX;

/// Records in order, as a linked list with order labels: where one goes
/// is the YjsMod rule (see `order.rs`), which only compares positions, so
/// placing one costs the records it passes over, not the list.
struct Linked {
    keys: Vec<Key>,
    /// Its origins, as nodes.
    left: Vec<usize>,
    right: Vec<usize>,
    prev: Vec<usize>,
    next: Vec<usize>,
    /// Increasing along the list, between 0 (before it) and `u64::MAX`
    /// (after it).
    label: Vec<u64>,
    head: usize,
    tail: usize,
    node: HashMap<Key, usize>,
    capacity: usize,
}

impl Linked {
    fn with_capacity(n: usize) -> Linked {
        Linked {
            keys: Vec::with_capacity(n),
            left: Vec::with_capacity(n),
            right: Vec::with_capacity(n),
            prev: Vec::with_capacity(n),
            next: Vec::with_capacity(n),
            label: Vec::with_capacity(n),
            head: NIL,
            tail: NIL,
            node: HashMap::with_capacity(n),
            capacity: n,
        }
    }

    fn has(&self, k: Key) -> bool {
        self.node.contains_key(&k)
    }

    fn new_node(&mut self, key: Key, left: Option<Key>, right: Option<Key>) -> usize {
        let n = self.keys.len();
        self.keys.push(key);
        self.left.push(left.map_or(NIL, |k| self.node[&k]));
        self.right.push(right.map_or(NIL, |k| self.node[&k]));
        self.prev.push(NIL);
        self.next.push(NIL);
        self.label.push(0);
        self.node.insert(key, n);
        n
    }

    fn push_back(&mut self, key: Key, left: Option<Key>, right: Option<Key>) {
        let n = self.new_node(key, left, right);
        self.link_before(n, NIL);
    }

    fn link_before(&mut self, n: usize, at: usize) {
        let p = if at == NIL { self.tail } else { self.prev[at] };
        self.prev[n] = p;
        self.next[n] = at;
        if p == NIL {
            self.head = n;
        } else {
            self.next[p] = n;
        }
        if at == NIL {
            self.tail = n;
        } else {
            self.prev[at] = n;
        }
    }

    /// Spaces the labels out evenly.
    fn relabel(&mut self) {
        let step = u64::MAX / (self.capacity.max(self.keys.len()) as u64 + 2);
        let mut n = self.head;
        let mut l = step;
        while n != NIL {
            self.label[n] = l;
            l += step;
            n = self.next[n];
        }
    }

    fn label_or(&self, n: usize, none: u64) -> u64 {
        if n == NIL { none } else { self.label[n] }
    }

    /// Puts `key` where the YjsMod rule says, for origins `left` and
    /// `right`; `cmp` orders it against another by key, to break ties.
    fn place(
        &mut self,
        key: Key,
        left: Option<Key>,
        right: Option<Key>,
        cmp: impl Fn(Key) -> std::cmp::Ordering,
    ) {
        let n = self.new_node(key, left, right);
        let (l, mut r) = (self.left[n], self.right[n]);
        let left_at = self.label_or(l, 0);
        // A right origin before the left one says nothing: see `order.rs`.
        if r != NIL && self.label[r] <= left_at && l != NIL {
            r = NIL;
        }
        let right_at = self.label_or(r, u64::MAX);
        let mut i = if l == NIL { self.head } else { self.next[l] };
        let mut dest = i;
        let mut scanning = false;
        loop {
            if !scanning {
                dest = i;
            }
            if i == NIL || i == r {
                break;
            }
            let other_left = self.label_or(self.left[i], 0);
            let other_right = match self.right[i] {
                r if r != NIL && self.left[i] != NIL && self.label[r] <= other_left => u64::MAX,
                r => self.label_or(r, u64::MAX),
            };
            if other_left < left_at {
                break;
            }
            if other_left == left_at {
                if other_right == right_at {
                    if cmp(self.keys[i]) == std::cmp::Ordering::Less {
                        break;
                    }
                    scanning = false;
                } else {
                    scanning = other_right < right_at;
                }
            }
            i = self.next[i];
        }
        self.link_before(n, dest);
        let lo = self.label_or(self.prev[n], 0);
        let hi = self.label_or(self.next[n], u64::MAX);
        if hi - lo < 2 {
            self.relabel();
        } else {
            self.label[n] = lo + (hi - lo) / 2;
        }
    }

    fn order(&self) -> Vec<Key> {
        let mut out = Vec::with_capacity(self.keys.len());
        let mut n = self.head;
        while n != NIL {
            out.push(self.keys[n]);
            n = self.next[n];
        }
        out
    }
}

/// Where each event goes, among the active moves it was worked out for.
type Redirects = (Vec<Ix>, Vec<Ix>, HashMap<Ix, (Spot, Vec<Ix>)>);

pub(crate) use crate::copies::Unsupported;

pub(crate) struct Replay<'a, T> {
    log: &'a OpLog<T>,
    /// The starting document, as runs of roots.
    /// The starting document's items: their keys, one run in order, and
    /// who each is (first key, first root, how many).
    base_keys: Keys,
    base_roots: Vec<(Key, Lv, usize)>,
    events: Vec<Lv>,
    ix: HashMap<Lv, Ix>,
    info: Vec<Info>,
    /// What each event saw, of the replayed ones, as a version vector over
    /// chains (runs of events, each after the one before): each event's
    /// chain and place in it, and how many of each chain's it saw.
    chain: Vec<(usize, usize)>,
    seen: Vec<Vec<usize>>,
    /// Each chain's events, and its moves (by place in it, and event).
    chains: Vec<Vec<Ix>>,
    chain_moves: Vec<Vec<(usize, Ix)>>,
    /// The moves concurrent with each event.
    conc: Vec<Vec<Ix>>,
    copy_base: Key,
    /// Each move's first copy, in order of keys: (key, move).
    copy_runs: Vec<(Key, Ix)>,
    /// Where records must start: where moves' sources start and end.
    breaks: BTreeSet<Key>,
    /// Every record of an item something deleted, and by root.
    tracked: BTreeSet<Key>,
    records_of: HashMap<Lv, Vec<Key>>,
    redirects: RefCell<Redirects>,
}

/// The document drawn at a version.
#[derive(Clone)]
pub(crate) struct View {
    /// Every record laid out, in order.
    order: Order,
    /// The records drawn, in order, and whether they show.
    drawn: Vec<(Keys, bool)>,
    deleted: HashSet<Lv>,
    pub(crate) losses: Vec<Loss>,
    active: Vec<Ix>,
}

/// Every record laid out, in order: runs of keys, linked by first key, so
/// finding the one holding a key, and putting more after it, cost the
/// log of how many there are.
#[derive(Clone, Default)]
struct Order {
    /// Each run by its first key: where it ends, and the next run's first.
    runs: BTreeMap<Key, (Key, Option<Key>)>,
    head: Option<Key>,
}

impl Order {
    fn from_runs(list: Vec<Keys>) -> Order {
        let mut runs = BTreeMap::new();
        for (i, r) in list.iter().enumerate() {
            runs.insert(r.start, (r.end, list.get(i + 1).map(|n| n.start)));
        }
        Order {
            runs,
            head: list.first().map(|r| r.start),
        }
    }

    /// The run holding `k`: its first key.
    fn holding(&self, k: Key) -> Key {
        let (&s, &(end, _)) = self.runs.range(..=k).next_back().expect("laid out");
        debug_assert!(k < end, "{k} is laid out");
        s
    }

    /// The key laid out after `k` (or first, after none).
    fn next(&self, k: Option<Key>) -> Option<Key> {
        let Some(k) = k else { return self.head };
        let (end, next) = self.runs[&self.holding(k)];
        if k + 1 < end { Some(k + 1) } else { next }
    }

    /// `list`, after `left` (or first, after none).
    fn insert_after(&mut self, left: Option<Key>, list: &[Keys]) {
        if list.is_empty() {
            return;
        }
        let rest = match left {
            None => self.head.replace(list[0].start),
            Some(l) => {
                let s = self.holding(l);
                let (end, next) = self.runs[&s];
                let rest = if l + 1 < end {
                    self.runs.insert(l + 1, (end, next));
                    Some(l + 1)
                } else {
                    next
                };
                self.runs.insert(s, (l + 1, Some(list[0].start)));
                rest
            }
        };
        for (i, r) in list.iter().enumerate() {
            let next = list.get(i + 1).map(|n| n.start).or(rest);
            self.runs.insert(r.start, (r.end, next));
        }
    }

    fn keys(&self) -> Vec<Key> {
        let mut out = Vec::new();
        let mut at = self.head;
        while let Some(s) = at {
            let (end, next) = self.runs[&s];
            out.extend(s..end);
            at = next;
        }
        out
    }
}

/// Pushes `r` onto `list`, joining it to the last if it carries on.
fn push_run(list: &mut Vec<Keys>, r: Keys) {
    match list.last_mut() {
        Some(last) if last.end == r.start => last.end = r.end,
        _ => list.push(r),
    }
}

fn push_drawn(list: &mut Vec<(Keys, bool)>, r: Keys, shows: bool) {
    match list.last_mut() {
        Some((last, s)) if last.end == r.start && *s == shows => last.end = r.end,
        _ => list.push((r, shows)),
    }
}

/// Makes `k` the start of a record in `list`, if it's inside one.
fn split_at<X: Copy>(list: &mut Vec<(Keys, X)>, k: Key) {
    if let Some(i) = list.iter().position(|(r, _)| r.start < k && k < r.end) {
        let (r, x) = list[i].clone();
        list[i].0.end = k;
        list.insert(i + 1, (k..r.end, x));
    }
}

impl<'a, T: Clone> Replay<'a, T> {
    fn n(&self) -> usize {
        self.info.len()
    }

    /// The move that made copy `c`.
    fn copier(&self, c: Key) -> Ix {
        let i = self.copy_runs.partition_point(|&(k, _)| k <= c) - 1;
        self.copy_runs[i].1
    }

    fn by(&self, key: Key) -> Option<Ix> {
        if key >= self.copy_base {
            Some(self.copier(key))
        } else {
            self.ix.get(&key).copied()
        }
    }

    fn made_by(&self, key: Key) -> Lv {
        match self.by(key) {
            Some(i) => self.events[i],
            None => self.real(key),
        }
    }

    /// Who item `who` (a root, here) is in the log: its insert.
    pub(crate) fn real(&self, who: Key) -> Lv {
        if !self.base_keys.contains(&who) {
            return who;
        }
        let i = self.base_roots.partition_point(|&(k, _, _)| k <= who) - 1;
        let (k, root, _) = self.base_roots[i];
        root + (who - k)
    }

    /// The window's key for insert `root`: see [`real`](Self::real).
    pub(crate) fn who(&self, root: Lv) -> Key {
        match self
            .base_roots
            .iter()
            .find(|&&(_, r, n)| (r..r + n).contains(&root))
        {
            Some(&(k, r, _)) => k + (root - r),
            None => root,
        }
    }

    /// `who`, as runs of inserts in the log.
    pub(crate) fn real_runs(&self, who: &[Keys]) -> Vec<Keys> {
        let mut out = Vec::new();
        for r in who {
            if !self.base_keys.contains(&r.start) {
                push_run(&mut out, r.clone());
                continue;
            }
            let mut i = self.base_roots.partition_point(|&(k, _, _)| k <= r.start) - 1;
            let mut k = r.start;
            while k < r.end {
                let (first, root, n) = self.base_roots[i];
                let end = (first + n).min(r.end);
                push_run(&mut out, root + (k - first)..root + (end - first));
                k = end;
                i += 1;
            }
        }
        out
    }

    /// The starting document, by who.
    pub(crate) fn start(&self) -> Vec<Keys> {
        if self.base_keys.is_empty() {
            Vec::new()
        } else {
            vec![self.base_keys.clone()]
        }
    }

    fn source(&self, c: Key) -> Key {
        self.mv(self.copier(c)).source_of(c)
    }

    fn root(&self, mut key: Key) -> Lv {
        while key >= self.copy_base {
            key = self.source(key);
        }
        key
    }

    /// The roots of `keys`, in order, as runs.
    fn roots_of(&self, keys: Keys, out: &mut Vec<Keys>) {
        let mut todo = vec![keys];
        while let Some(keys) = todo.pop() {
            if keys.start < self.copy_base {
                push_run(out, keys);
                continue;
            }
            // Copies: one move's, or several moves' side by side.
            let i = self.copy_runs.partition_point(|&(k, _)| k <= keys.start) - 1;
            let (first, m) = self.copy_runs[i];
            let end = first + self.mv(m).len;
            if keys.end > end {
                todo.push(end..keys.end);
            }
            let pieces = self.mv(m).pieces(keys.start..keys.end.min(end));
            todo.extend(pieces.into_iter().rev().map(|(_, s)| s));
        }
    }

    fn mv(&self, m: Ix) -> &Move {
        match &self.info[m] {
            Info::Move(mv) => mv,
            _ => unreachable!("{m} is a move"),
        }
    }

    /// Whether `a` saw `b`.
    fn saw(&self, a: Ix, b: Ix) -> bool {
        let (c, p) = self.chain[b];
        a != b && self.seen[a].get(c).is_some_and(|&n| p < n)
    }

    fn concurrent(&self, a: Ix, b: Ix) -> bool {
        a != b && !self.saw(a, b) && !self.saw(b, a)
    }

    fn key(&self, i: Ix) -> (u64, (&str, u64)) {
        let lv = self.events[i];
        (self.log.lamport(lv), self.log.order_key(lv))
    }

    /// `key`, or if it isn't laid out, the nearest thing it copied that is.
    fn live(&self, key: Key, act: &Active) -> Key {
        if !act.some_off || key < self.copy_base {
            return key;
        }
        // Up the copies to the insert; what's laid out is what has every
        // move from there down to it standing.
        let mut chain = vec![key];
        let mut k = key;
        while k >= self.copy_base {
            k = self.source(k);
            chain.push(k);
        }
        let mut live = k;
        for &c in chain.iter().rev().skip(1) {
            if !act.on[self.copier(c)] {
                break;
            }
            live = c;
        }
        live
    }

    fn spot(&self, x: Ix) -> Spot {
        match &self.info[x] {
            Info::Insert(spot) => *spot,
            Info::Move(mv) => mv.to,
            _ => unreachable!("{x} puts something somewhere"),
        }
    }

    fn redirect(&self, x: Ix, act: &Active) -> (Spot, Vec<Ix>) {
        {
            let mut memo = self.redirects.borrow_mut();
            if memo.0 != act.list || memo.1 != act.looped {
                *memo = (act.list.clone(), act.looped.clone(), HashMap::new());
            } else if let Some(found) = memo.2.get(&x) {
                return found.clone();
            }
        }
        let found = self.find_redirect(x, act);
        self.redirects.borrow_mut().2.insert(x, found.clone());
        found
    }

    fn find_redirect(&self, x: Ix, act: &Active) -> (Spot, Vec<Ix>) {
        let live = |k: Option<Key>| k.map(|k| self.live(k, act));
        let raw = self.spot(x);
        let mut spot = Spot {
            left: live(raw.left),
            right: live(raw.right),
            before: live(raw.before),
            after: live(raw.after),
        };
        let mut through: Vec<Ix> = Vec::new();
        loop {
            let mut candidates: Vec<Ix> = self.conc[x]
                .iter()
                .copied()
                .filter(|&m| act.on[m])
                .collect();
            candidates.sort_unstable_by_key(|&m| act.rank[m]);
            let holder = candidates
                .into_iter()
                .filter(|&m| !through.contains(&m))
                .filter(|&m| {
                    let inside = |k: Key| {
                        self.mv(m).contains(k)
                            || self.by(k).is_some_and(|b| {
                                b != x
                                    && self.concurrent(b, m)
                                    && self.redirect(b, act).1.contains(&m)
                            })
                    };
                    spot.before.is_some_and(inside) && spot.after.is_some_and(inside)
                })
                .min_by_key(|&m| self.mv(m).len);
            let Some(m) = holder else {
                if !through.is_empty() {
                    return (spot, through);
                }
                let carried = |k: Key| {
                    let by = self.by(k)?;
                    let (_, gone) = self.redirect(by, act);
                    gone.iter().any(|&g| !self.saw(x, g)).then_some(by)
                };
                // A copy carried off is passed over with the rest of its
                // move's copies, for the move's own origin: copies.rs
                // steps back one copy at a time, each carried off the same.
                // An origin that's a copy made by a move dropped for a
                // loop stands for what it copied: the copy went where the
                // other move took it, not where its author put it
                // (mamre#27). Loops only, on purpose: see copies.rs's
                // module docs before widening it to other losing moves.
                let unloop = |k: Option<Key>| {
                    k.map(|mut k| {
                        while k >= self.copy_base && act.looped.contains(&self.copier(k)) {
                            k = self.source(k);
                        }
                        k
                    })
                };
                let mut left = unloop(raw.left);
                while let Some(k) = left {
                    let Some(by) = carried(k) else { break };
                    left = self.spot(by).left;
                }
                let mut right = unloop(raw.right);
                while let Some(k) = right {
                    let Some(by) = carried(k) else { break };
                    right = self.spot(by).right;
                }
                return (Spot { left, right, ..raw }, through);
            };
            let mv = self.mv(m);
            let map = |k: Option<Key>| k.and_then(|k| mv.copy_of(k));
            let after = map(spot.after);
            spot = Spot {
                left: map(spot.left).or(spot.left),
                right: map(spot.right).or(after),
                before: map(spot.before),
                after,
            };
            through.push(m);
        }
    }

    fn active(&self, at: &[bool], dropped: &HashSet<Ix>, losses: &mut Vec<Loss>) -> Vec<Ix> {
        let mut moves: Vec<Ix> = (0..self.n())
            .filter(|&i| at[i] && !dropped.contains(&i))
            .filter(|&i| match &self.info[i] {
                Info::Move(_) => true,
                Info::Inert => {
                    losses.push(Loss {
                        op: self.events[i],
                        lost_to: None,
                        ranges: Vec::new(),
                    });
                    false
                }
                _ => false,
            })
            .collect();
        moves.sort_by_key(|&m| self.key(m));
        let mut active: Vec<Ix> = Vec::new();
        // Where each move went in, while it stands.
        let mut rank: HashMap<Ix, usize> = HashMap::new();
        let mut next_rank = 0;
        'next: for m in moves {
            let set = &self.mv(m).set;
            let mut replaced = None;
            let mut against: Vec<Ix> = self.conc[m]
                .iter()
                .copied()
                .filter(|a| rank.contains_key(a))
                .collect();
            against.sort_unstable_by_key(|a| rank[a]);
            for a in against {
                let theirs = &self.mv(a).set;
                if theirs == set {
                    replaced = Some(a);
                } else if !disjoint(theirs, set) && !subset(theirs, set) && !subset(set, theirs) {
                    losses.push(Loss {
                        op: self.events[m],
                        lost_to: Some(self.events[a]),
                        ranges: Vec::new(),
                    });
                    continue 'next;
                }
            }
            if let Some(a) = replaced {
                active.retain(|&x| x != a);
                rank.remove(&a);
                losses.push(Loss {
                    op: self.events[a],
                    lost_to: Some(self.events[m]),
                    ranges: Vec::new(),
                });
            }
            rank.insert(m, next_rank);
            next_rank += 1;
            active.push(m);
        }
        active
    }

    fn undone(&self, at: &[bool]) -> HashSet<Ix> {
        (0..self.n())
            .filter(|&u| at[u])
            .filter_map(|u| match self.info[u] {
                Info::Undelete { target } => Some(target),
                _ => None,
            })
            .collect()
    }

    fn deleted(&self, at: &[bool]) -> HashSet<Lv> {
        let undone = self.undone(at);
        (0..self.n())
            .filter(|&d| at[d] && !undone.contains(&d))
            .filter_map(|d| match self.info[d] {
                Info::Delete { root } => Some(root),
                _ => None,
            })
            .collect()
    }

    /// The document at `at`, from scratch.
    pub(crate) fn rebuild(&self, at: &[bool]) -> View {
        let mut dropped: HashMap<Ix, Ix> = HashMap::new();
        let (act, layout, mut losses) = loop {
            let mut losses = Vec::new();
            let skip: HashSet<Ix> = dropped.keys().copied().collect();
            let moves = (0..self.n())
                .filter(|&i| at[i] && matches!(self.info[i], Info::Move(_)))
                .count();
            let mut act = Active::new(self.active(at, &skip, &mut losses), self.n(), moves);
            act.looped = skip.iter().copied().collect();
            act.looped.sort_unstable();
            match self.build(at, &act) {
                Ok(layout) => break (act, layout, losses),
                Err(cycle) => {
                    let (k, &latest) = cycle
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, m)| self.key(**m))
                        .unwrap();
                    dropped.insert(latest, cycle[(k + 1) % cycle.len()]);
                }
            }
        };
        for (&m, &with) in &dropped {
            losses.push(Loss {
                op: self.events[m],
                lost_to: Some(self.events[with]),
                ranges: Vec::new(),
            });
        }
        let deleted = self.deleted(at);
        let gone: BTreeSet<Key> = deleted
            .iter()
            .flat_map(|r| self.records_of.get(r).into_iter().flatten().copied())
            .collect();
        let mut order: Vec<Keys> = Vec::new();
        let mut drawn: Vec<(Keys, bool)> = Vec::new();
        for &k in &layout.order {
            let a = &layout.atoms[&k];
            push_run(&mut order, k..a.end);
            if a.winner.is_none() && a.on_path {
                let hidden = gone.range(k..a.end).next().is_some();
                debug_assert!(
                    !hidden || a.end == k + 1,
                    "a deleted item is a record of its own"
                );
                push_drawn(&mut drawn, k..a.end, !hidden);
            }
        }
        let mut view = View {
            order: Order::from_runs(order),
            drawn,
            deleted,
            losses,
            active: act.list.clone(),
        };
        self.more_losses(at, &mut view, &act, &layout);
        view
    }

    /// Why copy `key` isn't on its path: see `copies.rs`.
    fn blocker(&self, mut key: Key, act: &Active, layout: &Layout, losses: &[Loss]) -> Option<Lv> {
        while key >= self.copy_base {
            let m = self.copier(key);
            let source = self.source(key);
            if !act.on[m] {
                return losses
                    .iter()
                    .find(|l| l.op == self.events[m])
                    .and_then(|l| l.lost_to);
            }
            match layout.atom(source).1.winner {
                Some(w) if w != m => return Some(self.events[w]),
                _ => key = source,
            }
        }
        None
    }

    /// Lays out every record in `at`: see `copies.rs`.
    fn build(&self, at: &[bool], act: &Active) -> Result<Layout, Vec<Ix>> {
        let mut spots: HashMap<Ix, Spot> = HashMap::new();
        let mut goes_with: HashMap<Ix, Vec<Ix>> = HashMap::new();
        for i in (0..self.n()).filter(|&i| at[i]) {
            if matches!(self.info[i], Info::Insert(_) | Info::Move(_)) {
                let (spot, through) = self.redirect(i, act);
                spots.insert(i, spot);
                if act.on[i] {
                    goes_with.insert(i, through);
                }
            }
        }
        for &start in &act.list {
            let mut path = vec![start];
            let mut todo = vec![(start, 0usize)];
            while let Some((m, k)) = todo.pop() {
                path.truncate(k + 1);
                path[k] = m;
                for &next in goes_with.get(&m).into_iter().flatten() {
                    if let Some(i) = path.iter().position(|&p| p == next) {
                        return Err(path[i..].to_vec());
                    }
                    todo.push((next, k + 1));
                    path.push(next);
                }
            }
        }

        // Where records start: where moves' sources do, around every item
        // deleted, and either side of every origin.
        let mut breaks = self.breaks.clone();
        for &k in &self.tracked {
            breaks.insert(k);
            breaks.insert(k + 1);
        }
        for spot in spots.values() {
            breaks.extend(spot.left.map(|k| k + 1));
            breaks.extend(spot.right);
        }
        let mut atoms: BTreeMap<Key, Atom> = BTreeMap::new();
        let plain = |end: Key| Atom {
            end,
            on_path: true,
            winner: None,
        };
        let split = |atoms: &mut BTreeMap<Key, Atom>, run: Keys, a: Atom| {
            let mut s = run.start;
            for &b in breaks.range(run.start + 1..run.end) {
                atoms.insert(s, Atom { end: b, ..a });
                s = b;
            }
            atoms.insert(s, Atom { end: run.end, ..a });
        };
        let mut base_atoms: Vec<Key> = Vec::new();
        for run in self.start().iter() {
            split(&mut atoms, run.clone(), plain(run.end));
            base_atoms.extend(atoms.range(run.clone()).map(|(&k, _)| k));
        }
        // Typing: each insert after the last, with its right, as one. (The
        // right is a guard: a record starting mid-run takes its own event's
        // origins, and one that doesn't has nothing with its left origin
        // there, so YjsMod puts it right after the one before whatever its
        // right is.)
        let mut typed: Option<(Keys, Spot)> = None;
        let mut inserts: Vec<Keys> = Vec::new();
        for i in (0..self.n()).filter(|&i| at[i] && matches!(self.info[i], Info::Insert(_))) {
            let lv = self.events[i];
            let spot = spots[&i];
            match &mut typed {
                Some((run, first))
                    if run.end == lv && spot.left == Some(lv - 1) && spot.right == first.right =>
                {
                    run.end += 1;
                }
                _ => {
                    inserts.extend(typed.take().map(|(r, _)| r));
                    typed = Some((lv..lv + 1, spot));
                }
            }
        }
        inserts.extend(typed.map(|(r, _)| r));
        for run in &inserts {
            split(&mut atoms, run.clone(), plain(run.end));
        }
        let moves: Vec<Ix> = (0..self.n())
            .filter(|&i| at[i] && matches!(self.info[i], Info::Move(_)))
            .collect();
        for &m in &moves {
            let mv = self.mv(m);
            let run = mv.first_copy..mv.first_copy + mv.len;
            split(&mut atoms, run.clone(), plain(run.end));
            // And where each source run's copies start.
            for &o in &mv.offsets[1..] {
                let k = mv.first_copy + o;
                if !atoms.contains_key(&k) {
                    let (&s, &a) = atoms.range(..k).next_back().unwrap();
                    atoms.insert(s, Atom { end: k, ..a });
                    atoms.insert(k, a);
                }
            }
        }

        // What took each record: the innermost standing move.
        for &m in &act.list {
            let len = self.mv(m).len;
            for r in &self.mv(m).set {
                for (_, a) in atoms.range_mut(r.clone()) {
                    debug_assert!(a.end <= r.end, "a move's sources are whole records");
                    if a.winner.is_none_or(|w| len < self.mv(w).len) {
                        a.winner = Some(m);
                    }
                }
            }
        }
        // Each copy, laid out and on its path as what it copied is: split
        // where that changes.
        for &m in &moves {
            let mv = self.mv(m);
            let on = act.on[m];
            let starts: Vec<Key> = atoms
                .range(mv.first_copy..mv.first_copy + mv.len)
                .map(|(&k, _)| k)
                .collect();
            for s in starts {
                let a = atoms[&s];
                let mut parts: Vec<(Key, bool)> = Vec::new();
                for (copy, source) in mv.pieces(s..a.end) {
                    let mut k = source.start;
                    while k < source.end {
                        let (_, sa) = atoms.range(..=k).next_back().expect("sources are laid out");
                        let end = sa.end.min(source.end);
                        let on_path = on && sa.on_path && sa.winner == Some(m);
                        let c = copy + (k - source.start);
                        if parts.last().is_none_or(|&(_, p)| p != on_path) {
                            parts.push((c, on_path));
                        }
                        k = end;
                    }
                }
                for (j, &(c, on_path)) in parts.iter().enumerate() {
                    let end = parts.get(j + 1).map_or(a.end, |p| p.0);
                    atoms.insert(c, Atom { end, on_path, ..a });
                }
            }
        }

        // Each record after its origins, as copies.rs places items.
        let start_of = |k: Key| -> Key {
            let (&s, a) = atoms
                .range(..=k)
                .next_back()
                .expect("an origin is laid out");
            debug_assert!(k < a.end);
            s
        };
        let origins = |s: Key| -> (Option<Key>, Option<Key>) {
            let by = self.by(s).expect("a replayed record");
            let spot = spots[&by];
            // An insert's own origins (typing on: the one before it); a
            // copy after the one before, but the first.
            let (left, right) = if s < self.copy_base || s == self.mv(by).first_copy {
                (spot.left, spot.right)
            } else {
                (Some(s - 1), spot.right)
            };
            let left = left.map(|l| {
                debug_assert_eq!(
                    atoms[&start_of(l)].end,
                    l + 1,
                    "a left origin ends a record"
                );
                start_of(l)
            });
            debug_assert!(
                right.is_none_or(|r| atoms.contains_key(&r)),
                "a right origin starts a record"
            );
            (left, right)
        };
        let mut todo: Vec<Key> = Vec::new();
        for run in &inserts {
            todo.extend(atoms.range(run.clone()).map(|(&k, _)| k));
        }
        for &m in &moves {
            let mv = self.mv(m);
            todo.extend(
                atoms
                    .range(mv.first_copy..mv.first_copy + mv.len)
                    .map(|(&k, _)| k),
            );
        }
        let mut list = Linked::with_capacity(atoms.len());
        for (i, &k) in base_atoms.iter().enumerate() {
            list.push_back(k, i.checked_sub(1).map(|p| base_atoms[p]), None);
        }
        list.relabel();
        let limit = 4 * atoms.len() + 8;
        for key in todo {
            let mut stack = vec![key];
            while let Some(&k) = stack.last() {
                if list.has(k) {
                    stack.pop();
                    continue;
                }
                let (left, right) = origins(k);
                let waiting: Vec<Key> = [left, right]
                    .into_iter()
                    .flatten()
                    .filter(|&o| !list.has(o))
                    .collect();
                if !waiting.is_empty() {
                    assert!(stack.len() <= limit, "origins in a loop");
                    stack.extend(waiting);
                    continue;
                }
                let by = self.made_by(k);
                list.place(k, left, right, |other| {
                    tie_break(self.log, by, self.made_by(other))
                });
                stack.pop();
            }
        }
        Ok(Layout {
            order: list.order(),
            atoms,
        })
    }

    /// The losses that need the drawing: see `copies.rs`.
    fn more_losses(&self, at: &[bool], view: &mut View, act: &Active, layout: &Layout) {
        let undone = self.undone(at);
        let mut deletes: HashMap<Lv, Vec<Ix>> = HashMap::new();
        for d in (0..self.n()).filter(|&d| at[d] && !undone.contains(&d)) {
            if let Info::Delete { root } = self.info[d] {
                deletes.entry(root).or_default().push(d);
            }
        }
        let on_path = |r: &Keys| -> bool {
            let mut k = r.start;
            while k < r.end {
                let (_, a) = layout.atom(k);
                if a.on_path {
                    return true;
                }
                k = a.end;
            }
            false
        };
        for &m in &act.list {
            let mv = self.mv(m);
            if !mv.sources.iter().any(on_path) {
                let lost_to = self.blocker(mv.sources[0].start, act, layout, &view.losses);
                view.losses.push(Loss {
                    op: self.events[m],
                    lost_to,
                    ranges: Vec::new(),
                });
                continue;
            }
            if mv.shown_len == 0 || mv.shown_len > deletes.len() {
                continue;
            }
            let mut roots = Vec::new();
            for r in &mv.shown {
                self.roots_of(r.clone(), &mut roots);
            }
            let mut by = Vec::new();
            let all = roots.iter().flat_map(|r| r.clone()).all(|root| {
                let killer = deletes
                    .get(&root)
                    .and_then(|ds| ds.iter().copied().filter(|&d| self.concurrent(d, m)).max());
                by.extend(killer);
                killer.is_some()
            });
            if all {
                view.losses.push(Loss {
                    op: self.events[m],
                    lost_to: by
                        .into_iter()
                        .max_by_key(|&d| self.key(d))
                        .map(|d| self.events[d]),
                    ranges: Vec::new(),
                });
            }
        }
        for (_, (winner, ks)) in self.marks(at) {
            for k in ks {
                if k != winner && !self.saw(winner, k) {
                    view.losses.push(Loss {
                        op: self.events[k],
                        lost_to: Some(self.events[winner]),
                        ranges: Vec::new(),
                    });
                }
            }
        }
        if !view.losses.is_empty() {
            let where_ = Positions::new(&view.roots(self));
            for loss in &mut view.losses {
                let mut roots: Vec<Keys> = Vec::new();
                match &self.info[self.ix[&loss.op]] {
                    Info::Move(mv) => {
                        for r in &mv.shown {
                            self.roots_of(r.clone(), &mut roots);
                        }
                    }
                    Info::Mark { root, .. } => roots.push(*root..*root + 1),
                    _ => {}
                }
                loss.ranges = where_.ranges(&roots);
            }
        }
        view.losses.sort_by_key(|l| l.op);
    }

    fn marks(&self, at: &[bool]) -> BTreeMap<Lv, (Ix, Vec<Ix>)> {
        let mut marks: BTreeMap<Lv, (Ix, Vec<Ix>)> = BTreeMap::new();
        for k in (0..self.n()).filter(|&k| at[k]) {
            if let Info::Mark { root, .. } = self.info[k] {
                let e = marks.entry(root).or_insert((k, Vec::new()));
                if self.key(k) > self.key(e.0) {
                    e.0 = k;
                }
                e.1.push(k);
            }
        }
        marks
    }

    pub(crate) fn levels(&self, at: &[bool]) -> BTreeMap<Lv, (Lv, u8)> {
        self.marks(at)
            .into_iter()
            .map(|(root, (winner, _))| match self.info[winner] {
                Info::Mark { level, .. } => (root, (self.events[winner], level)),
                _ => unreachable!("a mark"),
            })
            .collect()
    }

    /// The document after event `x`, from the document at its own
    /// version, flatly.
    fn after(&self, mut v: View, x: Ix) -> View {
        v.losses.clear();
        // New records, after `left`.
        let place_after = |v: &mut View, left: Option<Key>, keys: Vec<(Keys, bool)>| {
            let runs: Vec<Keys> = keys.iter().map(|(r, _)| r.clone()).collect();
            v.order.insert_after(left, &runs);
            let d = match left {
                None => 0,
                Some(l) => {
                    split_at(&mut v.drawn, l + 1);
                    v.drawn
                        .iter()
                        .position(|(r, _)| r.contains(&l))
                        .expect("drawn")
                        + 1
                }
            };
            v.drawn.splice(d..d, keys);
        };
        let show = |v: &mut View, root: Lv, shows: bool| {
            for &k in self.records_of.get(&root).into_iter().flatten() {
                split_at(&mut v.drawn, k);
                split_at(&mut v.drawn, k + 1);
                if let Some(d) = v
                    .drawn
                    .iter_mut()
                    .find(|(r, _)| r.start == k && r.end == k + 1)
                {
                    d.1 = shows;
                }
            }
        };
        match &self.info[x] {
            Info::Insert(spot) => {
                let lv = self.events[x];
                place_after(&mut v, spot.left, vec![(lv..lv + 1, true)]);
            }
            Info::Delete { root } => {
                v.deleted.insert(*root);
                show(&mut v, *root, false);
            }
            Info::Undelete { target } => {
                let Info::Delete { root } = self.info[*target] else {
                    unreachable!("an undelete undoes a delete")
                };
                let mut at: Vec<bool> = (0..self.n()).map(|j| j < x && self.saw(x, j)).collect();
                at[x] = true;
                if !self.deleted(&at).contains(&root) {
                    v.deleted.remove(&root);
                    show(&mut v, root, true);
                }
            }
            Info::Mark { .. } | Info::Inert => {}
            Info::Move(mv) => {
                v.active.push(x);
                for r in &mv.sources {
                    split_at(&mut v.drawn, r.start);
                    split_at(&mut v.drawn, r.end);
                }
                let mut copies: Vec<(Keys, bool)> = Vec::new();
                for (j, r) in mv.sources.iter().enumerate() {
                    let c = mv.first_copy + mv.offsets[j];
                    let mut k = r.start;
                    while k < r.end {
                        let (d, shows) = v
                            .drawn
                            .iter()
                            .find(|(d, _)| d.contains(&k))
                            .cloned()
                            .expect("a move's sources are drawn");
                        let end = d.end.min(r.end);
                        push_drawn(&mut copies, c + (k - r.start)..c + (end - r.start), shows);
                        k = end;
                    }
                }
                v.drawn.retain(|(d, _)| !mv.contains(d.start));
                place_after(&mut v, mv.to.left, copies);
            }
        }
        v
    }
}

/// Where items are in a document: its roots as runs, by root.
pub(crate) struct Positions {
    /// (first root, how many, position), by root.
    runs: Vec<(Lv, usize, usize)>,
}

impl Positions {
    pub(crate) fn new(roots: &[Keys]) -> Positions {
        let mut runs = Vec::with_capacity(roots.len());
        let mut pos = 0;
        for r in roots {
            runs.push((r.start, r.len(), pos));
            pos += r.len();
        }
        runs.sort_unstable();
        Positions { runs }
    }

    pub(crate) fn get(&self, root: Lv) -> Option<usize> {
        let i = self
            .runs
            .partition_point(|&(s, _, _)| s <= root)
            .checked_sub(1)?;
        let (s, n, p) = self.runs[i];
        (root < s + n).then_some(p + (root - s))
    }

    /// Where `roots` are, as ranges of positions in order.
    fn ranges(&self, roots: &[Keys]) -> Vec<Range<usize>> {
        let mut at: Vec<Range<usize>> = Vec::new();
        for r in roots {
            let mut k = r.start;
            while k < r.end {
                let i = self.runs.partition_point(|&(s, _, _)| s <= k);
                match i.checked_sub(1).map(|i| self.runs[i]) {
                    Some((s, n, p)) if k < s + n => {
                        let end = (s + n).min(r.end);
                        at.push(p + (k - s)..p + (end - s));
                        k = end;
                    }
                    _ => {
                        // Not in the document: on to the next that is.
                        k = self.runs.get(i).map_or(r.end, |&(s, _, _)| s.min(r.end));
                    }
                }
            }
        }
        at.sort_unstable_by_key(|r| r.start);
        let mut out: Vec<Range<usize>> = Vec::new();
        for r in at {
            match out.last_mut() {
                Some(last) if last.end == r.start => last.end = r.end,
                _ => out.push(r),
            }
        }
        out
    }
}

impl View {
    /// The items that show, by root, in order, as runs.
    pub(crate) fn roots<T: Clone>(&self, r: &Replay<'_, T>) -> Vec<Keys> {
        let mut out = Vec::new();
        for (keys, shows) in &self.drawn {
            if *shows {
                r.roots_of(keys.clone(), &mut out);
            }
        }
        out
    }
}

/// A replay of `events` from the document whose items are `base`, as
/// runs of roots: see `copies::replay`.
pub(crate) fn replay<'a, T: Clone>(
    log: &'a OpLog<T>,
    base: Vec<Keys>,
    events: Vec<Lv>,
    flat: bool,
) -> Result<Replay<'a, T>, Unsupported> {
    replay_checked(log, base, events, flat, false)
}

fn replay_checked<'a, T: Clone>(
    log: &'a OpLog<T>,
    roots: Vec<Keys>,
    events: Vec<Lv>,
    flat: bool,
    check: bool,
) -> Result<Replay<'a, T>, Unsupported> {
    let n = events.len();
    let ix: HashMap<Lv, Ix> = events.iter().enumerate().map(|(i, &lv)| (lv, i)).collect();
    // The start's items get keys of their own, side by side: one run.
    let first = log.len() + 1;
    let mut base_roots = Vec::with_capacity(roots.len());
    let mut k = first;
    for r in &roots {
        base_roots.push((k, r.start, r.len()));
        k += r.len();
    }
    let mut r = Replay {
        log,
        base_keys: first..k,
        base_roots,
        events,
        ix,
        info: Vec::with_capacity(n),
        chain: Vec::with_capacity(n),
        seen: Vec::with_capacity(n),
        chains: Vec::new(),
        chain_moves: Vec::new(),
        conc: Vec::with_capacity(n),
        // One key apart, so no run of keys crosses over.
        copy_base: k + 1,
        copy_runs: Vec::new(),
        breaks: BTreeSet::new(),
        tracked: BTreeSet::new(),
        records_of: HashMap::new(),
        redirects: Default::default(),
    };
    let mut next_copy = r.copy_base;
    let mut after: HashMap<Ix, View> = HashMap::new();
    let mut uses: Vec<usize> = vec![0; n];
    let parents_of = |lv: Lv, ix: &HashMap<Lv, Ix>| -> Vec<Ix> {
        let mut p: Vec<Ix> = log
            .event(lv)
            .parents
            .iter()
            .filter_map(|p| ix.get(p).copied())
            .collect();
        p.sort_unstable();
        p
    };
    if flat {
        for i in 0..n {
            if let [p] = parents_of(r.events[i], &r.ix)[..] {
                uses[p] += 1;
            }
        }
    }
    let empty = View {
        order: Order::from_runs(r.start()),
        drawn: r.start().into_iter().map(|k| (k, true)).collect(),
        deleted: HashSet::new(),
        losses: Vec::new(),
        active: Vec::new(),
    };
    for i in 0..n {
        let lv = r.events[i];
        let event = log.event(lv);
        let parents = parents_of(lv, &r.ix);
        // Its chain: its parent's, if that's the chain's last.
        let (c, place) = match parents
            .iter()
            .find(|&&p| r.chains[r.chain[p].0].last() == Some(&p))
        {
            Some(&p) => (r.chain[p].0, r.chain[p].1 + 1),
            None => {
                r.chains.push(Vec::new());
                r.chain_moves.push(Vec::new());
                (r.chains.len() - 1, 0)
            }
        };
        let mut seen = vec![0; r.chains.len()];
        for &p in &parents {
            for (s, &x) in seen.iter_mut().zip(&r.seen[p]) {
                *s = (*s).max(x);
            }
            let (pc, pp) = r.chain[p];
            seen[pc] = seen[pc].max(pp + 1);
        }
        r.chain.push((c, place));
        r.chains[c].push(i);
        r.seen.push(seen);
        let needs_at = !(flat && parents.len() == 1) || check;
        let at: Vec<bool> = if needs_at {
            (0..n).map(|j| j < i && r.saw(i, j)).collect()
        } else {
            Vec::new()
        };
        let view = match (&parents[..], flat) {
            ([], _) => empty.clone(),
            ([p], true) => {
                uses[*p] -= 1;
                if uses[*p] == 0 {
                    after.remove(p).expect("kept for it")
                } else {
                    after[p].clone()
                }
            }
            _ => r.rebuild(&at),
        };
        if check && flat && parents.len() == 1 {
            let from_scratch = r.rebuild(&at);
            assert_eq!(
                view.order.keys(),
                from_scratch.order.keys(),
                "laid out flatly at {lv}"
            );
            assert_eq!(
                drawn(&view.drawn),
                drawn(&from_scratch.drawn),
                "drawn flatly at {lv}"
            );
        }
        // The drawn records, and how many items show before each.
        let mut shown_before: Vec<usize> = Vec::with_capacity(view.drawn.len());
        let mut total = 0;
        for (d, shows) in &view.drawn {
            shown_before.push(total);
            if *shows {
                total += d.len();
            }
        }
        // The item showing at `pos`: its record, and its key.
        let item = |pos: usize| -> Option<(usize, Key)> {
            if pos >= total {
                return None;
            }
            let i = shown_before.partition_point(|&b| b <= pos) - 1;
            debug_assert!(view.drawn[i].1);
            Some((i, view.drawn[i].0.start + (pos - shown_before[i])))
        };
        let spot_at = |pos: usize| -> Spot {
            assert!(pos <= total, "position {pos} is past the end");
            let before = pos.checked_sub(1).and_then(item).map(|(_, k)| k);
            // The next record as laid out, whatever it is.
            let right = view.order.next(before);
            Spot {
                left: before,
                right,
                before,
                after: item(pos).map(|(_, k)| k),
            }
        };
        let item_at = |pos: usize| -> Key {
            item(pos)
                .expect("an event names an item its version shows")
                .1
        };
        let info = match &event.op {
            Op::Insert { pos, .. } => Info::Insert(spot_at(*pos)),
            Op::Delete { pos } => {
                let root = r.root(item_at(*pos));
                r.track(root);
                Info::Delete { root }
            }
            &Op::Mark { pos, level } => Info::Mark {
                root: r.root(item_at(pos)),
                level,
            },
            Op::Undelete { target } => match r.ix.get(target) {
                Some(&t) => {
                    assert!(r.saw(i, t), "undelete {lv} undoes a delete it includes");
                    Info::Undelete { target: t }
                }
                None => return Err(Unsupported),
            },
            &Op::Move { pos, len, to } => {
                if len == 0 || (pos..=pos + len).contains(&to) || pos + len > total {
                    Info::Inert
                } else {
                    let (first, fk) = item(pos).unwrap();
                    let (last, lk) = item(pos + len - 1).unwrap();
                    let mut sources: Vec<Keys> = Vec::new();
                    let mut shown: Vec<Keys> = Vec::new();
                    for d in first..=last {
                        let (keys, shows) = &view.drawn[d];
                        let from = if d == first { fk } else { keys.start };
                        let to = if d == last { lk + 1 } else { keys.end };
                        sources.push(from..to);
                        if *shows {
                            shown.push(from..to);
                        }
                    }
                    for s in &sources {
                        r.breaks.insert(s.start);
                        r.breaks.insert(s.end);
                    }
                    let mv = Move::new(sources, shown, next_copy, spot_at(to));
                    next_copy += mv.len;
                    r.copy_runs.push((mv.first_copy, i));
                    // Copies of items deleted somewhere are tracked too.
                    let mut more = Vec::new();
                    for s in &mv.sources {
                        for &k in r.tracked.range(s.clone()) {
                            more.push((r.root(k), mv.copy_of(k).unwrap()));
                        }
                    }
                    for (root, c) in more {
                        r.tracked.insert(c);
                        r.records_of.entry(root).or_default().push(c);
                    }
                    Info::Move(mv)
                }
            }
        };
        let is_move = matches!(info, Info::Move(_));
        r.info.push(info);
        // What it's concurrent with, of the moves before it, and if it's a
        // move, of everything before it: what it didn't see of each chain.
        r.conc.push(Vec::new());
        for ch in 0..r.chains.len() {
            let from = r.seen[i].get(ch).copied().unwrap_or(0);
            if is_move {
                for k in from..r.chains[ch].len() {
                    let j = r.chains[ch][k];
                    if j >= i {
                        break;
                    }
                    if matches!(r.info[j], Info::Move(_)) {
                        r.conc[i].push(j);
                    }
                    r.conc[j].push(i);
                }
            } else {
                let moves = &r.chain_moves[ch];
                let start = moves.partition_point(|&(p, _)| p < from);
                for &(_, j) in &moves[start..] {
                    if j >= i {
                        break;
                    }
                    r.conc[i].push(j);
                }
            }
        }
        if is_move {
            r.chain_moves[c].push((place, i));
        }
        if flat && uses[i] > 0 {
            let next = r.after(view, i);
            after.insert(i, next);
        }
    }
    Ok(r)
}

fn keys(list: &[Keys]) -> Vec<Key> {
    list.iter().flat_map(|r| r.clone()).collect()
}

fn drawn(list: &[(Keys, bool)]) -> Vec<(Key, bool)> {
    list.iter()
        .flat_map(|(r, s)| r.clone().map(move |k| (k, *s)))
        .collect()
}

impl<T: Clone> Replay<'_, T> {
    /// Tracks every record of `root`: it's deleted somewhere.
    fn track(&mut self, root: Lv) {
        if self.records_of.contains_key(&root) {
            return;
        }
        let mut all = vec![root];
        let mut i = 0;
        while i < all.len() {
            let k = all[i];
            for info in &self.info {
                if let Info::Move(mv) = info
                    && let Some(c) = mv.copy_of(k)
                {
                    all.push(c);
                }
            }
            i += 1;
        }
        self.tracked.extend(all.iter().copied());
        self.records_of.insert(root, all);
    }

    pub(crate) fn all(&self) -> Vec<bool> {
        vec![true; self.n()]
    }

    pub(crate) fn deletes_of(&self, at: &[bool]) -> HashMap<Lv, Lv> {
        let deleted = self.deleted(at);
        let mut out = HashMap::new();
        for d in (0..self.n()).filter(|&d| at[d]) {
            if let Info::Delete { root } = self.info[d]
                && deleted.contains(&root)
            {
                out.insert(root, self.events[d]);
            }
        }
        out
    }
}

/// The items at `version`, by root, replayed from `start` with runs,
/// flatly, each flat step checked against a rebuild: for testing against
/// the reference.
#[doc(hidden)]
pub fn from_start<T: Clone>(log: &OpLog<T>, start: &[Lv], version: &[Lv]) -> Option<Vec<Lv>> {
    let mut base: Vec<Keys> = Vec::new();
    for root in crate::copies::identities(log, start) {
        push_run(&mut base, root..root + 1);
    }
    let events: Vec<Lv> = (0..log.len())
        .filter(|&lv| log.contains(version, lv) && !log.contains(start, lv))
        .collect();
    let r = replay_checked(log, base, events, true, true).ok()?;
    let view = r.rebuild(&r.all());
    Some(keys(&r.real_runs(&view.roots(&r))))
}

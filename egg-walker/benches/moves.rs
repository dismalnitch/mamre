//! What moves cost the walk: a synthetic book, typed with sections moved
//! now and then, timed three ways, each against the same history with no
//! moves.
//!
//! 1. Checking it out: all of it in order, nothing concurrent.
//! 2. A live merge: two writers each type a few characters at once,
//!    merging the other's, as a replica does: keeping its identities
//!    (`changes_with`), or keeping all its books, its own edits recorded
//!    one by one (an `Accountant`, as `examples/peer.rs` does).
//! 3. An offline merge: each side edits a lot before they meet.
//!
//! `cargo bench -p mamre-egg-walker --bench moves`. `SIZES=10000,100000`
//! picks the book sizes, in characters; `OFFLINE=` the edits each side
//! makes apart.

use std::time::{Duration, Instant};

use mamre_egg_walker::{Accountant, Identities, Lv, OpLog};

/// A small deterministic random source, so every run builds the same book.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

/// A book of `chars` characters typed by one writer, in paragraphs of 80
/// at places through it, with `moves` sections (of a few paragraphs)
/// moved along the way. Returns the log, its version, and the writer.
fn book(chars: usize, moves: usize) -> (OpLog<char>, Vec<Lv>) {
    let mut log = OpLog::new();
    let me = log.agent("author");
    let mut rng = Rng(7);
    let mut v = Vec::new();
    let mut len = 0;
    let paragraph: String = "a".repeat(79) + "\n";
    let paragraphs = chars / 80;
    let every = paragraphs.checked_div(moves).map_or(usize::MAX, |n| n.max(1));
    for p in 0..paragraphs {
        // Somewhere at a paragraph's start.
        let at = 80 * rng.next(len / 80 + 1);
        v = log.add_insert(me, &v, at, paragraph.chars());
        len += 80;
        if p % every == every - 1 && len >= 800 {
            // A section of a few paragraphs, somewhere else.
            let n = 80 * (1 + rng.next(4));
            let from = 80 * rng.next((len - n) / 80 + 1);
            let mut to = 80 * rng.next(len / 80 + 1);
            if to > from && to < from + n {
                to = from;
            }
            if to != from && to != from + n {
                v = log.add_move(me, &v, from..from + n, to);
            }
        }
    }
    (log, v)
}

/// The fastest of `runs` runs of `f`.
fn best(runs: usize, mut f: impl FnMut()) -> Duration {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .unwrap()
}

fn show(d: Duration) -> String {
    if d.as_secs_f64() >= 1.0 {
        format!("{:.2} s", d.as_secs_f64())
    } else if d.as_secs_f64() >= 1e-3 {
        format!("{:.2} ms", d.as_secs_f64() * 1e3)
    } else {
        format!("{:.1} µs", d.as_secs_f64() * 1e6)
    }
}

/// Two writers, each making `edits` edits (typing a character at a place
/// through the book) apart from the other: the history, and each side's
/// version, and writer a's after each of its edits.
fn apart(
    mut log: OpLog<char>,
    v: &[Lv],
    edits: usize,
) -> (OpLog<char>, Vec<Lv>, Vec<Lv>, Vec<Vec<Lv>>) {
    let (a, b) = (log.agent("a"), log.agent("b"));
    let len = log.checkout(v).len();
    let mut rng = Rng(11);
    let (mut va, mut vb) = (v.to_vec(), v.to_vec());
    let mut steps = Vec::new();
    for k in 0..edits {
        let at = rng.next(len + k + 1);
        va = log.add_insert(a, &va, at, ['x']);
        steps.push(va.clone());
        let at = rng.next(len + k + 1);
        vb = log.add_insert(b, &vb, at, ['y']);
    }
    (log, va, vb, steps)
}

/// Writer a merging b's edits, as a replica does: keeping its identities
/// (and so its blocks) at its own version first.
fn merge(log: &OpLog<char>, va: &[Lv], vb: &[Lv]) -> Duration {
    let mut ids = Identities::new();
    log.changes_with(&[], va, &mut ids);
    best(3, || {
        let mut kept = ids.clone();
        std::hint::black_box(log.changes_with(va, vb, &mut kept));
    })
}

/// Writer a merging b's edits through its accountant: at the book first,
/// then each of its own edits recorded as it made them.
fn merge_books(log: &OpLog<char>, v: &[Lv], steps: &[Vec<Lv>], vb: &[Lv]) -> Duration {
    let mut books = Accountant::new();
    books.merge(log, v);
    for step in steps {
        books.record(log, step);
    }
    best(3, || {
        let mut kept = books.clone();
        std::hint::black_box(kept.merge(log, vb));
    })
}

fn main() {
    let sizes: Vec<usize> = std::env::var("SIZES")
        .ok()
        .map(|s| s.split(',').map(|n| n.trim().parse().unwrap()).collect())
        .unwrap_or_else(|| vec![10_000, 100_000]);
    let offline: usize = std::env::var("OFFLINE")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(200);
    println!(
        "{:>8} {:>6}  {:>12} {:>14} {:>14} {:>16}",
        "chars", "moves", "checkout", "live merge", "live, books", "offline merge"
    );
    for &chars in &sizes {
        for moves in [0, 100] {
            let (log, v) = book(chars, moves);
            let checkout = best(3, || {
                std::hint::black_box(log.checkout(&v));
            });
            let (live_log, la, lb, steps) = apart(log.clone(), &v, 10);
            let live = merge(&live_log, &la, &lb);
            let books = merge_books(&live_log, &v, &steps, &lb);
            let (off_log, oa, ob, _) = apart(log, &v, offline);
            let off = merge(&off_log, &oa, &ob);
            println!(
                "{:>8} {:>6}  {:>12} {:>14} {:>14} {:>16}",
                chars,
                moves,
                show(checkout),
                show(live),
                show(books),
                show(off)
            );
        }
    }
}

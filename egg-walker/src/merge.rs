//! A merge's window, for a log with moves: the stretch of a replay where
//! something is concurrent. It starts from a version every event in it
//! saw, whose document is known by identity; replays its events with the
//! copies' rules on runs (see `runs.rs`); and hands out the changes that
//! take the document from what it holds (the start, and the replayed
//! events it already has) to all of it, found by comparing the two by
//! identity, run by run.
//!
//! In debug builds each window is replayed by `copies.rs` too, item by
//! item, and the two must agree (unless built with the `perf` feature).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::ids::Ids;
use crate::oplog::{Lv, OpLog};
use crate::runs::{self, Positions};
use crate::walk::Changes;

pub(crate) struct Window {
    /// The document at the start, as runs of roots.
    base: Vec<Range<Lv>>,
    /// The window's events, as stepped, and whether the document needs
    /// each (or already has it).
    events: Vec<(Lv, bool)>,
}

impl Window {
    /// A window from a version whose document's identities are `ids`.
    pub(crate) fn new(ids: &Ids) -> Self {
        Window {
            base: ids.runs().collect(),
            events: Vec::new(),
        }
    }

    pub(crate) fn step(&mut self, lv: Lv, output: bool) {
        self.events.push((lv, output));
    }

    /// Hands out the window's changes.
    pub(crate) fn close<'a, T: Clone>(self, log: &'a OpLog<T>, out: &mut Changes<'a, T>) {
        let mut events: Vec<Lv> = self.events.iter().map(|&(lv, _)| lv).collect();
        events.sort_unstable();
        let had: HashSet<Lv> = self
            .events
            .iter()
            .filter(|(_, output)| !output)
            .map(|&(lv, _)| lv)
            .collect();
        let r = runs::replay(log, self.base.clone(), events.clone(), true)
            .expect("every delete a window's undo needs is in the window");
        let now: Vec<bool> = events.iter().map(|lv| had.contains(lv)).collect();
        let all = r.all();
        // Who's where, by the window's keys for the start: its items are
        // one run, so comparing costs the window, not the document.
        let (cur, cur_view) = if had.is_empty() {
            (r.start(), None)
        } else {
            let v = r.rebuild(&now);
            (v.roots(&r), Some(v))
        };
        let view = r.rebuild(&all);
        let want = view.roots(&r);
        if let Some(ids) = &out.ids {
            debug_assert_eq!(
                (0..ids.len()).map(|p| ids.get(p)).collect::<Vec<_>>(),
                expand(&r.real_runs(&cur)),
                "the document is what the window starts from"
            );
        }
        #[cfg(all(debug_assertions, not(feature = "perf")))]
        check(log, &self.base, &events, &had, &cur, &want, &r, &view);
        let deletes = r.deletes_of(&all);
        let learned = out.learned.len();
        diff(out, log, &r, cur, &want, &deletes);
        debug_assert!(
            out.learned[learned..]
                .iter()
                .all(|&(d, root)| root < log.len() && deletes.get(&r.who(root)) == Some(&d)),
            "what deletes removed is named by insert"
        );

        // Marks whose latest changed, and what's newly lost.
        let position = Positions::new(&want);
        let before = if had.is_empty() {
            Default::default()
        } else {
            r.levels(&now)
        };
        for (root, (winner, level)) in r.levels(&all) {
            if before.get(&root) != Some(&(winner, level)) {
                out.mark(r.real(root), position.get(root), level);
            }
        }
        let lost_before: HashSet<Lv> = cur_view
            .iter()
            .flat_map(|v| v.losses.iter().map(|l| l.op))
            .collect();
        for loss in view.losses {
            if !lost_before.contains(&loss.op) {
                out.lost(loss.op, loss.lost_to, loss.ranges);
            }
        }
    }
}

fn expand(runs: &[Range<Lv>]) -> Vec<Lv> {
    runs.iter().flat_map(|r| r.clone()).collect()
}

/// The window replayed item by item, by the reference's rules, agrees.
#[cfg(all(debug_assertions, not(feature = "perf")))]
#[allow(clippy::too_many_arguments)]
fn check<T: Clone>(
    log: &OpLog<T>,
    base: &[Range<Lv>],
    events: &[Lv],
    had: &HashSet<Lv>,
    cur: &[Range<Lv>],
    want: &[Range<Lv>],
    r: &runs::Replay<'_, T>,
    view: &runs::View,
) {
    let c =
        crate::copies::replay(log, expand(base), events.to_vec(), true).expect("the same window");
    let all = c.all();
    let now: Vec<bool> = events.iter().map(|lv| had.contains(lv)).collect();
    if !had.is_empty() {
        assert_eq!(
            c.rebuild(&now).roots(&c),
            expand(&r.real_runs(cur)),
            "the runs agree on where it starts"
        );
    }
    let theirs = c.rebuild(&all);
    assert_eq!(
        theirs.roots(&c),
        expand(&r.real_runs(want)),
        "the runs agree with copies.rs"
    );
    assert_eq!(
        theirs.losses, view.losses,
        "the runs lose what copies.rs loses"
    );
    let levels: std::collections::BTreeMap<Lv, (Lv, u8)> = r
        .levels(&r.all())
        .into_iter()
        .map(|(k, v)| (r.real(k), v))
        .collect();
    assert_eq!(c.levels(&all), levels, "the same marks");
    let deletes: HashMap<Lv, Lv> = r
        .deletes_of(&r.all())
        .into_iter()
        .map(|(k, d)| (r.real(k), d))
        .collect();
    assert_eq!(c.deletes_of(&all), deletes, "the same deletes");
}

/// The changes taking a document whose items are `cur` (by root, as runs)
/// to one whose items are `want`: deletes, then moves, then inserts.
/// `deletes` says which delete took each item that goes.
fn diff<'a, T: Clone>(
    out: &mut Changes<'a, T>,
    log: &'a OpLog<T>,
    r: &runs::Replay<'a, T>,
    cur: Vec<Range<Lv>>,
    want: &[Range<Lv>],
    deletes: &HashMap<Lv, Lv>,
) {
    // Pieces both sides cut alike: each wholly in the other, or not at all.
    let cuts: BTreeSet<Lv> = cur
        .iter()
        .chain(want)
        .flat_map(|r| [r.start, r.end])
        .collect();
    let pieces = |runs: &[Range<Lv>]| -> Vec<Range<Lv>> {
        let mut out = Vec::new();
        for r in runs {
            let mut s = r.start;
            for &c in cuts.range(r.start + 1..r.end) {
                out.push(s..c);
                s = c;
            }
            out.push(s..r.end);
        }
        out
    };
    let cur = pieces(&cur);
    let want = pieces(want);
    let wanted: HashSet<Lv> = want.iter().map(|r| r.start).collect();
    // What goes: deleting forward.
    let mut doc: Vec<Range<Lv>> = Vec::with_capacity(cur.len());
    let mut pos = 0;
    for piece in cur {
        if wanted.contains(&piece.start) {
            pos += piece.len();
            doc.push(piece);
        } else {
            for root in piece.clone() {
                if let Some(&d) = deletes.get(&root) {
                    out.learned.push((d, r.real(root)));
                }
            }
            out.delete_run(pos, piece.len());
        }
    }
    // What moves: the pieces both have, into the order wanted.
    let kept: HashSet<Lv> = doc.iter().map(|r| r.start).collect();
    let order: Vec<Range<Lv>> = want
        .iter()
        .filter(|r| kept.contains(&r.start))
        .cloned()
        .collect();
    if doc != order {
        let mut i = 0;
        let mut at = 0;
        while i < order.len() {
            if doc[i] == order[i] {
                at += doc[i].len();
                i += 1;
                continue;
            }
            let j = i + doc[i..]
                .iter()
                .position(|r| *r == order[i])
                .expect("the same pieces");
            let mut n = 1;
            while j + n < doc.len() && i + n < order.len() && doc[j + n] == order[i + n] {
                n += 1;
            }
            let from = at + doc[i..j].iter().map(|r| r.len()).sum::<usize>();
            let len: usize = doc[j..j + n].iter().map(|r| r.len()).sum();
            out.move_range(from..from + len, at);
            let moved: Vec<Range<Lv>> = doc.drain(j..j + n).collect();
            doc.splice(i..i, moved);
            at += len;
            i += n;
        }
    }
    // What comes: new items, and ones an undo brings back, a log run at a
    // time.
    let mut i = 0;
    let mut at = 0;
    while i < want.len() {
        if doc.get(i) != Some(&want[i]) {
            let mut p = at;
            for run in r.real_runs(std::slice::from_ref(&want[i])) {
                let mut lv = run.start;
                while lv < run.end {
                    let n = (log.run_of(lv).end() - lv).min(run.end - lv);
                    out.insert_run(p, lv, n);
                    p += n;
                    lv += n;
                }
            }
            doc.insert(i, want[i].clone());
        }
        at += want[i].len();
        i += 1;
    }
    debug_assert_eq!(doc, want);
}

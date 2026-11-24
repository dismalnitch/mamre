//! A merge's window, for a log with moves: the stretch of a replay where
//! something is concurrent. It starts from a version every event in it
//! saw, whose document is known by identity; replays its events with the
//! copies' rules (see `copies.rs`); and hands out the changes that take
//! the document from what it holds (the start, and the replayed events it
//! already has) to all of it, found by comparing the two by identity.

use std::collections::{HashMap, HashSet};

use crate::copies;
use crate::ids::Ids;
use crate::oplog::{Lv, OpLog};
use crate::walk::Changes;

pub(crate) struct Window {
    /// The document at the start, by root.
    base: Vec<Lv>,
    /// The window's events, as stepped, and whether the document needs
    /// each (or already has it).
    events: Vec<(Lv, bool)>,
}

impl Window {
    /// A window from a version whose document's identities are `ids`.
    pub(crate) fn new(ids: &Ids) -> Self {
        Window {
            base: (0..ids.len()).map(|p| ids.get(p)).collect(),
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
        let r = copies::replay(log, self.base.clone(), events.clone(), true)
            .expect("every delete a window's undo needs is in the window");
        let now: Vec<bool> = events.iter().map(|lv| had.contains(lv)).collect();
        let all = vec![true; events.len()];
        let (cur, cur_view) = if had.is_empty() {
            (self.base.clone(), None)
        } else {
            let v = r.rebuild(&now);
            (v.roots(&r), Some(v))
        };
        let view = r.rebuild(&all);
        let want = view.roots(&r);
        if let Some(ids) = &out.ids {
            debug_assert_eq!(
                (0..ids.len()).map(|p| ids.get(p)).collect::<Vec<_>>(),
                cur,
                "the document is what the window starts from"
            );
        }
        let deletes = r.deletes_of(&all);
        diff(out, cur, &want, &deletes);

        // Marks whose latest changed, and what's newly lost.
        let position: HashMap<Lv, usize> = want.iter().enumerate().map(|(i, &r)| (r, i)).collect();
        let before = if had.is_empty() {
            Default::default()
        } else {
            r.levels(&now)
        };
        for (root, (winner, level)) in r.levels(&all) {
            if before.get(&root) != Some(&(winner, level)) {
                out.mark(root, position.get(&root).copied(), level);
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

/// The changes taking a document whose items are `cur` (by root) to one
/// whose items are `want`: deletes, then moves, then inserts. `deletes`
/// says which delete took each item that goes.
fn diff<T: Clone>(out: &mut Changes<'_, T>, cur: Vec<Lv>, want: &[Lv], deletes: &HashMap<Lv, Lv>) {
    let wanted: HashSet<Lv> = want.iter().copied().collect();
    let mut doc: Vec<Lv> = Vec::with_capacity(cur.len());
    // What goes: deleting forward, so each run is one change.
    let mut pos = 0;
    for root in cur {
        if wanted.contains(&root) {
            doc.push(root);
            pos += 1;
        } else {
            if let Some(&d) = deletes.get(&root) {
                out.learned.push((d, root));
            }
            out.delete(pos);
        }
    }
    // What moves: the items both have, into the order wanted.
    let kept: HashSet<Lv> = doc.iter().copied().collect();
    let order: Vec<Lv> = want.iter().copied().filter(|r| kept.contains(r)).collect();
    if doc != order {
        let mut i = 0;
        while i < order.len() {
            if doc[i] == order[i] {
                i += 1;
                continue;
            }
            let j = i + doc[i..]
                .iter()
                .position(|&r| r == order[i])
                .expect("the same items");
            let mut n = 1;
            while j + n < doc.len() && i + n < order.len() && doc[j + n] == order[i + n] {
                n += 1;
            }
            out.move_range(j..j + n, i);
            let moved: Vec<Lv> = doc.drain(j..j + n).collect();
            doc.splice(i..i, moved);
            i += n;
        }
    }
    // What comes: new items, and ones an undo brings back.
    let mut i = 0;
    while i < want.len() {
        if doc.get(i) == Some(&want[i]) {
            i += 1;
            continue;
        }
        out.insert_run(i, want[i], 1);
        doc.insert(i, want[i]);
        i += 1;
    }
    debug_assert_eq!(doc, want);
}

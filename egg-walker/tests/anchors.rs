//! M3's acceptance test: ink pinned to a paragraph stays on it through an
//! undo, local or remote, and a reload.
//!
//! The ink is pinned to the paragraph's identifying item (here its first
//! character; which item names a paragraph is the editor's choice). After
//! the paragraph is deleted the anchor is parked; after the delete is
//! undone it is back on the very same item, wherever that is now.

use mamre_egg_walker::{Anchors, Identities, Lv, Op, OpLog};

/// A replica: its log, its document at a version, and what's kept beside
/// the document.
#[derive(Clone)]
struct Replica {
    log: OpLog<char>,
    version: Vec<Lv>,
    doc: Vec<char>,
    ids: Identities,
    anchors: Anchors,
}

impl Replica {
    fn new() -> Self {
        Replica {
            log: OpLog::new(),
            version: Vec::new(),
            doc: Vec::new(),
            ids: Identities::new(),
            anchors: Anchors::new(0),
        }
    }

    fn text(&self) -> String {
        self.doc.iter().collect()
    }

    fn type_at(&mut self, who: &str, pos: usize, text: &str) {
        let agent = self.log.agent(who);
        let n = text.chars().count();
        self.version = self.log.add_insert(agent, &self.version, pos, text.chars());
        self.ids.insert(pos, self.version[0] + 1 - n, n);
        self.anchors.insert(pos, n);
        self.doc.splice(pos..pos, text.chars());
    }

    /// Deletes `range`; returns the deletes, for undoing.
    fn delete(&mut self, who: &str, range: std::ops::Range<usize>) -> Vec<Lv> {
        let agent = self.log.agent(who);
        self.version = self.log.add_delete(agent, &self.version, range.clone());
        let first = self.version[0] + 1 - range.len();
        self.ids.delete(range.start, first, range.len());
        self.anchors.delete(range.clone());
        self.doc.drain(range.clone());
        (first..first + range.len()).collect()
    }

    /// Brings the document from its version to `to`, by the walk.
    fn advance(&mut self, to: &[Lv]) {
        let to = self.log.union(&self.version, to);
        for change in self.log.changes_with(&self.version, &to, &mut self.ids) {
            self.anchors.apply(&change);
            change.apply(&mut self.doc);
        }
        self.version = to;
    }

    fn undo(&mut self, who: &str, deletes: &[Lv]) {
        let agent = self.log.agent(who);
        let after = self
            .log
            .add_undelete(agent, &self.version, deletes.iter().copied());
        self.advance(&after);
    }

    /// Takes in what `other` has, up to its version.
    fn sync_from(&mut self, other: &Replica) {
        self.log.pull(&other.log);
        let theirs = self
            .log
            .translate(&other.log, &other.version)
            .expect("pulled");
        self.advance(&theirs);
    }

    /// This replica's number for `other`'s item.
    fn here(&self, other: &Replica, item: Lv) -> Lv {
        self.log.translate(&other.log, &[item]).expect("pulled")[0]
    }
}

const TEXT: &str = "First paragraph.\nSecond one.\nThird.\n";
/// Where the second paragraph starts, and how long it is.
const SECOND: usize = 17;
const SECOND_LEN: usize = 12;

/// The ink, and the item it's pinned to.
fn ink(r: &mut Replica) -> Lv {
    let item = r.ids.item(SECOND);
    r.anchors.watch(SECOND, item);
    item
}

#[test]
fn local_undo() {
    let mut alice = Replica::new();
    alice.type_at("alice", 0, TEXT);
    let pinned = ink(&mut alice);

    let gone = alice.delete("alice", SECOND..SECOND + SECOND_LEN);
    assert_eq!(alice.text(), "First paragraph.\nThird.\n");
    assert!(alice.anchors.is_parked(pinned));
    assert_eq!(alice.anchors.position(pinned), None);

    alice.undo("alice", &gone);
    assert_eq!(alice.text(), TEXT);
    assert_eq!(alice.anchors.position(pinned), Some(SECOND));
    assert_eq!(alice.ids.item(SECOND), pinned, "the same item, not a copy");
}

#[test]
fn remote_undo() {
    let mut alice = Replica::new();
    alice.type_at("alice", 0, TEXT);
    let mut bob = Replica::new();
    bob.sync_from(&alice);
    let pinned = bob.here(&alice, alice.ids.item(SECOND));
    bob.anchors.watch(SECOND, pinned);
    // Bob writes an introduction meanwhile.
    bob.type_at("bob", 0, "Intro.\n");

    let gone = alice.delete("alice", SECOND..SECOND + SECOND_LEN);
    bob.sync_from(&alice);
    assert_eq!(bob.text(), "Intro.\nFirst paragraph.\nThird.\n");
    assert!(bob.anchors.is_parked(pinned));

    alice.undo("alice", &gone);
    bob.sync_from(&alice);
    assert_eq!(bob.text(), format!("Intro.\n{TEXT}"));
    assert_eq!(bob.anchors.position(pinned), Some(7 + SECOND));
    assert_eq!(
        bob.ids.item(7 + SECOND),
        pinned,
        "the same item, not a copy"
    );
}

#[test]
fn reload() {
    let mut alice = Replica::new();
    alice.type_at("alice", 0, TEXT);
    let pinned = ink(&mut alice);
    let gone = alice.delete("alice", SECOND..SECOND + SECOND_LEN);

    // Saved and loaded with everything kept beside the document (a clone
    // stands in for encoding, which is M4's).
    let mut loaded = alice.clone();
    drop(alice);
    loaded.undo("alice", &gone);
    assert_eq!(loaded.text(), TEXT);
    assert_eq!(loaded.anchors.position(pinned), Some(SECOND));

    // Loaded from the log alone: identities rebuilt from the history,
    // anchors restored by identity.
    let mut log_only = Replica::new();
    log_only.log = loaded.log.clone();
    let deleted_version = log_only.log.union(&[], &[gone[gone.len() - 1]]);
    log_only.advance(&deleted_version);
    assert_eq!(log_only.text(), "First paragraph.\nThird.\n");
    log_only.anchors = Anchors::new(log_only.doc.len());
    log_only.anchors.watch_deleted(pinned);
    log_only.undo("alice", &gone);
    assert_eq!(log_only.text(), TEXT);
    assert_eq!(log_only.anchors.position(pinned), Some(SECOND));
    assert_eq!(log_only.ids.item(SECOND), pinned);
}

/// The same, with a redo and a second undo after reloading, and the undo
/// going round to a third replica.
#[test]
fn undo_redo_undo_everywhere() {
    let mut alice = Replica::new();
    alice.type_at("alice", 0, TEXT);
    let mut carol = Replica::new();
    carol.sync_from(&alice);
    let pinned = ink(&mut alice);
    let carols = carol.here(&alice, pinned);
    carol.anchors.watch(SECOND, carols);

    let gone = alice.delete("alice", SECOND..SECOND + SECOND_LEN);
    alice.undo("alice", &gone);
    // Redo: a new delete.
    let again = alice.delete("alice", SECOND..SECOND + SECOND_LEN);
    let mut alice = alice.clone();
    alice.undo("alice", &again);
    assert_eq!(alice.anchors.position(pinned), Some(SECOND));

    carol.sync_from(&alice);
    assert_eq!(carol.text(), TEXT);
    assert_eq!(carol.anchors.position(carols), Some(SECOND));
    // Every event is accounted for: deletes, undos, the redo, the undo.
    let undos = (0..carol.log.len())
        .filter(|&lv| matches!(carol.log.event(lv).op, Op::Undelete { .. }))
        .count();
    assert_eq!(undos, 2 * SECOND_LEN);
}

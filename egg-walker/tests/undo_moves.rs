//! Undoing a move (#30): the originals come back, the copies go, and a
//! redo brings back the same copies.

use mamre_egg_walker::copies;
use mamre_egg_walker::{Accountant, Change, Lv, OpLog};

fn typed(log: &mut OpLog<char>, who: &str, text: &str) -> Vec<Lv> {
    let a = log.agent(who);
    log.add_insert(a, &[], 0, text.chars())
}

fn text_at(log: &OpLog<char>, v: &[Lv]) -> String {
    copies::checkout(log, v).into_iter().collect()
}

/// A replica merging `versions` in turn: its text, its items, and every
/// move it heard was lost.
fn replica(log: &OpLog<char>, versions: &[&[Lv]]) -> (String, Vec<Lv>, Vec<Lv>) {
    let mut books = Accountant::new();
    let mut doc: Vec<char> = Vec::new();
    let mut lost = Vec::new();
    for v in versions {
        for c in books.merge(log, v) {
            if let Change::Lost { op, .. } = c {
                lost.push(op);
            }
            c.apply(&mut doc);
        }
    }
    let ids = (0..books.len())
        .map(|i| books.identities().item(i))
        .collect();
    (doc.into_iter().collect(), ids, lost)
}

#[test]
fn an_undo_restores_the_document_exactly() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "abcdef");
    let a = log.agent("a");
    let moved = log.add_move(a, &v, 1..3, 5);
    assert_eq!(text_at(&log, &moved), "adebcf");
    let undone = log.add_undo(a, &moved, [moved[0]]);
    assert_eq!(text_at(&log, &undone), "abcdef");
    // The same items, in the same order: not copies of them.
    assert_eq!(
        copies::identities(&log, &undone),
        copies::identities(&log, &v)
    );
    assert_eq!(log.checkout(&undone), copies::checkout(&log, &undone));
    // A replica gets there by moving them back, and nothing is lost.
    let (text, ids, lost) = replica(&log, &[&v, &moved, &undone]);
    assert_eq!(text, "abcdef");
    assert_eq!(ids, copies::identities(&log, &v));
    assert!(lost.is_empty());
    assert!(copies::losses(&log, &undone).is_empty());
}

#[test]
fn a_redo_brings_back_the_same_copies() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "abcdef");
    let a = log.agent("a");
    let moved = log.add_move(a, &v, 1..3, 5);
    let undone = log.add_undo(a, &moved, [moved[0]]);
    let redone = log.add_undo(a, &undone, [undone[0]]);
    assert_eq!(text_at(&log, &redone), "adebcf");
    assert_eq!(
        copies::identities(&log, &redone),
        copies::identities(&log, &moved)
    );
    // And again, and again: the same copies each time.
    let undone2 = log.add_undo(a, &redone, [redone[0]]);
    assert_eq!(text_at(&log, &undone2), "abcdef");
    let redone2 = log.add_undo(a, &undone2, [undone2[0]]);
    assert_eq!(
        copies::identities(&log, &redone2),
        copies::identities(&log, &moved)
    );
    let (text, _, lost) = replica(&log, &[&moved, &undone, &redone, &undone2, &redone2]);
    assert_eq!(text, "adebcf");
    assert!(lost.is_empty());
}

#[test]
fn typing_inside_the_block_concurrently_survives_the_undo() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "abcdef");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    // U1 moves "bc" to the end; U2, not seeing it, types between b and c.
    let moved = log.add_move(u1, &base, 1..3, 6);
    let typed_in = log.add_insert(u2, &base, 2, ['X']);
    let both = log.union(&moved, &typed_in);
    assert_eq!(text_at(&log, &both), "adefbXc");
    // Undone: the X goes back with b and c.
    let undone = log.add_undo(u1, &both, [moved[0]]);
    assert_eq!(text_at(&log, &undone), "abXcdef");
    assert_eq!(log.checkout(&undone), copies::checkout(&log, &undone));
    let (text, _, lost) = replica(&log, &[&moved, &both, &undone]);
    assert_eq!(text, "abXcdef");
    assert!(lost.is_empty());
    let (text, _, _) = replica(&log, &[&typed_in, &undone]);
    assert_eq!(text, "abXcdef");
}

#[test]
fn typing_among_the_copies_goes_back_with_the_originals() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "abcdef");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    let moved = log.add_move(u1, &base, 1..3, 6);
    // U2 sees the move, and types between the copies of b and c.
    let typed_in = log.add_insert(u2, &moved, 5, ['X']);
    assert_eq!(text_at(&log, &typed_in), "adefbXc");
    // U1 undoes the move without seeing it: the X goes back too.
    let undone = log.add_undo(u1, &moved, [moved[0]]);
    let both = log.union(&undone, &typed_in);
    assert_eq!(text_at(&log, &both), "abXcdef");
    assert_eq!(log.checkout(&both), copies::checkout(&log, &both));
    let (text, _, _) = replica(&log, &[&typed_in, &both]);
    assert_eq!(text, "abXcdef");
    let (text, _, _) = replica(&log, &[&undone, &both]);
    assert_eq!(text, "abXcdef");
}

#[test]
fn undoing_a_move_that_lost_changes_nothing() {
    let mut log = OpLog::new();
    let base = typed(&mut log, "a", "abcdef");
    let (u1, u2) = (log.agent("u1"), log.agent("u2"));
    // The same block moved twice at once: U2's, later, wins.
    let first = log.add_move(u1, &base, 1..3, 0);
    let second = log.add_move(u2, &base, 1..3, 6);
    let both = log.union(&first, &second);
    let before = text_at(&log, &both);
    assert_eq!(before, "adefbc");
    let undone = log.add_undo(u1, &both, [first[0]]);
    assert_eq!(text_at(&log, &undone), before);
    // It's undone, not lost: the loss it was goes.
    assert!(copies::losses(&log, &undone).is_empty());
}

#[test]
fn a_section_survives_the_round_trip() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "One.Two.Three.");
    let a = log.agent("a");
    let v = log.add_mark(a, &v, 0, 1);
    let v = log.add_mark(a, &v, 4, 1);
    let v = log.add_mark(a, &v, 8, 1);
    let mut books = Accountant::new();
    let mut doc: Vec<char> = Vec::new();
    for c in books.merge(&log, &v) {
        c.apply(&mut doc);
    }
    let two = books.sections()[1].item;
    let range = books.section_range(two).unwrap();
    let moved = log.add_move(a, &v, range, 14);
    for c in books.merge(&log, &moved) {
        c.apply(&mut doc);
    }
    assert_eq!(doc.iter().collect::<String>(), "One.Three.Two.");
    let undone = log.add_undo(a, &moved, [moved[0]]);
    for c in books.merge(&log, &undone) {
        c.apply(&mut doc);
    }
    assert_eq!(doc.iter().collect::<String>(), "One.Two.Three.");
    // The same section, by the same item, where it was.
    assert_eq!(books.section_range(two), Some(4..8));
    assert_eq!(books.level(two), 1);
}

#[test]
fn undo_and_redo_round_trip_in_a_chunk() {
    let mut log = OpLog::new();
    let v = typed(&mut log, "a", "abcdef");
    let a = log.agent("a");
    let v = log.add_move(a, &v, 1..3, 5);
    let m = v[0];
    let v = log.add_delete(a, &v, 0..1);
    let d = v[0];
    // One undo of both, then a redo of the move.
    let v = log.add_undo(a, &v, [d, m]);
    let v = log.add_undo(a, &v, [v[0]]);
    let mut bytes = Vec::new();
    log.encode_chunk(0..log.len(), &mut bytes);
    let mut back = OpLog::<char>::new();
    assert_eq!(back.decode_chunk(&bytes), Ok(log.len()));
    for lv in 0..log.len() {
        assert_eq!(
            format!("{:?}", back.event(lv)),
            format!("{:?}", log.event(lv))
        );
    }
    assert_eq!(back.checkout(back.version()), log.checkout(&v));
    assert_eq!(text_at(&log, &v), "adebcf");
}

#[test]
fn an_undo_of_an_insert_is_refused() {
    // "x" by "a", then a kind-2 run naming it.
    // compression, agents, "a", runs: 2,
    //   agent 0, seq 0, no parents, insert, pos 0, count 1, "x";
    //   agent 0, seq 1, parent the event before, undo, count 1, that event.
    let bytes = [
        0, 1, 1, b'a', 2, 0, 0, 0, 0, 0, 1, 1, b'x', 0, 0, 1, 2, 4, 1, 2,
    ];
    let mut log = OpLog::<char>::new();
    assert_eq!(
        log.decode_chunk(&bytes),
        Err(mamre::codec::DecodeError::Invalid(
            "an undo of something not a delete, a move or an undo of a move"
        ))
    );
    assert_eq!(log.len(), 0);
}

/// Found by the copies proptests: typing at the edge of a moved block
/// (after what's before it, before its first copy) stays there when the
/// move is undone, beside the copies, rather than going back with them.
#[test]
fn typing_at_the_edge_of_an_undone_move_stays() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, ['a']);
    let v = log.add_insert(a, &v, 0, ['b']);
    let v = log.add_insert(a, &v, 0, ['c']);
    let moved = log.add_move(a, &v, 0..1, 2);
    let typed = log.add_insert(a, &moved, 1, ['x']);
    assert_eq!(text_at(&log, &typed), "bxca");
    let undone = log.add_undo(a, &typed, [moved[0]]);
    assert_eq!(text_at(&log, &undone), "cbxa");
    let (text, ids, _) = replica(&log, &[&v, &moved, &typed, &undone]);
    assert_eq!(text, "cbxa");
    assert_eq!(ids, copies::identities(&log, &undone));
}

/// Found by the copies proptests: a move, undone, then made again, then
/// typing at the start, past the undone move's copies (laid out, never
/// drawn).
#[test]
fn a_move_made_again_after_an_undo() {
    let mut log = OpLog::new();
    let a = log.agent("a");
    let v = log.add_insert(a, &[], 0, ['a']);
    let v = log.add_insert(a, &v, 0, ['b']);
    let v = log.add_insert(a, &v, 0, ['c']);
    let moved = log.add_move(a, &v, 1..2, 0);
    let undone = log.add_undo(a, &moved, [moved[0]]);
    let again = log.add_move(a, &undone, 1..2, 0);
    let typed = log.add_insert(a, &again, 0, ['x']);
    assert_eq!(text_at(&log, &typed), "xbca");
    let (text, ids, _) = replica(&log, &[&v, &moved, &undone, &again, &typed]);
    assert_eq!(text, "xbca");
    assert_eq!(ids, copies::identities(&log, &typed));
}

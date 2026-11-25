//! Saving and loading: a file must give back exactly what was saved, a crash
//! at any byte must lose only the save it cut off, a damaged byte must be
//! caught, and a chunk must sync a log as `pull` does.

use mamre_egg_walker::file::{self, Loaded};
use mamre_egg_walker::{Accountant, Lv, Op, OpLog};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Act {
    Insert {
        r: usize,
        at: u16,
        n: usize,
    },
    Delete {
        r: usize,
        a: u16,
        b: u16,
    },
    Undo {
        r: usize,
        pick: u16,
    },
    Sync {
        r: usize,
        from: usize,
    },
    Watch {
        r: usize,
        at: u16,
    },
    /// Replica 0 saves.
    Save,
}

#[derive(Clone)]
struct Replica {
    name: String,
    log: OpLog<char>,
    doc: Vec<char>,
    books: Accountant,
}

fn frac(len: usize, f: u16) -> usize {
    len * f as usize / u16::MAX as usize
}

/// Merges `to` in: a local edit, or what came from elsewhere.
fn advance(rep: &mut Replica, to: &[Lv]) {
    for change in rep.books.merge(&rep.log, to) {
        change.apply(&mut rep.doc);
    }
}

fn act(reps: &mut [Replica], act: &Act, next: &mut u32) {
    match *act {
        Act::Insert { r, at, n } => {
            let rep = &mut reps[r];
            let agent = rep.log.agent(&rep.name.clone());
            let pos = frac(rep.doc.len(), at);
            let items: Vec<char> = (0..n as u32)
                .map(|k| {
                    char::from_u32(
                        0x61 + (*next + k) % 26
                            + if (*next + k).is_multiple_of(7) {
                                0x3b1 - 0x61
                            } else {
                                0
                            },
                    )
                    .unwrap()
                })
                .collect();
            *next += n as u32;
            // A local edit: made on the document, then recorded.
            let v = rep
                .log
                .add_insert(agent, rep.books.version(), pos, items.clone());
            rep.doc.splice(pos..pos, items);
            rep.books.record(&rep.log, &v);
            assert_eq!(rep.books.len(), rep.doc.len());
        }
        Act::Delete { r, a, b } => {
            let rep = &mut reps[r];
            let (a, b) = (frac(rep.doc.len(), a), frac(rep.doc.len(), b));
            let range = a.min(b)..a.max(b);
            if !range.is_empty() {
                let agent = rep.log.agent(&rep.name.clone());
                let v = rep
                    .log
                    .add_delete(agent, rep.books.version(), range.clone());
                rep.doc.drain(range);
                rep.books.record(&rep.log, &v);
            }
        }
        Act::Undo { r, pick } => {
            let rep = &mut reps[r];
            let deletes: Vec<Lv> = (0..rep.log.len())
                .filter(|&lv| {
                    matches!(rep.log.event(lv).op, Op::Delete { .. })
                        && rep.log.contains(rep.books.version(), lv)
                })
                .collect();
            if !deletes.is_empty() {
                let d = deletes[frac(deletes.len() - 1, pick)];
                let agent = rep.log.agent(&rep.name.clone());
                let after = rep.log.add_undelete(agent, rep.books.version(), [d]);
                advance(rep, &after);
            }
        }
        Act::Sync { r, from } => {
            if r != from {
                let theirs = reps[from].clone();
                let rep = &mut reps[r];
                rep.log.pull(&theirs.log);
                let v = rep
                    .log
                    .translate(&theirs.log, theirs.books.version())
                    .unwrap();
                advance(rep, &v);
            }
        }
        Act::Watch { r, at } => {
            let rep = &mut reps[r];
            if !rep.doc.is_empty() {
                let pos = frac(rep.doc.len() - 1, at);
                rep.books.watch(pos);
            }
        }
        Act::Save => {}
    }
}

fn acts() -> impl Strategy<Value = Vec<Act>> {
    let r = 0..3usize;
    prop::collection::vec(
        prop_oneof![
            4 => (r.clone(), any::<u16>(), 1usize..5).prop_map(|(r, at, n)| Act::Insert { r, at, n }),
            2 => (r.clone(), any::<u16>(), any::<u16>()).prop_map(|(r, a, b)| Act::Delete { r, a, b }),
            1 => (r.clone(), any::<u16>()).prop_map(|(r, pick)| Act::Undo { r, pick }),
            2 => (r.clone(), r.clone()).prop_map(|(r, from)| Act::Sync { r, from }),
            1 => (r, any::<u16>()).prop_map(|(r, at)| Act::Watch { r, at }),
            2 => Just(Act::Save),
        ],
        1..40,
    )
}

/// Every event of a log in its order, by agent name and seq, parents
/// sorted: what it means, whatever order a log keeps parents in.
type Events = Vec<((String, u64), Vec<(String, u64)>, String)>;

fn events(log: &OpLog<char>) -> Events {
    let stable = |lv: Lv| {
        let e = log.event(lv);
        (log.agent_name(e.agent).to_string(), e.seq)
    };
    (0..log.len())
        .map(|lv| {
            let e = log.event(lv);
            let mut parents: Vec<_> = e.parents.iter().map(|&p| stable(p)).collect();
            parents.sort();
            let op = match e.op {
                Op::Insert { pos, item } => format!("ins {pos} {item}"),
                Op::Delete { pos } => format!("del {pos}"),
                Op::Undelete { target } => format!("undel {:?}", stable(target)),
                Op::Move { pos, len, to } => format!("move {pos} {len} {to}"),
                Op::Mark { pos, level } => format!("mark {pos} {level}"),
            };
            (stable(lv), parents, op)
        })
        .collect()
}

/// What a replica saves, to compare with what loads.
#[derive(Debug, PartialEq)]
struct State {
    version: Vec<Lv>,
    doc: Vec<char>,
    ids: Vec<Lv>,
    removed: Vec<(Lv, Lv)>,
    anchors: Vec<(Lv, Option<usize>, bool)>,
}

fn state(log: &OpLog<char>, doc: &[char], books: &Accountant) -> State {
    let (ids, anchors) = (books.identities(), books.anchors());
    State {
        version: books.version().to_vec(),
        doc: doc.to_vec(),
        ids: (0..ids.len()).map(|k| ids.item(k)).collect(),
        removed: (0..log.len())
            .filter_map(|lv| ids.removed(lv).map(|item| (lv, item)))
            .collect(),
        anchors: (0..log.len())
            .map(|lv| (lv, anchors.position(lv), anchors.is_parked(lv)))
            .filter(|&(_, p, parked)| p.is_some() || parked)
            .collect(),
    }
}

/// Runs the acts; returns the file replica 0 wrote, and the state and
/// event count at each save, in order.
#[allow(clippy::type_complexity)]
fn run(acts: &[Act]) -> (Vec<u8>, Vec<(usize, usize, State, Events)>) {
    let mut reps: Vec<Replica> = (0..3)
        .map(|i| Replica {
            name: format!("r{i}"),
            log: OpLog::new(),
            doc: Vec::new(),
            books: Accountant::new(),
        })
        .collect();
    let mut next = 0;
    let mut bytes = file::create();
    let mut saved = 0;
    let mut saves = Vec::new();
    for a in acts.iter().chain([&Act::Save]) {
        act(&mut reps, a, &mut next);
        if let Act::Save = a {
            let r = &reps[0];
            saved = file::save(&mut bytes, &r.log, saved, r.doc.iter().copied(), &r.books);
            saves.push((
                bytes.len(),
                saved,
                state(&r.log, &r.doc, &r.books),
                events(&r.log),
            ));
        }
    }
    (bytes, saves)
}

fn loaded_state(l: &Loaded<char>) -> Option<State> {
    let s = l.snapshot.as_ref()?;
    Some(state(&l.log, &s.doc, &s.accountant))
}

proptest! {
    /// Loading gives back the log and the last snapshot exactly, and the
    /// snapshot's document is what the log checks out to.
    #[test]
    fn round_trip(acts in acts()) {
        let (bytes, saves) = run(&acts);
        let loaded = file::load::<char>(&bytes).unwrap();
        prop_assert!(!loaded.damaged);
        prop_assert_eq!(loaded.good, bytes.len());
        let (_, saved, want, log_events) = saves.last().unwrap();
        prop_assert_eq!(loaded.log.len(), *saved);
        prop_assert_eq!(&events(&loaded.log), log_events, "the same events, numbered the same");
        let got = loaded_state(&loaded).unwrap();
        prop_assert_eq!(&got, want);
        prop_assert_eq!(loaded.log.checkout(&got.version), got.doc);
    }

    /// Cut off at any byte, a file loads as far as it was good: every save
    /// that finished, nothing after, and never a panic.
    #[test]
    fn a_crash_at_any_byte(acts in acts()) {
        let (bytes, saves) = run(&acts);
        let header = file::create().len();
        for cut in 0..=bytes.len() {
            let Ok(loaded) = file::load::<char>(&bytes[..cut]) else {
                prop_assert!(cut < header);
                continue;
            };
            prop_assert!(loaded.good <= cut);
            prop_assert_eq!(loaded.damaged, loaded.good != cut);
            // The last save that finished before the cut is what loads.
            match saves.iter().rev().find(|(end, ..)| *end <= cut) {
                Some((_, _, want, _)) => {
                    let got = loaded_state(&loaded);
                    prop_assert_eq!(got.as_ref(), Some(want));
                }
                None => prop_assert!(loaded.snapshot.is_none()),
            }
        }
    }

    /// A damaged byte anywhere is caught: loading stops at or before it,
    /// and never panics.
    #[test]
    fn a_damaged_byte(acts in acts(), at in any::<prop::sample::Index>(), flip in 1u8..=255) {
        let (mut bytes, _) = run(&acts);
        let header = file::create().len();
        let i = at.index(bytes.len());
        bytes[i] ^= flip;
        match file::load::<char>(&bytes) {
            Err(_) => prop_assert!(i < header),
            Ok(loaded) => {
                prop_assert!(loaded.damaged);
                prop_assert!(loaded.good <= i);
            }
        }
    }

    /// A chunk syncs a log behind as `pull` does, and applying it again
    /// adds nothing.
    #[test]
    fn a_chunk_syncs_like_pull(acts in acts(), split in any::<prop::sample::Index>()) {
        let (bytes, _) = run(&acts);
        let full = file::load::<char>(&bytes).unwrap().log;
        if full.is_empty() {
            return Ok(());
        }
        // A replica that has a prefix of the events, numbered its own way:
        // pulled from a log holding just that prefix.
        let k = split.index(full.len() + 1);
        let mut prefix_bytes = Vec::new();
        full.encode_chunk(0..k, &mut prefix_bytes);
        let mut behind: OpLog<char> = OpLog::new();
        behind.agent("someone else");
        behind.decode_chunk(&prefix_bytes).unwrap();
        let mut pulled = behind.clone();
        pulled.pull(&full);

        let mut chunk = Vec::new();
        full.encode_chunk(k..full.len(), &mut chunk);
        prop_assert_eq!(behind.decode_chunk(&chunk).unwrap(), full.len() - k);
        prop_assert_eq!(events(&behind), events(&pulled));
        prop_assert_eq!(behind.decode_chunk(&chunk).unwrap(), 0, "again: nothing new");
    }

    /// Any bytes at all, as a file or a chunk: an error or a result, never
    /// a panic.
    #[test]
    fn garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let mut file_bytes = file::create();
        file_bytes.extend(&bytes);
        let _ = file::load::<char>(&file_bytes);
        let mut log: OpLog<char> = OpLog::new();
        let _ = log.decode_chunk(&bytes);
    }
}

/// A chunk that refers to events the log doesn't have adds nothing.
#[test]
fn a_chunk_needs_what_came_before() {
    let mut a: OpLog<char> = OpLog::new();
    let alice = a.agent("alice");
    let v = a.add_insert(alice, &[], 0, "hello".chars());
    a.add_insert(alice, &v, 5, " world".chars());
    let mut chunk = Vec::new();
    a.encode_chunk(5..a.len(), &mut chunk);
    let mut empty: OpLog<char> = OpLog::new();
    assert!(empty.decode_chunk(&chunk).is_err());
    assert!(empty.is_empty(), "nothing added");
}

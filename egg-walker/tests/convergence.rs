//! Replicas editing concurrently and syncing in random patterns, so events
//! arrive in different orders and merges are of genuinely concurrent work.
//!
//! After every action, on the replica it touched: the document it keeps up
//! to date by applying the walk's changes must equal a fresh checkout of
//! its version, and the slow reference's. At the end, everyone syncs with
//! everyone, and every replica must hold the same document, though each
//! log numbered the events differently.

use mamre_egg_walker::{Lv, OpLog, reference};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Act {
    /// Replica `r` inserts `n` items about this fraction of the way in.
    Insert { r: usize, at: u16, n: usize },
    /// Replica `r` deletes between two fractions.
    Delete { r: usize, a: u16, b: u16 },
    /// Replica `r` takes in everything replica `from` knows.
    Sync { r: usize, from: usize },
}

struct Replica {
    log: OpLog<u32>,
    name: String,
    /// The version this replica's document is at.
    version: Vec<Lv>,
    doc: Vec<u32>,
}

fn frac(len: usize, f: u16) -> usize {
    len * f as usize / u16::MAX as usize
}

/// Checks a replica's document against a fresh checkout and the reference.
fn verify(r: &Replica) -> Result<(), String> {
    let fresh = r.log.checkout(&r.version);
    if r.doc != fresh {
        return Err(format!(
            "{}: kept {:?}, fresh checkout {fresh:?}",
            r.name, r.doc
        ));
    }
    let slow = reference::checkout(&r.log, &r.version);
    if r.doc != slow {
        return Err(format!("{}: walk {:?}, reference {slow:?}", r.name, r.doc));
    }
    Ok(())
}

fn sync(replicas: &mut [Replica], r: usize, from: usize) {
    if r == from {
        return;
    }
    let (theirs, their_version) = (replicas[from].log.clone(), replicas[from].version.clone());
    let me = &mut replicas[r];
    me.log.pull(&theirs);
    let theirs_here = me
        .log
        .translate(&theirs, &their_version)
        .expect("just pulled");
    let before = me.version.clone();
    me.version = me.log.union(&before, &theirs_here);
    let changes = me.log.changes(&before, &me.version);
    // The fast walk and the simple one must agree change for change, not
    // just on the document they lead to.
    assert_eq!(
        changes,
        me.log.changes_simple(&before, &me.version),
        "the tree walk and the list walk differ"
    );
    for change in changes {
        change.apply(&mut me.doc);
    }
}

fn run(replicas: usize, acts: &[Act]) -> Result<(), String> {
    let mut next_item = 0u32;
    let mut reps: Vec<Replica> = (0..replicas)
        .map(|i| Replica {
            log: OpLog::new(),
            name: format!("r{i}"),
            version: Vec::new(),
            doc: Vec::new(),
        })
        .collect();

    for (step, act) in acts.iter().enumerate() {
        let r = match *act {
            Act::Insert { r, at, n } => {
                let rep = &mut reps[r];
                let agent = rep.log.agent(&rep.name.clone());
                let pos = frac(rep.doc.len(), at);
                let items: Vec<u32> = (0..n as u32).map(|k| next_item + k).collect();
                next_item += n as u32;
                rep.version = rep.log.add_insert(agent, &rep.version, pos, items.clone());
                rep.doc.splice(pos..pos, items);
                r
            }
            Act::Delete { r, a, b } => {
                let rep = &mut reps[r];
                let agent = rep.log.agent(&rep.name.clone());
                let (a, b) = (frac(rep.doc.len(), a), frac(rep.doc.len(), b));
                let range = a.min(b)..a.max(b);
                rep.version = rep.log.add_delete(agent, &rep.version, range.clone());
                rep.doc.drain(range);
                r
            }
            Act::Sync { r, from } => {
                sync(&mut reps, r, from);
                r
            }
        };
        verify(&reps[r]).map_err(|e| format!("step {step}, {act:?}: {e}"))?;
    }

    // Everyone syncs with everyone, twice round, so all know everything.
    for _ in 0..2 {
        for r in 0..replicas {
            for from in 0..replicas {
                sync(&mut reps, r, from);
            }
        }
    }
    for rep in &reps {
        verify(rep).map_err(|e| format!("after full sync: {e}"))?;
        if rep.doc != reps[0].doc {
            return Err(format!(
                "diverged: {} has {:?}, {} has {:?}",
                rep.name, rep.doc, reps[0].name, reps[0].doc
            ));
        }
    }
    Ok(())
}

fn acts(replicas: usize) -> impl Strategy<Value = Vec<Act>> {
    let r = 0..replicas;
    let act = prop_oneof![
        4 => (r.clone(), any::<u16>(), 1usize..4).prop_map(|(r, at, n)| Act::Insert { r, at, n }),
        2 => (r.clone(), any::<u16>(), any::<u16>()).prop_map(|(r, a, b)| Act::Delete { r, a, b }),
        2 => (r.clone(), r).prop_map(|(r, from)| Act::Sync { r, from }),
    ];
    prop::collection::vec(act, 1..50)
}

proptest! {
    #[test]
    fn two_replicas_converge(acts in acts(2)) {
        run(2, &acts).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn four_replicas_converge(acts in acts(4)) {
        run(4, &acts).map_err(TestCaseError::fail)?;
    }
}

/// Two replicas insert at the same place without seeing each other: both
/// end up with the same order, decided by agent name.
#[test]
fn concurrent_inserts_at_one_place() {
    let mut a: OpLog<char> = OpLog::new();
    let alice = a.agent("alice");
    let base = a.add_insert(alice, &[], 0, "xz".chars());
    let mut b = a.clone();
    let bob = b.agent("bob");

    let va = a.add_insert(alice, &base, 1, "A".chars());
    let vb = b.add_insert(bob, &base, 1, "B".chars());
    a.pull(&b);
    b.pull(&a);
    let both_a = a.union(&va, &a.translate(&b, &vb).unwrap());
    let both_b = b.union(&vb, &b.translate(&a, &va).unwrap());
    let text = |log: &OpLog<char>, v: &[Lv]| log.checkout(v).into_iter().collect::<String>();
    assert_eq!(text(&a, &both_a), "xABz");
    assert_eq!(text(&b, &both_b), "xABz");
}

/// A delete and a concurrent insert inside the deleted range: the insert
/// survives, and both replicas agree.
#[test]
fn insert_inside_a_concurrent_delete() {
    let mut a: OpLog<char> = OpLog::new();
    let alice = a.agent("alice");
    let base = a.add_insert(alice, &[], 0, "hello".chars());
    let mut b = a.clone();
    let bob = b.agent("bob");

    let va = a.add_delete(alice, &base, 1..4); // "ho"
    let vb = b.add_insert(bob, &base, 2, "X".chars()); // "heXllo"
    a.pull(&b);
    let both = a.union(&va, &a.translate(&b, &vb).unwrap());
    let text: String = a.checkout(&both).into_iter().collect();
    assert_eq!(text, "hXo");
    assert_eq!(
        reference::checkout(&a, &both)
            .into_iter()
            .collect::<String>(),
        "hXo"
    );
}

//! Agreement with diamond-types, the paper's own implementation: the one
//! check on the ordering rule that doesn't share code with it.
//!
//! The same random concurrent history goes into both logs: several agents
//! editing at their own versions and merging each other's now and then.
//! Both number events the same way (one per char, in the order added), so
//! versions carry over as they are. Every agent's version, and the whole,
//! must check out to the same text in both.

use diamond_types::list::OpLog as Diamond;
use mamre_egg_walker::{Lv, OpLog};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Act {
    Insert {
        a: usize,
        at: u16,
        text: String,
    },
    Delete {
        a: usize,
        from: u16,
        to: u16,
    },
    /// Agent `a` takes in everything agent `from` has seen.
    Merge {
        a: usize,
        from: usize,
    },
}

const AGENTS: usize = 3;

fn frac(len: usize, f: u16) -> usize {
    len * f as usize / u16::MAX as usize
}

fn text(log: &OpLog<char>, version: &[Lv]) -> String {
    log.checkout(version).into_iter().collect()
}

fn run(acts: &[Act]) -> Result<(), String> {
    let mut ours: OpLog<char> = OpLog::new();
    let mut theirs = Diamond::new();
    let names: Vec<String> = (0..AGENTS).map(|i| format!("agent{i}")).collect();
    let agents: Vec<_> = names.iter().map(|n| ours.agent(n)).collect();
    let dagents: Vec<_> = names
        .iter()
        .map(|n| theirs.get_or_create_agent_id(n))
        .collect();
    let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); AGENTS];
    // Each agent's document, kept up to date by applying merges' changes:
    // other agents' events are numbered among this one's, so merges replay
    // new events interleaved with old ones.
    let mut docs: Vec<Vec<char>> = vec![Vec::new(); AGENTS];

    for act in acts {
        match act {
            Act::Insert { a, at, text: t } => {
                if t.is_empty() {
                    continue;
                }
                let len = ours.checkout(&versions[*a]).len();
                let pos = frac(len, *at);
                let v = ours.add_insert(agents[*a], &versions[*a], pos, t.chars());
                docs[*a].splice(pos..pos, t.chars());
                let d = theirs.add_insert_at(dagents[*a], &versions[*a], pos, t);
                if v != [d] {
                    return Err(format!("numbering differs: ours {v:?}, theirs {d}"));
                }
                versions[*a] = v;
            }
            Act::Delete { a, from, to } => {
                let len = ours.checkout(&versions[*a]).len();
                let (x, y) = (frac(len, *from), frac(len, *to));
                let range = x.min(y)..x.max(y);
                if range.is_empty() {
                    continue;
                }
                let v = ours.add_delete(agents[*a], &versions[*a], range.clone());
                docs[*a].drain(range.clone());
                let d = theirs.add_delete_at(dagents[*a], &versions[*a], range);
                if v != [d] {
                    return Err(format!("numbering differs: ours {v:?}, theirs {d}"));
                }
                versions[*a] = v;
            }
            Act::Merge { a, from } => {
                let merged = ours.union(&versions[*a], &versions[*from]);
                for change in ours.changes(&versions[*a], &merged) {
                    change.apply(&mut docs[*a]);
                }
                versions[*a] = merged;
                let fresh = ours.checkout(&versions[*a]);
                if docs[*a] != fresh {
                    return Err(format!(
                        "agent {a}'s merged document {:?} isn't a fresh checkout {:?}",
                        docs[*a].iter().collect::<String>(),
                        fresh.iter().collect::<String>()
                    ));
                }
            }
        }
    }

    let mut check = versions.clone();
    check.push(ours.version().to_vec());
    for v in &check {
        let (mine, dt) = (text(&ours, v), theirs.checkout(v).content().to_string());
        if mine != dt {
            return Err(format!("at {v:?}: ours {mine:?}, diamond-types {dt:?}"));
        }
    }
    Ok(())
}

fn act() -> impl Strategy<Value = Act> {
    let a = 0..AGENTS;
    prop_oneof![
        4 => (a.clone(), any::<u16>(), "[a-e]{1,4}").prop_map(|(a, at, text)| Act::Insert { a, at, text }),
        2 => (a.clone(), any::<u16>(), any::<u16>()).prop_map(|(a, from, to)| Act::Delete { a, from, to }),
        2 => (a.clone(), a).prop_map(|(a, from)| Act::Merge { a, from }),
    ]
}

proptest! {
    #[test]
    fn agrees_with_diamond_types(acts in prop::collection::vec(act(), 1..60)) {
        run(&acts).map_err(TestCaseError::fail)?;
    }
}

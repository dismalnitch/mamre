//! Random concurrent histories: several replicas edit at their own versions
//! and sync now and then. The rope, fed transformed operations, must match
//! diamond-types' own checkout at every replica's version, both built from
//! scratch and merged forward from where each replica was.

use diamond_types::list::OpLog;
use diamond_types::{LocalVersion, Time};
use mamre_diamond::merge;
use mamre_rope::{Markdown, Rope};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Act {
    /// Replica `r` inserts `text` about this fraction of the way through.
    Insert { r: usize, at: u16, text: String },
    /// Replica `r` deletes between two fractions.
    Delete { r: usize, a: u16, b: u16 },
    /// Replica `r` backspaces `n` times from about this fraction: one char
    /// at a time, backwards, which diamond-types stores as a reversed run.
    Backspaces { r: usize, at: u16, n: usize },
    /// Replica `r` takes in everything replica `from` has seen.
    Sync { r: usize, from: usize },
}

const REPLICAS: usize = 3;

/// Both versions merged, as a frontier: keep only entries no other entry
/// already contains. (`OpLog::version_union` can keep ancestors, which
/// diamond-types then rejects.)
fn union(oplog: &OpLog, a: &[Time], b: &[Time]) -> LocalVersion {
    let mut all: Vec<Time> = a.iter().chain(b).copied().collect();
    all.sort();
    all.dedup();
    all.iter()
        .copied()
        .filter(|&t| {
            !all.iter()
                .any(|&u| u != t && oplog.version_contains_time(&[u], t))
        })
        .collect()
}

fn chars_at(oplog: &OpLog, version: &[Time]) -> usize {
    oplog.checkout(version).content().len_chars()
}

fn play(acts: &[Act]) -> Result<(), String> {
    let mut oplog = OpLog::new();
    let agents: Vec<_> = (0..REPLICAS)
        .map(|i| oplog.get_or_create_agent_id(&format!("r{i}")))
        .collect();
    let mut versions: Vec<LocalVersion> = vec![LocalVersion::new(); REPLICAS];

    for act in acts {
        match act {
            Act::Insert { r, at, text } => {
                if text.is_empty() {
                    continue;
                }
                let n = chars_at(&oplog, &versions[*r]);
                let pos = n * *at as usize / u16::MAX as usize;
                let t = oplog.add_insert_at(agents[*r], &versions[*r], pos, text);
                versions[*r] = [t].into_iter().collect();
            }
            Act::Delete { r, a, b } => {
                let n = chars_at(&oplog, &versions[*r]);
                let (a, b) = (
                    n * *a as usize / u16::MAX as usize,
                    n * *b as usize / u16::MAX as usize,
                );
                let (a, b) = (a.min(b), a.max(b));
                if a == b {
                    continue;
                }
                let t = oplog.add_delete_at(agents[*r], &versions[*r], a..b);
                versions[*r] = [t].into_iter().collect();
            }
            Act::Backspaces { r, at, n } => {
                let len = chars_at(&oplog, &versions[*r]);
                let mut pos = len * *at as usize / u16::MAX as usize;
                for _ in 0..*n {
                    if pos == 0 {
                        break;
                    }
                    let t = oplog.add_delete_at(agents[*r], &versions[*r], pos - 1..pos);
                    versions[*r] = [t].into_iter().collect();
                    pos -= 1;
                }
            }
            Act::Sync { r, from } => {
                versions[*r] = union(&oplog, &versions[*r], &versions[*from]);
            }
        }
    }

    let tip = oplog.local_version();
    let want_tip = oplog.checkout_tip().content().to_string();

    // From scratch to the tip.
    let mut rope: Rope<Markdown, 8> = Rope::new();
    merge(&mut rope, &oplog, &[], &tip);
    rope.check()?;
    if rope.to_string() != want_tip {
        return Err(format!(
            "from scratch: {:?}, want {want_tip:?}",
            rope.to_string()
        ));
    }

    // Each replica's own version, then merged forward to the tip: what a
    // replica does when everyone else's edits arrive.
    for (r, version) in versions.iter().enumerate() {
        let want = oplog.checkout(version).content().to_string();
        let mut rope: Rope<Markdown, 8> = Rope::new();
        merge(&mut rope, &oplog, &[], version);
        if rope.to_string() != want {
            return Err(format!(
                "replica {r} at its version: {:?}, want {want:?}",
                rope.to_string()
            ));
        }
        merge(&mut rope, &oplog, version, &tip);
        rope.check()?;
        if rope.to_string() != want_tip {
            return Err(format!(
                "replica {r} merged to the tip: {:?}, want {want_tip:?}",
                rope.to_string()
            ));
        }
        if *rope.summary() != mamre_rope::TextSummary::of(&want_tip) {
            return Err(format!("replica {r}: summary differs"));
        }
    }
    Ok(())
}

fn text() -> impl Strategy<Value = String> {
    let pool = vec!['a', 'b', 'c', ' ', '\n', '#', 'é', '日', '🦀'];
    prop::collection::vec(prop::sample::select(pool), 0..12).prop_map(|cs| cs.into_iter().collect())
}

fn act() -> impl Strategy<Value = Act> {
    let r = 0..REPLICAS;
    prop_oneof![
        4 => (r.clone(), any::<u16>(), text()).prop_map(|(r, at, text)| Act::Insert { r, at, text }),
        2 => (r.clone(), any::<u16>(), any::<u16>()).prop_map(|(r, a, b)| Act::Delete { r, a, b }),
        2 => (r.clone(), any::<u16>(), 1usize..8).prop_map(|(r, at, n)| Act::Backspaces { r, at, n }),
        1 => (r.clone(), r).prop_map(|(r, from)| Act::Sync { r, from }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(400))]
    #[test]
    fn the_rope_plays_diamond_types(acts in prop::collection::vec(act(), 1..60)) {
        play(&acts).map_err(TestCaseError::fail)?;
    }
}

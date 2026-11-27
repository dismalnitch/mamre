//! Peers syncing over a bad network: messages dropped, duplicated, delayed
//! and delivered in any order, plus garbage. Each peer only ever sends its
//! summary, or a chunk of what another is missing; a chunk that arrives
//! before what it needs fails whole and comes again later.
//!
//! After every step, each peer's document is checked against the
//! reference's checkout of its version, and its identities against the
//! reference's. Once the network heals (every message delivered, rounds
//! until nothing new), every peer must have the same events and the same
//! document.

use mamre_egg_walker::sync::Message;
use mamre_egg_walker::{Identities, Lv, Op, OpLog, reference};
use proptest::prelude::*;

/// A message on the wire: its bytes.
type Msg = Vec<u8>;

#[derive(Clone, Debug)]
enum Act {
    Insert {
        p: usize,
        at: u16,
        n: usize,
    },
    Delete {
        p: usize,
        a: u16,
        b: u16,
    },
    Undo {
        p: usize,
        pick: u16,
    },
    /// Peer `p` copies its text from `a` to `b` and pastes it at `at`
    /// (#36).
    Paste {
        p: usize,
        a: u16,
        b: u16,
        at: u16,
    },
    /// Peer `p` sends its summary to peer `to`.
    Gossip {
        p: usize,
        to: usize,
    },
    /// Peer `p` pushes its latest `k` events to peer `to`, as an editor
    /// sends new edits straight away: they can overtake what they need.
    Push {
        p: usize,
        to: usize,
        k: usize,
    },
    /// Delivers the message about this far along the queue.
    Deliver {
        pick: u16,
    },
    Drop {
        pick: u16,
    },
    Duplicate {
        pick: u16,
    },
    /// Garbage, or a message with a byte changed, arrives at peer `p`.
    Garbage {
        p: usize,
        bytes: Vec<u8>,
        from_queue: Option<u16>,
    },
}

struct Peer {
    name: String,
    log: OpLog<char>,
    doc: Vec<char>,
    ids: Identities,
}

impl Peer {
    fn version(&self) -> Vec<Lv> {
        self.log.version().to_vec()
    }
}

/// Each pasted event and its source, named by agent and seq (#36).
fn lineage(log: &OpLog<char>) -> std::collections::BTreeSet<((String, u64), (String, u64))> {
    let name = |lv: Lv| {
        let e = log.event(lv);
        (log.agent_name(e.agent).to_string(), e.seq)
    };
    (0..log.len())
        .filter_map(|lv| log.source(lv).map(|s| (name(lv), name(s))))
        .collect()
}

fn frac(len: usize, f: u16) -> usize {
    len * f as usize / u16::MAX as usize
}

/// Brings a peer's document up to everything its log has.
fn catch_up(peer: &mut Peer, from: &[Lv]) {
    let to = peer.version();
    for change in peer.log.changes_with(from, &to, &mut peer.ids) {
        change.apply(&mut peer.doc);
    }
}

fn verify(peer: &Peer) -> Result<(), String> {
    let v = peer.version();
    let want = reference::checkout(&peer.log, &v);
    if peer.doc != want {
        return Err(format!(
            "{}: doc {:?}, reference {want:?}",
            peer.name, peer.doc
        ));
    }
    let ids: Vec<Lv> = (0..peer.ids.len()).map(|k| peer.ids.item(k)).collect();
    if ids != reference::identities(&peer.log, &v) {
        return Err(format!("{}: identities differ", peer.name));
    }
    Ok(())
}

/// Delivers `msg` to peer `to` from peer `from`: answers a summary, applies
/// a chunk. Returns the reply, if any.
fn deliver(peers: &mut [Peer], from: usize, to: usize, msg: &Msg) -> Option<(usize, usize, Msg)> {
    let peer = &mut peers[to];
    // Damaged on the way: refused, and nothing changes.
    let Ok(msg) = Message::decode(msg) else {
        return None;
    };
    match msg {
        Message::Summary(theirs) => {
            let missing = peer.log.missing(&theirs);
            if missing.is_empty() {
                return None;
            }
            let mut chunk = Vec::new();
            peer.log.encode_events(&missing, &mut chunk);
            Some((to, from, Message::Events(chunk).encode()))
        }
        Message::Events(chunk) => {
            let before = peer.version();
            let len = peer.log.len();
            match peer.log.decode_chunk(&chunk) {
                Ok(_) => catch_up(peer, &before),
                Err(_) => assert_eq!(peer.log.len(), len, "a refused chunk adds nothing"),
            }
            None
        }
    }
}

fn run(n: usize, acts: &[Act]) -> Result<(), String> {
    let mut peers: Vec<Peer> = (0..n)
        .map(|i| Peer {
            name: format!("p{i}"),
            log: OpLog::new(),
            doc: Vec::new(),
            ids: Identities::new(),
        })
        .collect();
    // In flight: (from, to, message).
    let mut queue: Vec<(usize, usize, Msg)> = Vec::new();
    let mut next = 0u32;
    for (step, act) in acts.iter().enumerate() {
        let touched: Vec<usize> = match act.clone() {
            Act::Insert { p, at, n } => {
                let peer = &mut peers[p];
                let agent = peer.log.agent(&peer.name.clone());
                let pos = frac(peer.doc.len(), at);
                let items: Vec<char> = (0..n as u32)
                    .map(|k| char::from(b'a' + ((next + k) % 26) as u8))
                    .collect();
                next += n as u32;
                let v = peer.version();
                peer.log.add_insert(agent, &v, pos, items.clone());
                peer.ids.insert(pos, peer.log.len() - n, n);
                peer.doc.splice(pos..pos, items);
                vec![p]
            }
            Act::Delete { p, a, b } => {
                let peer = &mut peers[p];
                let (a, b) = (frac(peer.doc.len(), a), frac(peer.doc.len(), b));
                let range = a.min(b)..a.max(b);
                if !range.is_empty() {
                    let agent = peer.log.agent(&peer.name.clone());
                    let v = peer.version();
                    peer.log.add_delete(agent, &v, range.clone());
                    peer.ids
                        .delete(range.start, peer.log.len() - range.len(), range.len());
                    peer.doc.drain(range);
                }
                vec![p]
            }
            Act::Undo { p, pick } => {
                let peer = &mut peers[p];
                let deletes: Vec<Lv> = (0..peer.log.len())
                    .filter(|&lv| matches!(peer.log.event(lv).op, Op::Delete { .. }))
                    .collect();
                if !deletes.is_empty() {
                    let d = deletes[frac(deletes.len() - 1, pick)];
                    let agent = peer.log.agent(&peer.name.clone());
                    let before = peer.version();
                    peer.log.add_undelete(agent, &before, [d]);
                    catch_up(peer, &before);
                }
                vec![p]
            }
            Act::Paste { p, a, b, at } => {
                let peer = &mut peers[p];
                let (a, b) = (frac(peer.doc.len(), a), frac(peer.doc.len(), b));
                let range = a.min(b)..a.max(b);
                if !range.is_empty() {
                    let agent = peer.log.agent(&peer.name.clone());
                    let sources: Vec<Lv> = range.clone().map(|k| peer.ids.item(k)).collect();
                    let pos = frac(peer.doc.len(), at);
                    let v = peer.version();
                    peer.log.add_paste(agent, &v, pos, &sources);
                    let n = range.len();
                    peer.ids.insert(pos, peer.log.len() - n, n);
                    let items: Vec<char> = peer.doc[range].to_vec();
                    peer.doc.splice(pos..pos, items);
                }
                vec![p]
            }
            Act::Gossip { p, to } => {
                if p != to {
                    queue.push((p, to, Message::Summary(peers[p].log.summary()).encode()));
                }
                vec![]
            }
            Act::Push { p, to, k } => {
                let log = &peers[p].log;
                if p != to && !log.is_empty() {
                    let mut chunk = Vec::new();
                    log.encode_chunk(log.len().saturating_sub(k)..log.len(), &mut chunk);
                    queue.push((p, to, Message::Events(chunk).encode()));
                }
                vec![]
            }
            Act::Deliver { pick } => {
                if queue.is_empty() {
                    vec![]
                } else {
                    let (from, to, msg) = queue.remove(frac(queue.len() - 1, pick));
                    if let Some(reply) = deliver(&mut peers, from, to, &msg) {
                        queue.push(reply);
                    }
                    vec![to]
                }
            }
            Act::Drop { pick } => {
                if !queue.is_empty() {
                    queue.remove(frac(queue.len() - 1, pick));
                }
                vec![]
            }
            Act::Duplicate { pick } => {
                if !queue.is_empty() {
                    let m = queue[frac(queue.len() - 1, pick)].clone();
                    queue.push(m);
                }
                vec![]
            }
            Act::Garbage {
                p,
                bytes,
                from_queue,
            } => {
                // A real message with a byte changed, or bytes from nowhere.
                let msg = match from_queue.filter(|_| !queue.is_empty()) {
                    Some(pick) => {
                        let (_, _, m) = &queue[frac(queue.len() - 1, pick)];
                        let mut b = m.clone();
                        if !b.is_empty() {
                            let i = bytes.first().copied().unwrap_or(0) as usize % b.len();
                            b[i] ^= 0x5a;
                        }
                        b
                    }
                    None => bytes,
                };
                let reply = deliver(&mut peers, (p + 1) % n, p, &msg);
                drop(reply);
                vec![p]
            }
        };
        for p in touched {
            verify(&peers[p]).map_err(|e| format!("step {step}, {act:?}: {e}"))?;
        }
    }

    // The network heals: every message in flight arrives, then everyone
    // gossips with everyone until nothing new moves.
    for (from, to, msg) in std::mem::take(&mut queue) {
        if let Some(reply) = deliver(&mut peers, from, to, &msg) {
            queue.push(reply);
        }
    }
    for round in 0.. {
        let before: Vec<usize> = peers.iter().map(|p| p.log.len()).collect();
        for (p, peer) in peers.iter().enumerate() {
            for to in 0..n {
                if p != to {
                    queue.push((p, to, Message::Summary(peer.log.summary()).encode()));
                }
            }
        }
        while let Some((from, to, msg)) = queue.pop() {
            if let Some(reply) = deliver(&mut peers, from, to, &msg) {
                queue.push(reply);
            }
        }
        let after: Vec<usize> = peers.iter().map(|p| p.log.len()).collect();
        if after == before {
            break;
        }
        if round > 10 {
            return Err("still syncing after 10 rounds".to_string());
        }
    }
    for peer in &peers {
        verify(peer).map_err(|e| format!("healed: {e}"))?;
        if peer.log.summary() != peers[0].log.summary() {
            return Err(format!(
                "{} has different events from {}",
                peer.name, peers[0].name
            ));
        }
        if lineage(&peer.log) != lineage(&peers[0].log) {
            return Err(format!(
                "{} has different lineage from {}",
                peer.name, peers[0].name
            ));
        }
        if peer.doc != peers[0].doc {
            return Err(format!(
                "diverged: {} has {:?}, {} has {:?}",
                peer.name, peer.doc, peers[0].name, peers[0].doc
            ));
        }
    }
    Ok(())
}

fn acts(n: usize) -> impl Strategy<Value = Vec<Act>> {
    let p = 0..n;
    prop::collection::vec(
        prop_oneof![
            4 => (p.clone(), any::<u16>(), 1usize..4).prop_map(|(p, at, n)| Act::Insert { p, at, n }),
            2 => (p.clone(), any::<u16>(), any::<u16>()).prop_map(|(p, a, b)| Act::Delete { p, a, b }),
            1 => (p.clone(), any::<u16>()).prop_map(|(p, pick)| Act::Undo { p, pick }),
            1 => (p.clone(), any::<u16>(), any::<u16>(), any::<u16>())
                .prop_map(|(p, a, b, at)| Act::Paste { p, a, b, at }),
            3 => (p.clone(), p.clone()).prop_map(|(p, to)| Act::Gossip { p, to }),
            2 => (p.clone(), p.clone(), 1usize..6).prop_map(|(p, to, k)| Act::Push { p, to, k }),
            5 => any::<u16>().prop_map(|pick| Act::Deliver { pick }),
            1 => any::<u16>().prop_map(|pick| Act::Drop { pick }),
            1 => any::<u16>().prop_map(|pick| Act::Duplicate { pick }),
            1 => (p, prop::collection::vec(any::<u8>(), 0..24), prop::option::of(any::<u16>()))
                .prop_map(|(p, bytes, from_queue)| Act::Garbage { p, bytes, from_queue }),
        ],
        1..80,
    )
}

proptest! {
    #[test]
    fn three_peers_on_a_bad_network(acts in acts(3)) {
        run(3, &acts).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn four_peers_on_a_bad_network(acts in acts(4)) {
        run(4, &acts).map_err(TestCaseError::fail)?;
    }
}

/// A chunk claiming an agent's events with a gap (an honest agent's each
/// include the one before) is refused, so a summary can always say exactly
/// what a replica has.
#[test]
fn a_gap_in_an_agents_events_is_refused() {
    let mut log: OpLog<char> = OpLog::new();
    let alice = log.agent("alice");
    log.add_event(alice, 0, &[], Op::Insert { pos: 0, item: 'a' });
    log.add_event(alice, 2, &[], Op::Insert { pos: 0, item: 'b' });
    let mut chunk = Vec::new();
    log.encode_chunk(0..2, &mut chunk);
    let mut other: OpLog<char> = OpLog::new();
    assert!(other.decode_chunk(&chunk).is_err());
    assert!(other.is_empty());
}

proptest! {
    /// What `missing` gives is exactly what the other side lacks: every
    /// event at or past its count of that agent, and nothing else.
    #[test]
    fn missing_is_exact(acts in acts(3), cut in prop::collection::vec(any::<u16>(), 3)) {
        // A log with several agents' events, from a run of the network.
        let mut log: OpLog<char> = OpLog::new();
        let names = ["p0", "p1", "p2"];
        let mut versions: Vec<Vec<Lv>> = vec![Vec::new(); 3];
        for act in &acts {
            if let Act::Insert { p, at, n } = *act {
                let agent = log.agent(names[p]);
                let v = log.union(&versions[p], &[]);
                let len = log.checkout(&v).len();
                versions[p] = log.add_insert(agent, &v, frac(len, at), (0..n).map(|_| 'x'));
            }
        }
        // Another replica's summary: a prefix of each agent's events.
        let mine = log.summary();
        let mut kept = Vec::new();
        for (k, name) in names.iter().enumerate() {
            let have = mine.count(name);
            let take = (have as u128 * cut[k] as u128 / u16::MAX as u128) as u64;
            kept.push((*name, take));
        }
        let theirs = summary_of(&kept);
        let spans = log.missing(&theirs);
        let sent: Vec<Lv> = spans.iter().flat_map(|s| s.clone()).collect();
        let want: Vec<Lv> = (0..log.len())
            .filter(|&lv| {
                let e = log.event(lv);
                e.seq >= theirs.count(log.agent_name(e.agent))
            })
            .collect();
        prop_assert_eq!(sent, want);
    }
}

/// A summary saying how many events of each agent: built by decoding, the
/// way one arrives.
fn summary_of(counts: &[(&str, u64)]) -> mamre_egg_walker::sync::Summary {
    use mamre::codec::{Codec, put_str, put_usize, put_varint};
    let mut bytes = Vec::new();
    let counts: Vec<_> = counts.iter().filter(|(_, n)| *n > 0).collect();
    put_usize(&mut bytes, counts.len());
    for (name, n) in counts {
        put_str(&mut bytes, name);
        put_varint(&mut bytes, *n);
    }
    mamre_egg_walker::sync::Summary::decode(&mut &bytes[..]).unwrap()
}

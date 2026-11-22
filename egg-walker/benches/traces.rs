//! The Eg-walker paper's editing traces, replayed: each loaded into both
//! diamond-types and egg-walker, checked to check out to exactly the same
//! text, and timed.
//!
//! Fetch the traces first: `egg-walker/benchmark_data/fetch.sh`. Then
//! `cargo bench -p mamre-egg-walker --bench traces`.

use std::path::Path;
use std::time::{Duration, Instant};

use diamond_types::list::OpLog as Diamond;
use diamond_types::list::operation::OpKind;
use mamre_egg_walker::{Change, Lv, Op, OpLog};
use mamre_rope::Rope;

const TRACES: [(&str, &str); 7] = [
    ("S1", "sequential"),
    ("S2", "sequential"),
    ("S3", "sequential"),
    ("C1", "concurrent"),
    ("C2", "concurrent"),
    ("A1", "asynchronous"),
    ("A2", "asynchronous"),
];

/// diamond-types' history as egg-walker events, one per item, numbered the
/// same.
fn convert(dt: &Diamond) -> OpLog<char> {
    let n = dt.len();
    // Parents: a history entry's first event has its parents; the rest of
    // its span, the event before.
    let mut parents: Vec<Vec<Lv>> = vec![Vec::new(); n];
    for entry in dt.iter_history() {
        let span: std::ops::Range<usize> = entry.span.into();
        for t in span.clone() {
            parents[t] = if t == span.start {
                entry.parents.to_vec()
            } else {
                vec![t - 1]
            };
        }
    }
    // Who made each event, and their number for it.
    let mut ours: OpLog<char> = OpLog::new();
    let mut who = Vec::with_capacity(n);
    for span in dt.iter_mappings() {
        let agent = ours.agent(dt.get_agent_name(span.agent));
        let seqs: std::ops::Range<usize> = span.seq_range.into();
        for seq in seqs {
            who.push((agent, seq as u64));
        }
    }
    // What each event does: runs split back into single items. Forward
    // runs move along; reversed ones (backspacing) stay put or move back.
    let mut t = 0;
    for op in dt.iter() {
        let span: std::ops::Range<usize> = op.loc.span.into();
        let content: Vec<char> = op
            .content_as_str()
            .map(|c| c.chars().collect())
            .unwrap_or_default();
        // By index: a delete has a span but no content.
        #[allow(clippy::needless_range_loop)]
        for k in 0..span.len() {
            let event = match op.kind {
                OpKind::Ins if op.loc.fwd => Op::Insert {
                    pos: span.start + k,
                    item: content[k],
                },
                OpKind::Ins => Op::Insert {
                    pos: span.start,
                    item: content[k],
                },
                OpKind::Del if op.loc.fwd => Op::Delete { pos: span.start },
                OpKind::Del => Op::Delete {
                    pos: span.end - 1 - k,
                },
            };
            let (agent, seq) = who[t];
            let lv = ours.add_event(agent, seq, &parents[t], event);
            assert_eq!(lv, t, "numbering must line up");
            t += 1;
        }
    }
    assert_eq!(t, n);
    ours
}

/// The document at the tip, in a rope: one rope edit per change.
fn checkout(log: &OpLog<char>) -> Rope {
    let mut rope = Rope::new();
    let mut text = String::new();
    for change in log.changes(&[], log.version()) {
        match change {
            Change::Insert { pos, items, .. } => {
                text.clear();
                text.extend(items);
                let at = rope.char_to_offset(pos);
                rope.insert(at, &text);
            }
            Change::Delete { range } => {
                let range = rope.char_to_offset(range.start)..rope.char_to_offset(range.end);
                rope.remove(range);
            }
        }
    }
    rope
}

/// The fastest of a few runs of `f`.
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

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmark_data");
    println!(
        "{:<5}{:<14}{:>9}{:>7}{:>10}{:>14}{:>14}",
        "trace", "kind", "events", "runs", "chars", "egg-walker", "diamond-types"
    );
    let only = std::env::var("TRACE").ok();
    for (name, kind) in TRACES {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        let path = dir.join(format!("{name}.dt"));
        let Ok(bytes) = std::fs::read(&path) else {
            println!("{name}: missing; run benchmark_data/fetch.sh");
            continue;
        };
        let dt = Diamond::load_from(&bytes).expect("a diamond-types trace");
        let ours = convert(&dt);

        // Agreement first: the same text, exactly. diamond-types takes
        // minutes on some traces, so its one checkout here is its timing.
        let mine: String = checkout(&ours).chars().collect();
        let start = Instant::now();
        let theirs = dt.checkout_tip().content().to_string();
        let dt_time = start.elapsed();
        assert_eq!(
            mine, theirs,
            "{name}: egg-walker and diamond-types disagree"
        );

        let ours_time = best(3, || {
            std::hint::black_box(checkout(&ours));
        });
        if let Ok(n) = std::env::var("WALKS") {
            for _ in 0..n.parse::<usize>().unwrap() {
                std::hint::black_box(ours.changes(&[], ours.version()));
            }
            continue;
        }
        if std::env::var("SIZES").is_ok() {
            // The whole history as one chunk, against diamond-types' file
            // of the same trace; and back again, checked.
            let start = Instant::now();
            let mut chunk = Vec::new();
            ours.encode_chunk(0..ours.len(), &mut chunk);
            let encode = start.elapsed();
            let start = Instant::now();
            let mut back: OpLog<char> = OpLog::new();
            back.decode_chunk(&chunk).expect("our own chunk decodes");
            let decode = start.elapsed();
            assert_eq!(back.len(), ours.len());
            let again: String = checkout(&back).chars().collect();
            assert_eq!(again, mine, "{name}: a decoded log checks out the same");
            if let Ok(dir) = std::env::var("DUMP") {
                std::fs::write(Path::new(&dir).join(format!("{name}.chunk")), &chunk).unwrap();
            }
            eprintln!(
                "{name}: history {:>7} bytes ({:.2} per event), diamond-types file {:>7} bytes; encode {encode:.2?}, decode {decode:.2?}",
                chunk.len(),
                chunk.len() as f64 / ours.len() as f64,
                bytes.len()
            );
            continue;
        }
        if std::env::var("SPLIT").is_ok() {
            let start = Instant::now();
            let changes = ours.changes(&[], ours.version());
            let walk = start.elapsed();
            let start = Instant::now();
            std::hint::black_box(checkout(&ours));
            eprintln!(
                "{name}: walk {walk:.2?}, {} changes, checkout {:.2?}",
                changes.len(),
                start.elapsed()
            );
        }
        println!(
            "{:<5}{:<14}{:>9}{:>7}{:>10}{:>14.2?}{:>14.2?}",
            name,
            kind,
            ours.len(),
            ours.runs(),
            mine.chars().count(),
            ours_time,
            dt_time
        );
    }
}

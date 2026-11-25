//! Two replicas on two machines, syncing over TCP while both type.
//!
//! Each side makes `EDITS` random edits (typing, deleting, now and then
//! moving a stretch), a few milliseconds apart, keeping its document and
//! books as a real replica does: local edits made on the document and
//! [`record`](Accountant::record)ed, the other side's [`merge`](Accountant::merge)d
//! and applied. All the while each sends its summary, and answers the
//! other's with what it's missing.
//!
//! When both are done and have the same events, each checks its document
//! against a fresh checkout (and with `REF=1`, the reference's: slow), and
//! they swap a hash of the document: it must match.
//!
//! ```text
//! cargo run --release -p mamre-egg-walker --example peer -- listen 100.70.81.83:7171 cocchi
//! cargo run --release -p mamre-egg-walker --example peer -- connect 100.70.81.83:7171 lillet
//! ```
//!
//! `EDITS` (default 2000), `MOVES` (percent of edits, default 3) and `SEED`
//! set the run.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mamre_egg_walker::sync::{Message, Summary};
use mamre_egg_walker::{Accountant, OpLog, copies};

/// What goes on the wire: a sync message; "I'm done typing"; and, once
/// both are done and have the same events, the document's hash.
const SYNC: u8 = 0;
const DONE: u8 = 1;
const HASH: u8 = 2;

fn send(out: &mut TcpStream, kind: u8, body: &[u8]) {
    let mut frame = Vec::with_capacity(body.len() + 5);
    frame.push(kind);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(body);
    out.write_all(&frame).expect("the other side hung up");
}

fn reader(stream: TcpStream, to: mpsc::Sender<(u8, Vec<u8>)>) {
    let mut input = BufReader::new(stream);
    loop {
        let mut head = [0u8; 5];
        if input.read_exact(&mut head).is_err() {
            return;
        }
        let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len];
        if input.read_exact(&mut body).is_err() || to.send((head[0], body)).is_err() {
            return;
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

fn hash(doc: &[char]) -> u64 {
    let mut h = DefaultHasher::new();
    doc.hash(&mut h);
    h.finish()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (mode, addr, name) = match &args[1..] {
        [m, a, n] => (m.as_str(), a.as_str(), n.as_str()),
        _ => panic!("usage: peer listen|connect ADDR NAME"),
    };
    let edits: usize = std::env::var("EDITS").map_or(2000, |s| s.parse().unwrap());
    // Percent of edits that are moves.
    let move_pct: usize = std::env::var("MOVES").map_or(3, |s| s.parse().unwrap());
    let seed: u64 = std::env::var("SEED").map_or(1, |s| s.parse().unwrap());

    let stream = match mode {
        "listen" => {
            let listener = TcpListener::bind(addr).expect("bind");
            println!("{name}: listening on {addr}");
            listener.accept().expect("accept").0
        }
        "connect" => TcpStream::connect(addr).expect("connect"),
        _ => panic!("listen or connect"),
    };
    stream.set_nodelay(true).ok();
    println!("{name}: connected to {}", stream.peer_addr().unwrap());
    let mut out = stream.try_clone().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || reader(stream, tx));

    let mut log = OpLog::<char>::new();
    let me = log.agent(name);
    let mut books = Accountant::new();
    let mut doc: Vec<char> = Vec::new();
    let mut rng =
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ hash(&name.chars().collect::<Vec<_>>()));
    let letters: Vec<char> = name.chars().filter(|c| c.is_alphabetic()).collect();

    let start = Instant::now();
    let (mut made, mut moves, mut merges, mut chunks, mut bytes_in) = (0, 0, 0, 0, 0usize);
    let mut merge_time = Duration::ZERO;
    let mut worst = Duration::ZERO;
    let (mut window_time, mut window_merges) = (Duration::ZERO, 0);
    let mut theirs: Option<Summary> = None;
    let mut their_done = false;
    let mut sent_done = false;
    let mut their_hash: Option<u64> = None;
    let mut sent_hash = false;
    let mut last_summary = Instant::now() - Duration::from_secs(1);

    loop {
        // A local edit, now and then.
        if made < edits && rng.next(3) == 0 {
            let len = doc.len();
            let roll = rng.next(100);
            if len > 20 && roll < move_pct {
                // Move a stretch somewhere outside it, through the books.
                let n = 1 + rng.next(10.min(len / 2));
                let from = rng.next(len - n + 1);
                let mut to = rng.next(len + 1);
                if to > from && to < from + n {
                    to = from;
                }
                if to != from && to != from + n {
                    let v = log.add_move(me, books.version(), from..from + n, to);
                    for change in books.merge(&log, &v) {
                        change.apply(&mut doc);
                    }
                    moves += 1;
                }
            } else if len > 0 && roll < move_pct + 27 {
                let a = rng.next(len);
                let b = (a + 1 + rng.next(4)).min(len);
                doc.drain(a..b);
                let v = log.add_delete(me, books.version(), a..b);
                books.record(&log, &v);
            } else {
                let at = rng.next(len + 1);
                let n = 1 + rng.next(5);
                let text: Vec<char> = (0..n).map(|_| letters[rng.next(letters.len())]).collect();
                doc.splice(at..at, text.iter().copied());
                let v = log.add_insert(me, books.version(), at, text);
                books.record(&log, &v);
            }
            made += 1;
            if made % 250 == 0 {
                println!(
                    "{name}: {made} edits, {} events, {merges} merges so far, {:.2} ms each since the last line",
                    log.len(),
                    (merge_time - window_time).as_secs_f64() * 1e3
                        / (merges - window_merges).max(1) as f64
                );
                window_time = merge_time;
                window_merges = merges;
            }
        }

        // Tell the other side what we have, every few milliseconds.
        if last_summary.elapsed() >= Duration::from_millis(5) {
            send(&mut out, SYNC, &Message::Summary(log.summary()).encode());
            last_summary = Instant::now();
        }

        // Done typing: say so. Both done, and we have just what they
        // have (nothing more can come): send our hash.
        if made == edits && !sent_done {
            send(&mut out, DONE, &[]);
            sent_done = true;
        }
        if sent_done && their_done && !sent_hash && theirs.as_ref() == Some(&log.summary()) {
            send(&mut out, HASH, &hash(&doc).to_le_bytes());
            sent_hash = true;
        }
        if sent_hash && their_hash.is_some() {
            break;
        }

        // Whatever has come in.
        let wait = if made < edits {
            Duration::from_millis(2)
        } else {
            Duration::from_millis(5)
        };
        let Ok((kind, body)) = rx.recv_timeout(wait) else {
            continue;
        };
        bytes_in += body.len() + 5;
        if kind == DONE {
            // A summary from before they stopped may predate their last
            // edits: only one sent after this counts.
            their_done = true;
            theirs = None;
            continue;
        }
        if kind == HASH {
            their_hash = Some(u64::from_le_bytes(body[..8].try_into().unwrap()));
            continue;
        }
        match Message::decode(&body).expect("a damaged message over TCP") {
            Message::Summary(s) => {
                let missing = log.missing(&s);
                theirs = Some(s);
                if !missing.is_empty() {
                    let mut chunk = Vec::new();
                    log.encode_events(&missing, &mut chunk);
                    send(&mut out, SYNC, &Message::Events(chunk).encode());
                }
            }
            Message::Events(chunk) => {
                chunks += 1;
                if log.decode_chunk(&chunk).is_err() {
                    // Arrived before what it needs: it comes again.
                    continue;
                }
                let t = Instant::now();
                let v = log.version().to_vec();
                let changes = books.merge(&log, &v);
                for change in changes {
                    change.apply(&mut doc);
                }
                let took = t.elapsed();
                merge_time += took;
                worst = worst.max(took);
                merges += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    let mine = hash(&doc);
    let theirs = their_hash.unwrap();
    let v = log.version().to_vec();
    let fresh = log.checkout(&v) == doc;
    // The reference is slow (cubic): only when asked, with REF=1.
    let t = Instant::now();
    let refd = std::env::var("REF").is_err() || copies::checkout(&log, &v) == doc;
    let ref_time = t.elapsed();
    println!(
        "{name}: {made} edits ({moves} moves), {} events in the log, {} chars in the document",
        log.len(),
        doc.len()
    );
    println!(
        "{name}: {chunks} chunks in ({} KiB), {merges} merges, {:.2} ms each on average, worst {:.2} ms",
        bytes_in / 1024,
        merge_time.as_secs_f64() * 1e3 / merges.max(1) as f64,
        worst.as_secs_f64() * 1e3
    );
    println!(
        "{name}: {:.1} s; fresh checkout matches: {fresh}; reference matches: {refd} ({:.0} ms)",
        elapsed.as_secs_f64(),
        ref_time.as_secs_f64() * 1e3
    );
    let same = mine == theirs;
    println!(
        "{name}: hash {mine:016x}, theirs {theirs:016x}: {}",
        if same && fresh && refd {
            "CONVERGED"
        } else {
            "DIVERGED"
        }
    );
    std::process::exit(if same && fresh && refd { 0 } else { 1 });
}

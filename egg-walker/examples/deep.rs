//! Scratch: moves nested `N` deep. Each round types a character at the
//! end, then moves everything before it to after it: that block holds
//! the last one, so they nest.
use mamre_egg_walker::{Accountant, OpLog};
use std::time::Instant;
fn main() {
    let n: usize = std::env::var("N").map_or(1000, |s| s.parse().unwrap());
    let mut log = OpLog::<char>::new();
    let a = log.agent("a");
    let mut books = Accountant::new();
    let mut doc: Vec<char> = Vec::new();
    let v = log.add_insert(a, books.version(), 0, ['x']);
    doc.push('x');
    books.record(&log, &v);
    let t = Instant::now();
    for _ in 0..n {
        let len = doc.len();
        let v = log.add_insert(a, books.version(), len, ['y']);
        doc.push('y');
        books.record(&log, &v);
        let len = doc.len();
        let v = log.add_move(a, books.version(), 0..len - 1, len);
        for c in books.merge(&log, &v) {
            c.apply(&mut doc);
        }
    }
    println!("built {n} deep in {:?}", t.elapsed());
    // A second writer, typing concurrently with all of it.
    let b = log.agent("b");
    let vb = log.add_insert(b, &v, 0, ['z']);
    let all = log.union(log.version(), &vb);
    let t = Instant::now();
    let merged = log.checkout(&all);
    println!("merge-checkout {:?}, {} chars", t.elapsed(), merged.len());
    if std::env::var("REF").is_ok() {
        let t = Instant::now();
        let r = mamre_egg_walker::reference::checkout(&log, &all);
        println!("reference {:?}, matches: {}", t.elapsed(), r == merged);
    }
    let t = Instant::now();
    let fresh = log.checkout(books.version());
    println!("checkout {:?}, matches: {}", t.elapsed(), fresh == doc);
}

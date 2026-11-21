//! Rope edits at random places in a 170k-char document, at a few chunk
//! sizes: the cost of an edit, a slice, and a seek by char.
//!
//! `cargo bench -p mamre-rope --bench edits`

use std::time::{Duration, Instant};

use mamre_rope::{Plain, Rope};

const EDITS: u32 = 60_000;

fn main() {
    println!(
        "{:>6}{:>12}{:>12}{:>12}{:>12}{:>14}",
        "chunk", "insert", "remove", "prefix", "suffix", "char seek"
    );
    run::<512>();
    run::<128>();
    run::<32>();
}

fn run<const N: usize>() {
    let text: String = (0..170_000)
        .map(|i| if i % 60 == 59 { '\n' } else { 'a' })
        .collect();
    let mut rope: Rope<Plain, N> = Rope::new();
    rope.push_str(&text);
    // xorshift: the same places every run.
    let mut x: u64 = 1;
    let mut rand = move |n: usize| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x as usize) % n
    };
    let each = |f: &mut dyn FnMut()| -> Duration {
        let start = Instant::now();
        for _ in 0..EDITS {
            f();
        }
        start.elapsed() / EDITS
    };
    let insert = each(&mut || {
        let at = rand(rope.len());
        rope.insert(at, "b");
    });
    let remove = each(&mut || {
        let at = rand(rope.len());
        rope.remove(at..at + 1);
    });
    let prefix = each(&mut || {
        std::hint::black_box(rope.slice(0..rand(rope.len())));
    });
    let suffix = each(&mut || {
        std::hint::black_box(rope.slice(rand(rope.len())..rope.len()));
    });
    let seek = each(&mut || {
        std::hint::black_box(rope.char_to_offset(rand(rope.summary().chars)));
    });
    println!("{N:>6}{insert:>12.2?}{remove:>12.2?}{prefix:>12.2?}{suffix:>12.2?}{seek:>14.2?}");
}

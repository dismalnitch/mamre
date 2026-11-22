//! Sections on a book-sized document (40 chapters of 25 sections, about
//! 4 MB of Markdown): moving one, and finding them, should cost the same
//! whatever the size, and far less than a frame.
//!
//! `cargo bench -p mamre-rope --bench sections`

use std::time::{Duration, Instant};

use mamre_rope::{Address, Markdown, Rope};

fn book() -> String {
    let mut out = String::new();
    let para = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. ";
    for c in 1..=40 {
        out.push_str(&format!("# Chapter {c}\n\n"));
        for s in 1..=25 {
            out.push_str(&format!("## Section {c}.{s}\n\n"));
            for _ in 0..4 {
                out.push_str(&para.repeat(8));
                out.push_str("\n\n");
            }
        }
    }
    out
}

fn time(runs: u32, mut f: impl FnMut()) -> Duration {
    let start = Instant::now();
    for _ in 0..runs {
        f();
    }
    start.elapsed() / runs
}

fn main() {
    let text = book();
    let start = Instant::now();
    let rope: Rope<Markdown> = Rope::from(text.as_str());
    let build = start.elapsed();
    println!(
        "book: {:.1} MB, {} sections; built in {build:.2?}",
        text.len() as f64 / 1e6,
        rope.count(mamre_rope::Unit::Section)
    );

    let chapter = |n: u32| Address::new(&[n]).unwrap();
    let section = |c: u32, s: u32| Address::new(&[c, s]).unwrap();

    let find = time(1000, || {
        std::hint::black_box(rope.section_range(chapter(20)));
    });
    let address = time(1000, || {
        std::hint::black_box(rope.address_at(text.len() / 2));
    });
    // Chapter 10 to the end, and back, on fresh copies (clones are O(1)).
    let mut moves = 0u32;
    let chapter_move = time(1000, || {
        let mut r = rope.clone();
        let to = r.len();
        std::hint::black_box(r.move_section(chapter(10), to).unwrap());
        moves += 1;
    });
    let section_move = time(1000, || {
        let mut r = rope.clone();
        let to = r.heading_at(section(30, 1)).unwrap();
        std::hint::black_box(r.move_section(section(3, 7), to).unwrap());
    });
    let rebuild = time(10, || {
        std::hint::black_box(Rope::<Markdown>::from(text.as_str()));
    });
    println!("section_range: {find:.2?}   address_at: {address:.2?}");
    println!("move a chapter (~100 KB) to the end: {chapter_move:.2?}");
    println!("move a section (~4 KB) across the book: {section_move:.2?}");
    println!("for scale, rebuilding the whole rope: {rebuild:.2?}");
    let _ = moves;
}

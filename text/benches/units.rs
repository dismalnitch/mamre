//! Counts and seeks by unit through mamre-text (#31), on the book the
//! rope's section bench uses (40 chapters of 25 sections, about 4 MB),
//! with every heading marked as a section.
//!
//! `cargo bench -p mamre-text --bench units`

use std::hint::black_box;
use std::time::{Duration, Instant};

use mamre_text::{Text, Unit, Units};

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

fn time(runs: u32, mut f: impl FnMut(u32)) -> Duration {
    let start = Instant::now();
    for i in 0..runs {
        f(i);
    }
    start.elapsed() / runs
}

fn main() {
    let text = book();
    let start = Instant::now();
    let mut t: Text<Units> = Text::with_measure("bench");
    t.insert(0, &text).unwrap();
    // Every heading, marked at its level.
    let mut chapters = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        let level = line.bytes().take_while(|&b| b == b'#').count();
        if level > 0 {
            let (id, _) = t.mark_section(at, level as u8).unwrap();
            if level == 1 {
                chapters.push(id);
            }
        }
        at += line.len();
    }
    println!(
        "book: {:.1} MB, {} words, {} sentences, {} paragraphs, {} sections; built in {:.2?}",
        text.len() as f64 / 1e6,
        t.count(Unit::Word),
        t.count(Unit::Sentence),
        t.count(Unit::Paragraph),
        t.sections().len(),
        start.elapsed()
    );
    let len = t.len();
    // Spread-out positions, on char boundaries (the book is ASCII).
    let pos = |i: u32| (i as usize * 7919 * 131) % len;
    for unit in [Unit::Word, Unit::Sentence, Unit::Paragraph] {
        let n = t.count(unit);
        let seek = time(10_000, |i| {
            black_box(t.unit_start((i as usize * 7919) % n, unit));
        });
        let before = time(10_000, |i| {
            black_box(t.units_before(pos(i), unit).unwrap());
        });
        let next3 = time(10_000, |i| {
            black_box(t.next_start(pos(i), unit, 3).unwrap());
        });
        println!(
            "{unit:?}: nth start {seek:.2?}, units before {before:.2?}, 3rd start after {next3:.2?}"
        );
    }
    let chapter = time(1_000, |i| {
        black_box(t.section_count(chapters[i as usize % chapters.len()], Unit::Word));
    });
    let all = time(10, |_| {
        black_box(t.section_counts(Unit::Word));
    });
    println!(
        "one chapter's word count {chapter:.2?}; every section's ({} of them) {all:.2?}",
        t.sections().len()
    );
}

//! The rope against a `String` doing the same job: random edits applied to
//! both, with everything checked after each one. Run with tiny chunks, so
//! every edit cuts and joins chunks and trees are several levels tall, and
//! with the default chunk size.

use mamre_rope::{Markdown, Plain, Point, Prose, Rope, TextSummary, Unit};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Insert(u16, String),
    Remove(u16, u16),
    Replace(u16, u16, String),
    Push(String),
    Append(String),
    /// Slice out a range, and check the slice as a rope of its own.
    Slice(u16, u16),
}

/// A char boundary in `text`, about this fraction of the way through.
fn boundary(text: &str, f: u16) -> usize {
    let chars = text.chars().count();
    let k = chars * f as usize / u16::MAX as usize;
    text.char_indices().nth(k).map_or(text.len(), |(i, _)| i)
}

fn range(text: &str, a: u16, b: u16) -> std::ops::Range<usize> {
    let (a, b) = (boundary(text, a), boundary(text, b));
    a.min(b)..a.max(b)
}

/// The row and column of `offset`, the slow way.
fn point_of(text: &str, offset: usize) -> Point {
    Point::of(&text[..offset])
}

/// The offset of `point`, the slow way, with the rope's rules for points
/// that don't sit on text: past a row's end is its end, inside a char is the
/// next char, past the last row is the end.
fn offset_of(text: &str, point: Point) -> usize {
    let mut start = 0;
    for (row, line) in text.split('\n').enumerate() {
        if row == point.row {
            let mut col = point.column.min(line.len());
            while !line.is_char_boundary(col) {
                col += 1;
            }
            return start + col;
        }
        start += line.len() + 1;
    }
    text.len()
}

/// Everything that must hold between a rope and the string.
fn verify<const N: usize>(rope: &Rope<Markdown, N>, text: &str) -> Result<(), String> {
    rope.check()?;
    if rope.to_string() != text {
        return Err(format!("text is {:?}, want {text:?}", rope.to_string()));
    }
    if *rope.summary() != TextSummary::of(text) {
        return Err(format!(
            "summary is {:?}, want {:?}",
            rope.summary(),
            TextSummary::<Markdown>::of(text)
        ));
    }
    if rope.chars().ne(text.chars()) {
        return Err("chars() differ".into());
    }
    // Chars and points both ways, at every char boundary of a small text.
    if text.len() <= 300 {
        for (chars, (offset, _)) in text.char_indices().chain([(text.len(), ' ')]).enumerate() {
            let got = rope.offset_to_char(offset);
            if got != chars {
                return Err(format!("offset_to_char({offset}) is {got}, want {chars}"));
            }
            let got = rope.char_to_offset(chars);
            if got != offset {
                return Err(format!("char_to_offset({chars}) is {got}, want {offset}"));
            }
            let want = point_of(text, offset);
            let got = rope.offset_to_point(offset);
            if got != want {
                return Err(format!(
                    "offset_to_point({offset}) is {got:?}, want {want:?}"
                ));
            }
            let back = rope.point_to_offset(want);
            if back != offset {
                return Err(format!(
                    "point_to_offset({want:?}) is {back}, want {offset}"
                ));
            }
        }
        // And points that don't sit on text: past row ends, inside chars,
        // past the last row.
        let rows = text.matches('\n').count();
        for row in 0..=rows + 1 {
            for column in [1, 2, 5, 40] {
                let point = Point::new(row, column);
                let (got, want) = (rope.point_to_offset(point), offset_of(text, point));
                if got != want {
                    return Err(format!("point_to_offset({point:?}) is {got}, want {want}"));
                }
            }
        }
    }
    verify_units(rope, text)
}

const UNITS: [Unit; 6] = [
    Unit::Word,
    Unit::VimWord,
    Unit::Sentence,
    Unit::Paragraph,
    Unit::VimParagraph,
    Unit::Section,
];

/// Every unit, against the slow one-pass `Unit::ranges`: the count and
/// every unit's range, and for a small text, which units come before and
/// hold every char boundary.
fn verify_units<const N: usize>(rope: &Rope<Markdown, N>, text: &str) -> Result<(), String> {
    for unit in UNITS {
        let want = unit.ranges(text);
        if rope.count(unit) != want.len() {
            return Err(format!(
                "{unit:?}: count is {}, want {}",
                rope.count(unit),
                want.len()
            ));
        }
        // Every unit of a short text; a stride through a long one, and
        // always the last.
        let stride = (want.len() / 50).max(1);
        let picks = (0..want.len())
            .step_by(stride)
            .chain(want.len().checked_sub(1));
        for n in picks {
            let range = &want[n];
            let got = rope.unit_range(n, unit);
            if got.as_ref() != Some(range) {
                return Err(format!(
                    "{unit:?}: unit_range({n}) is {got:?}, want {range:?}"
                ));
            }
        }
        if let Some(extra) = rope.unit_range(want.len(), unit) {
            return Err(format!(
                "{unit:?}: unit_range({}) is {extra:?}, want none",
                want.len()
            ));
        }
        // Every boundary of a short text; a stride through a long one.
        let stride = (text.len() / 60).max(1);
        {
            let offsets = text.char_indices().map(|(i, _)| i).chain([text.len()]);
            for offset in offsets.step_by(stride) {
                let before = want.iter().filter(|r| r.start < offset).count();
                let got = rope.units_before(offset, unit);
                if got != before {
                    return Err(format!(
                        "{unit:?}: units_before({offset}) is {got}, want {before}"
                    ));
                }
                let at = want.iter().position(|r| r.contains(&offset));
                let got = rope.unit_at(offset, unit);
                if got != at {
                    return Err(format!(
                        "{unit:?}: unit_at({offset}) is {got:?}, want {at:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

fn run<const N: usize>(start: &str, ops: &[Op]) -> Result<(), String> {
    let mut rope = Rope::<Markdown, N>::from(start);
    let mut text = start.to_string();
    verify(&rope, &text).map_err(|e| format!("after building: {e}"))?;
    for (step, op) in ops.iter().enumerate() {
        let copy = rope.clone();
        let copy_text = text.clone();
        let at = |e: String| format!("step {step}, {op:?}: {e}");
        match op {
            Op::Insert(f, s) => {
                let i = boundary(&text, *f);
                rope.insert(i, s);
                text.insert_str(i, s);
            }
            Op::Remove(a, b) => {
                let r = range(&text, *a, *b);
                rope.remove(r.clone());
                text.replace_range(r, "");
            }
            Op::Replace(a, b, s) => {
                let r = range(&text, *a, *b);
                rope.replace(r.clone(), s);
                text.replace_range(r, s);
            }
            Op::Push(s) => {
                rope.push_str(s);
                text.push_str(s);
            }
            Op::Append(s) => {
                rope.append(Rope::from(s.as_str()));
                text.push_str(s);
            }
            Op::Slice(a, b) => {
                let r = range(&text, *a, *b);
                let piece = rope.slice(r.clone());
                verify(&piece, &text[r]).map_err(|e| at(format!("the slice: {e}")))?;
            }
        }
        verify(&rope, &text).map_err(at)?;
        verify(&copy, &copy_text).map_err(|e| at(format!("a copy made before it changed: {e}")))?;
    }
    Ok(())
}

fn text(max: usize) -> impl Strategy<Value = String> {
    // Word characters, punctuation that ends sentences and closes them, and
    // plenty of whitespace and newlines, so every unit has boundaries.
    let pool = vec![
        'a', 'b', 'c', '_', ' ', ' ', '\n', '\n', '\t', '\r', '#', '#', '.', '!', '"', ')', 'é',
        '日', '🦀',
    ];
    prop::collection::vec(prop::sample::select(pool), 0..=max)
        .prop_map(|cs| cs.into_iter().collect())
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (any::<u16>(), text(40)).prop_map(|(f, s)| Op::Insert(f, s)),
        2 => (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Op::Remove(a, b)),
        3 => (any::<u16>(), any::<u16>(), text(40)).prop_map(|(a, b, s)| Op::Replace(a, b, s)),
        1 => text(40).prop_map(Op::Push),
        1 => text(200).prop_map(Op::Append),
        2 => (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Op::Slice(a, b)),
    ]
}

proptest! {
    /// Tiny chunks: every edit cuts and merges chunks.
    #[test]
    fn matches_a_string_with_tiny_chunks(start in text(300), ops in prop::collection::vec(op(), 1..32)) {
        run::<4>(&start, &ops).map_err(TestCaseError::fail)?;
    }

    #[test]
    fn matches_a_string_with_small_chunks(start in text(300), ops in prop::collection::vec(op(), 1..32)) {
        run::<9>(&start, &ops).map_err(TestCaseError::fail)?;
    }

    /// The default size, and texts long enough for several levels.
    #[test]
    fn matches_a_string_with_default_chunks(start in text(4000), ops in prop::collection::vec(op(), 1..16)) {
        run::<{ mamre_rope::CHUNK }>(&start, &ops).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn a_plain_rope_counts_only_the_basics() {
    let rope: Rope = Rope::from("# Title\n\nSome words here.\n");
    assert_eq!(rope.summary().len, 26);
    assert_eq!(rope.summary().lines, Point::new(3, 0));
    assert_eq!(rope.summary().units, Plain);
}

#[test]
fn a_markdown_rope_counts_units() {
    let rope: Rope<Markdown> = Rope::from("# Title\n\nSome words here.\n\n## Part\nMore.");
    let units = rope.summary().units;
    // Words and paragraphs are prose rules, blind to markup: `#` and `##`
    // are words, and a heading with a line under it is one paragraph.
    assert_eq!(units.words(), 8);
    assert_eq!(units.paragraphs(), 3);
    assert_eq!(units.sections(), 2);
}

#[test]
#[should_panic(expected = "not a char boundary")]
fn cutting_inside_a_char_panics() {
    let mut rope: Rope = Rope::from("日本");
    rope.remove(1..3);
}

#[test]
fn seeking_by_unit() {
    let text = "# One\nHi there. Bye!\n\n## Two\nfoo.bar baz\n";
    let rope: Rope<Markdown, 4> = Rope::from(text);
    let piece = |r: Option<std::ops::Range<usize>>| r.map(|r| &text[r]);

    assert_eq!(rope.count(Unit::Section), 2);
    assert_eq!(
        piece(rope.unit_range(1, Unit::Section)),
        Some("## Two\nfoo.bar baz\n")
    );
    // Vim's rule: a heading has no full stop, so it runs into the sentence
    // after it.
    assert_eq!(
        piece(rope.unit_range(0, Unit::Sentence)),
        Some("# One\nHi there.")
    );
    assert_eq!(piece(rope.unit_range(1, Unit::Sentence)), Some("Bye!"));
    assert_eq!(piece(rope.unit_range(4, Unit::VimWord)), Some("."));

    // The word under the cursor, and where `w` goes from there.
    let cursor = text.find("bar").unwrap() + 1;
    let n = rope.unit_at(cursor, Unit::VimWord).unwrap();
    assert_eq!(piece(rope.unit_range(n, Unit::VimWord)), Some("bar"));
    assert_eq!(piece(rope.unit_range(n + 1, Unit::VimWord)), Some("baz"));
    assert_eq!(rope.units_before(cursor, Unit::Section), 2);
}

#[test]
#[should_panic(expected = "doesn't count Section")]
fn a_prose_rope_has_no_sections() {
    let rope: Rope<Prose> = Rope::from("# Heading");
    rope.count(Unit::Section);
}

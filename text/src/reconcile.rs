//! Edits made outside mamre (#35): a file changed by another tool, or text
//! that comes back from one, arrives with no ids. Matching its content
//! against the text keeps the ids of what didn't change, so anchors,
//! sections and history stay on it; the rest becomes ordinary edits.
//!
//! The matching is patience diff's (Bram Cohen's):
//! - lines that appear exactly once on each side are the same lines; of
//!   those, the longest run in the same order on both sides anchors the
//!   rest, and the stretches between anchors are matched the same way,
//!   recursively, against lines unique within them;
//! - so a repeated paragraph stays unmatched unless its neighbours pin it
//!   down, by making it unique within the stretch between them;
//! - lines unique on both sides but out of that order were moved: they're
//!   matched as they are and the text is moved, not deleted and retyped;
//! - inside a stretch with no unique lines (a paragraph edited in place),
//!   characters are matched: what the two share at either end, and for
//!   short middles the longest common subsequence.
//!
//! Lines are hashed whole (std's hasher, through a map), which is what
//! fingerprinting windows comes to at line granularity. Rolling
//! fingerprints of windows inside long paragraphs (the PolyHash sums the
//! tree can keep) would find moves within a paragraph too: later.

use std::collections::HashMap;
use std::ops::Range;

/// A piece of the new text, in order: some of the old text, kept (its
/// characters keep their ids), or new text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Old(Range<usize>),
    New(Range<usize>),
}

/// A character-level middle longer than this, on either side, is replaced
/// whole rather than matched: the subsequence costs the product of the two.
const LCS_LIMIT: usize = 400;

/// What of `new` is `old`'s: the pieces, in `new`'s order.
pub(crate) fn plan(old: &str, new: &str) -> Vec<Piece> {
    let ol = lines(old);
    let nl = lines(new);
    let mut matched: Vec<(Range<usize>, Range<usize>)> = Vec::new();
    let old_idx: Vec<usize> = (0..ol.len()).collect();
    let new_idx: Vec<usize> = (0..nl.len()).collect();
    let m = Matcher {
        old,
        new,
        ol: &ol,
        nl: &nl,
    };
    m.lines(&old_idx, &new_idx, &mut matched);
    #[cfg(debug_assertions)]
    {
        let mut o: Vec<Range<usize>> = matched.iter().map(|(o, _)| o.clone()).collect();
        let mut n: Vec<Range<usize>> = matched.iter().map(|(_, n)| n.clone()).collect();
        o.sort_unstable_by_key(|r| r.start);
        n.sort_unstable_by_key(|r| r.start);
        assert!(
            o.windows(2).all(|w| w[0].end <= w[1].start),
            "old matched twice: {o:?}"
        );
        assert!(
            n.windows(2).all(|w| w[0].end <= w[1].start),
            "new matched twice"
        );
    }
    pieces(new.len(), matched)
}

/// The lines of `s`, each with its newline.
fn lines(s: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    for line in s.split_inclusive('\n') {
        out.push(at..at + line.len());
        at += line.len();
    }
    out
}

/// The lines (indices into `ol` and `nl`, within `a` and `b`) that appear
/// once on each side, by their place on the new side.
fn unique_pairs(
    old: &str,
    new: &str,
    ol: &[Range<usize>],
    nl: &[Range<usize>],
    a: impl IntoIterator<Item = usize>,
    b: impl IntoIterator<Item = usize>,
) -> Vec<(usize, usize)> {
    let mut seen: HashMap<&str, (usize, usize, usize, usize)> = HashMap::new();
    for i in a {
        let e = seen.entry(&old[ol[i].clone()]).or_default();
        e.0 += 1;
        e.1 = i;
    }
    for j in b {
        let e = seen.entry(&new[nl[j].clone()]).or_default();
        e.2 += 1;
        e.3 = j;
    }
    let mut pairs: Vec<(usize, usize)> = seen
        .into_values()
        .filter(|&(ca, _, cb, _)| ca == 1 && cb == 1)
        .map(|(_, i, _, j)| (i, j))
        .collect();
    pairs.sort_unstable_by_key(|&(_, j)| j);
    pairs
}

/// The longest run of `pairs` (sorted by their second) in order by their
/// first too: the indices into `pairs`.
fn lis(pairs: &[(usize, usize)]) -> Vec<usize> {
    // Patience sorting: each pile's top, and each pair's predecessor.
    let mut tops: Vec<usize> = Vec::new();
    let mut prev: Vec<Option<usize>> = vec![None; pairs.len()];
    for (k, &(i, _)) in pairs.iter().enumerate() {
        let p = tops.partition_point(|&t| pairs[t].0 < i);
        prev[k] = p.checked_sub(1).map(|q| tops[q]);
        if p == tops.len() {
            tops.push(k);
        } else {
            tops[p] = k;
        }
    }
    let mut out = Vec::new();
    let mut at = tops.last().copied();
    while let Some(k) = at {
        out.push(k);
        at = prev[k];
    }
    out.reverse();
    out
}

struct Matcher<'a> {
    old: &'a str,
    new: &'a str,
    ol: &'a [Range<usize>],
    nl: &'a [Range<usize>],
}

impl Matcher<'_> {
    fn old_line(&self, i: usize) -> &str {
        &self.old[self.ol[i].clone()]
    }

    fn new_line(&self, j: usize) -> &str {
        &self.new[self.nl[j].clone()]
    }

    /// Matches old lines `a` against new lines `b` (indices, each in order).
    fn lines(&self, a: &[usize], b: &[usize], out: &mut Vec<(Range<usize>, Range<usize>)>) {
        // The same lines at either end.
        let mut s = 0;
        while s < a.len() && s < b.len() && self.old_line(a[s]) == self.new_line(b[s]) {
            out.push((self.ol[a[s]].clone(), self.nl[b[s]].clone()));
            s += 1;
        }
        let mut e = 0;
        while e < a.len() - s
            && e < b.len() - s
            && self.old_line(a[a.len() - 1 - e]) == self.new_line(b[b.len() - 1 - e])
        {
            let (i, j) = (a[a.len() - 1 - e], b[b.len() - 1 - e]);
            out.push((self.ol[i].clone(), self.nl[j].clone()));
            e += 1;
        }
        let (a, b) = (&a[s..a.len() - e], &b[s..b.len() - e]);
        if a.is_empty() || b.is_empty() {
            return;
        }
        // Lines unique to this stretch on both sides, in order on both.
        let pairs = unique_pairs(
            self.old,
            self.new,
            self.ol,
            self.nl,
            a.iter().copied(),
            b.iter().copied(),
        );
        if pairs.is_empty() {
            return self.chars(a, b, out);
        }
        // Those in order on both sides anchor the rest; those out of it
        // were moved: matched as they are, and out of the stretch.
        let keep: std::collections::HashSet<usize> = lis(&pairs).into_iter().collect();
        let anchors: Vec<(usize, usize)> = (0..pairs.len())
            .filter(|k| keep.contains(k))
            .map(|k| pairs[k])
            .collect();
        let mut moved_old = std::collections::HashSet::new();
        let mut moved_new = std::collections::HashSet::new();
        let in_a: std::collections::HashSet<usize> = a.iter().copied().collect();
        let in_b: std::collections::HashSet<usize> = b.iter().copied().collect();
        let anchored_old: std::collections::HashSet<usize> =
            anchors.iter().map(|&(i, _)| i).collect();
        let anchored_new: std::collections::HashSet<usize> =
            anchors.iter().map(|&(_, j)| j).collect();
        for (k, &(i, j)) in pairs.iter().enumerate() {
            // An anchor, or taken already by a move beside it.
            if keep.contains(&k) || moved_old.contains(&i) || moved_new.contains(&j) {
                continue;
            }
            out.push((self.ol[i].clone(), self.nl[j].clone()));
            moved_old.insert(i);
            moved_new.insert(j);
            // And the same lines either side of it, moved with it (blank
            // lines between a moved chapter's paragraphs, say).
            for step in [1isize, -1] {
                let (mut x, mut y) = (i as isize + step, j as isize + step);
                while x >= 0 && y >= 0 {
                    let (xu, yu) = (x as usize, y as usize);
                    let free = |set: &std::collections::HashSet<usize>,
                                anch: &std::collections::HashSet<usize>,
                                moved: &std::collections::HashSet<usize>,
                                v: usize| {
                        set.contains(&v) && !anch.contains(&v) && !moved.contains(&v)
                    };
                    if !free(&in_a, &anchored_old, &moved_old, xu)
                        || !free(&in_b, &anchored_new, &moved_new, yu)
                        || self.old_line(xu) != self.new_line(yu)
                    {
                        break;
                    }
                    out.push((self.ol[xu].clone(), self.nl[yu].clone()));
                    moved_old.insert(xu);
                    moved_new.insert(yu);
                    x += step;
                    y += step;
                }
            }
        }
        let a: Vec<usize> = a
            .iter()
            .copied()
            .filter(|i| !moved_old.contains(i))
            .collect();
        let b: Vec<usize> = b
            .iter()
            .copied()
            .filter(|j| !moved_new.contains(j))
            .collect();
        let (a, b) = (&a[..], &b[..]);
        // Between each anchor and the next, recursively.
        let (mut pa, mut pb) = (0, 0);
        for (i, j) in anchors {
            let ka = a
                .iter()
                .position(|&x| x == i)
                .expect("an anchor is in the stretch");
            let kb = b
                .iter()
                .position(|&x| x == j)
                .expect("an anchor is in the stretch");
            self.lines(&a[pa..ka], &b[pb..kb], out);
            out.push((self.ol[i].clone(), self.nl[j].clone()));
            (pa, pb) = (ka + 1, kb + 1);
        }
        self.lines(&a[pa..], &b[pb..], out);
    }

    /// Matches characters, between stretches of lines each side holds
    /// whole (one run of text a side; otherwise nothing matches).
    fn chars(&self, a: &[usize], b: &[usize], out: &mut Vec<(Range<usize>, Range<usize>)>) {
        let run = |ls: &[Range<usize>], idx: &[usize]| -> Option<Range<usize>> {
            let first = ls[idx[0]].start;
            let mut end = first;
            for &k in idx {
                if ls[k].start != end {
                    return None;
                }
                end = ls[k].end;
            }
            Some(first..end)
        };
        let (Some(x), Some(y)) = (run(self.ol, a), run(self.nl, b)) else {
            return;
        };
        let (os, ns) = (&self.old[x.clone()], &self.new[y.clone()]);
        // What they share at either end.
        let pre = common_prefix(os, ns);
        let suf = common_suffix(&os[pre..], &ns[pre..]);
        if pre > 0 {
            out.push((x.start..x.start + pre, y.start..y.start + pre));
        }
        if suf > 0 {
            out.push((x.end - suf..x.end, y.end - suf..y.end));
        }
        let (mo, mn) = (x.start + pre..x.end - suf, y.start + pre..y.end - suf);
        let oc: Vec<(usize, char)> = self.old[mo.clone()].char_indices().collect();
        let nc: Vec<(usize, char)> = self.new[mn.clone()].char_indices().collect();
        if oc.is_empty() || nc.is_empty() || oc.len() > LCS_LIMIT || nc.len() > LCS_LIMIT {
            return;
        }
        for (i, j) in lcs(&oc, &nc) {
            let (oi, c) = oc[i];
            let (nj, _) = nc[j];
            out.push((
                mo.start + oi..mo.start + oi + c.len_utf8(),
                mn.start + nj..mn.start + nj + c.len_utf8(),
            ));
        }
    }
}

/// The longest prefix both share, in bytes (on a char boundary of each).
fn common_prefix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            break;
        }
        n += x.len_utf8();
    }
    n
}

fn common_suffix(a: &str, b: &str) -> usize {
    let mut n = 0;
    for (x, y) in a.chars().rev().zip(b.chars().rev()) {
        if x != y {
            break;
        }
        n += x.len_utf8();
    }
    n
}

/// The longest common subsequence of two short sequences of chars: the
/// pairs of indices matched, in order.
fn lcs(a: &[(usize, char)], b: &[(usize, char)]) -> Vec<(usize, usize)> {
    let (n, m) = (a.len(), b.len());
    let mut t = vec![0u16; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            t[i * (m + 1) + j] = if a[i].1 == b[j].1 {
                t[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < n && j < m {
        if a[i].1 == b[j].1 {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The pieces of a new text of `len` bytes, from what of it matched what.
fn pieces(len: usize, mut matched: Vec<(Range<usize>, Range<usize>)>) -> Vec<Piece> {
    matched.sort_unstable_by_key(|(_, n)| n.start);
    let mut out: Vec<Piece> = Vec::new();
    let mut at = 0;
    for (o, n) in matched {
        if n.start > at {
            out.push(Piece::New(at..n.start));
        }
        // Joined to the last, if it carries on in both.
        match out.last_mut() {
            Some(Piece::Old(last)) if last.end == o.start && n.start == at => last.end = o.end,
            _ => out.push(Piece::Old(o)),
        }
        at = n.end;
    }
    if at < len {
        out.push(Piece::New(at..len));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pieces put back together make the new text.
    fn rebuild(old: &str, new: &str) -> String {
        plan(old, new)
            .into_iter()
            .map(|p| match p {
                Piece::Old(r) => old[r].to_string(),
                Piece::New(r) => new[r].to_string(),
            })
            .collect()
    }

    #[test]
    fn pieces_make_the_new_text() {
        for (old, new) in [
            ("a\nb\nc\n", "a\nb\nc\n"),
            ("a\nb\nc\n", "x\na\nb\nc\ny\n"),
            ("one\ntwo\nthree\n", "three\none\ntwo\n"),
            ("the big dog\n", "the large dog\n"),
            ("same\nsame\nx\nsame\n", "same\ny\nsame\nx\nsame\n"),
            ("whole\nfile\n", "something\nelse\n"),
            ("", "new\n"),
            ("old\n", ""),
            ("héllo 🦀\n", "hello 🦀!\n"),
        ] {
            assert_eq!(rebuild(old, new), new, "{old:?} to {new:?}");
        }
    }

    #[test]
    fn a_moved_chapter_with_blank_lines() {
        let mut old = String::new();
        for c in 1..=4 {
            old.push_str(&format!("# Chapter {c}\n\n"));
            for p in 0..3 {
                old.push_str(&format!("({c}.{p}) text\n\n"));
            }
        }
        let ch2 = old.find("# Chapter 2\n").unwrap();
        let ch3 = old.find("# Chapter 3\n").unwrap();
        let mut new = old.clone();
        let chapter = new[ch2..ch3].to_string();
        new.replace_range(ch2..ch3, "");
        new.push_str(&chapter);
        assert_eq!(rebuild(&old, &new), new);
        // The chapter, blank lines and all, is kept: at most its last
        // blank line is retyped (the file's last line takes the match).
        let typed: usize = plan(&old, &new)
            .into_iter()
            .map(|p| match p {
                Piece::New(r) => r.len(),
                Piece::Old(_) => 0,
            })
            .sum();
        assert!(typed <= 1, "{:?}", plan(&old, &new));
    }

    #[test]
    fn a_moved_line_is_kept() {
        let old = "one\ntwo\nthree\nfour\n";
        let new = "one\nthree\nfour\ntwo\n";
        let p = plan(old, new);
        // Everything is old text: nothing new typed.
        assert!(p.iter().all(|p| matches!(p, Piece::Old(_))), "{p:?}");
    }
}

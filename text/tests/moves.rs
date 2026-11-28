//! Moves as a delete and an insert (#68): the moved text is new
//! characters, and what's on the old ones (carets, sections, paragraph
//! kinds) follows them by the came-from record, here and on every other
//! replica. Undo brings back the originals, and with them where everything
//! was.

use mamre_text::{Edit, Kind, Text};

struct Replica {
    text: Text,
    editor: String,
    /// The moves hinted: where from, and the text that arrived.
    moved: Vec<(usize, String)>,
}

impl Replica {
    fn new(name: &str) -> Self {
        Replica {
            text: Text::new(name),
            editor: String::new(),
            moved: Vec::new(),
        }
    }

    fn apply(&mut self, edits: Vec<Edit>) {
        for edit in edits {
            match edit {
                Edit::Insert { at, text } => self.editor.insert_str(at, &text),
                Edit::Delete { range } => self.editor.replace_range(range, ""),
                Edit::Moved { from, to } => {
                    assert!(from <= self.editor.len(), "from {from}");
                    self.moved.push((from, self.editor[to].to_string()));
                }
                Edit::Section { .. } | Edit::Isolates | Edit::Lost { .. } => {}
            }
        }
        assert_eq!(self.editor, self.text.to_string(), "the edits are the text's");
    }

    fn type_(&mut self, at: usize, text: &str) {
        let e = self.text.insert(at, text).unwrap();
        self.apply(e);
    }

    fn move_text(&mut self, range: std::ops::Range<usize>, to: usize) -> Vec<Edit> {
        let e = self.text.move_text(range, to).unwrap();
        self.apply(e.clone());
        e
    }

    fn undo(&mut self) {
        let e = self.text.undo().expect("something to undo");
        self.apply(e);
    }

    fn redo(&mut self) {
        let e = self.text.redo().expect("something to redo");
        self.apply(e);
    }
}

fn sync(a: &mut Replica, b: &mut Replica) {
    let mut to_b = vec![a.text.hello()];
    let mut to_a = vec![b.text.hello()];
    for _ in 0..20 {
        if to_a.is_empty() && to_b.is_empty() {
            return;
        }
        for m in std::mem::take(&mut to_b) {
            let got = b.text.receive(&m).unwrap();
            to_a.extend(got.reply);
            b.apply(got.edits);
        }
        for m in std::mem::take(&mut to_a) {
            let got = a.text.receive(&m).unwrap();
            to_b.extend(got.reply);
            a.apply(got.edits);
        }
    }
    panic!("they kept talking");
}

#[test]
fn a_move_is_a_delete_and_an_insert() {
    let mut a = Replica::new("a");
    a.type_(0, "one two three");
    let e = a.move_text(4..8, 13);
    assert_eq!(
        e,
        [
            Edit::Delete { range: 4..8 },
            Edit::Insert {
                at: 9,
                text: "two ".into()
            },
            // And a hint that it was a move: from where the delete left a
            // gap, to the insert.
            Edit::Moved { from: 4, to: 9..13 }
        ]
    );
    assert_eq!(a.editor, "one threetwo ");
}

#[test]
fn a_move_is_hinted_on_the_other_replica_too() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    sync(&mut a, &mut b);
    a.move_text(4..8, 0);
    sync(&mut a, &mut b);
    assert_eq!(b.editor, "two one three");
    // The gap "two " left is after "one ", which now comes after it.
    assert_eq!(b.moved, [(8, "two ".to_string())]);
    assert_eq!(a.moved, b.moved);
}

#[test]
fn undo_and_redo_are_hinted_as_moves_back_and_forth() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    sync(&mut a, &mut b);
    a.move_text(4..8, 13);
    sync(&mut a, &mut b);
    a.moved.clear();
    b.moved.clear();
    a.undo();
    assert_eq!(a.editor, "one two three");
    // Back from the end, where the copy was.
    assert_eq!(a.moved, [(13, "two ".to_string())]);
    sync(&mut a, &mut b);
    assert_eq!(b.moved, a.moved, "an undo from elsewhere too");
    a.moved.clear();
    b.moved.clear();
    a.redo();
    assert_eq!(a.moved, [(4, "two ".to_string())]);
    sync(&mut a, &mut b);
    assert_eq!(b.moved, a.moved, "and a redo");
}

#[test]
fn typing_and_deleting_are_no_moves() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    let e = a.text.delete(4..8).unwrap();
    a.apply(e);
    a.type_(0, "two ");
    sync(&mut a, &mut b);
    a.undo();
    sync(&mut a, &mut b);
    assert!(a.moved.is_empty() && b.moved.is_empty());
}

/// Text two writers move at once shows twice: each replica hints the move
/// it made, and the other's copy is only an insert there (its originals
/// were gone already).
#[test]
fn text_both_moved_is_hinted_once_on_each() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "xx yy zz");
    sync(&mut a, &mut b);
    a.move_text(3..6, 0);
    b.move_text(3..6, 8);
    sync(&mut a, &mut b);
    assert_eq!(a.editor, "yy xx zzyy ");
    assert_eq!(a.moved, [(6, "yy ".to_string())]);
    assert_eq!(b.moved, [(3, "yy ".to_string())]);
}

#[test]
fn a_moved_section_is_hinted() {
    let mut a = Replica::new("a");
    a.type_(0, "Title\nbody\nEnd\n");
    let (end, e) = a.text.mark_section(11, 1).unwrap();
    a.apply(e);
    let e = a.text.move_section(end, 0).unwrap();
    a.apply(e);
    assert_eq!(a.editor, "End\nTitle\nbody\n");
    assert_eq!(a.moved, [(15, "End\n".to_string())]);
}

#[test]
fn a_caret_in_moved_text_goes_with_it() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    sync(&mut a, &mut b);
    // Carets between "t" and "wo", on both replicas.
    let mine = a.text.anchor(5).unwrap();
    let theirs = b.text.anchor(5).unwrap();
    a.move_text(4..8, 13);
    assert_eq!(a.editor, "one threetwo ");
    assert_eq!(a.text.position(&mine), Some(10));
    sync(&mut a, &mut b);
    assert_eq!(b.text.position(&theirs), Some(10), "on the other replica too");
}

#[test]
fn a_caret_outside_stays() {
    let mut a = Replica::new("a");
    a.type_(0, "one two three");
    let before = a.text.anchor(2).unwrap();
    let after = a.text.anchor(11).unwrap();
    a.move_text(4..8, 0);
    assert_eq!(a.editor, "two one three");
    assert_eq!(a.text.position(&before), Some(6));
    assert_eq!(a.text.position(&after), Some(11));
}

#[test]
fn undo_puts_carets_back_and_redo_takes_them_again() {
    let mut a = Replica::new("a");
    a.type_(0, "one two three");
    let caret = a.text.anchor(5).unwrap();
    a.move_text(4..8, 13);
    assert_eq!(a.text.position(&caret), Some(10));
    a.undo();
    assert_eq!(a.editor, "one two three");
    assert_eq!(a.text.position(&caret), Some(5), "back on the originals");
    a.redo();
    assert_eq!(a.editor, "one threetwo ");
    assert_eq!(a.text.position(&caret), Some(10));
}

#[test]
fn text_moved_twice_is_followed_twice() {
    let mut a = Replica::new("a");
    a.type_(0, "aa bb cc ");
    let caret = a.text.anchor(4).unwrap();
    a.move_text(3..6, 9);
    assert_eq!(a.editor, "aa cc bb ");
    assert_eq!(a.text.position(&caret), Some(7));
    a.move_text(6..9, 0);
    assert_eq!(a.editor, "bb aa cc ");
    assert_eq!(a.text.position(&caret), Some(1));
}

/// A caret made in the range at the same moment as the move: the move's
/// record names characters, not carets, so it follows too.
#[test]
fn a_caret_made_while_it_moved_follows() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "one two three");
    sync(&mut a, &mut b);
    a.move_text(4..8, 0);
    let theirs = b.text.anchor(6).unwrap();
    sync(&mut a, &mut b);
    assert_eq!(b.editor, "two one three");
    assert_eq!(b.text.position(&theirs), Some(2));
}

/// Two writers move the same text at once: each makes a copy, so it shows
/// twice. A caret on it can't tell which to follow, so it doesn't, and
/// falls to the side that still shows.
#[test]
fn text_both_moved_shows_twice_and_a_caret_on_it_stays_out() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "xx yy zz");
    sync(&mut a, &mut b);
    // A caret after "xx " and before "yy".
    let caret = a.text.anchor(3).unwrap();
    a.move_text(3..6, 0);
    b.move_text(3..6, 8);
    sync(&mut a, &mut b);
    assert_eq!(a.editor, "yy xx zzyy ");
    assert_eq!(b.editor, a.editor);
    // "yy" shows twice; the caret keeps to "xx ", which is where it was.
    assert_eq!(a.text.position(&caret), Some(6));
}

#[test]
fn a_moved_section_keeps_its_id_and_its_kind() {
    let (mut a, mut b) = (Replica::new("a"), Replica::new("b"));
    a.type_(0, "\nIntro\nbody\nLater\nmore\n");
    let (intro, e) = a.text.set_kind(1, Kind::Heading(1)).unwrap();
    a.apply(e);
    let (later, e) = a.text.set_kind(12, Kind::Heading(1)).unwrap();
    a.apply(e);
    let (quote, e) = a.text.set_kind(18, Kind::Quote).unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    // "Later" runs from the break before it to the end.
    assert_eq!(a.text.section_range(later), Some(11..23));
    let e = a.text.move_section(later, 0).unwrap();
    a.apply(e);
    sync(&mut a, &mut b);
    for r in [&a, &b] {
        assert_eq!(r.editor, "\nLater\nmore\n\nIntro\nbody");
        assert_eq!(r.text.section_range(later), Some(0..12));
        assert_eq!(r.text.section_range(intro), Some(12..23));
        assert_eq!(r.text.kind_at(r.editor.find("more").unwrap()), Kind::Quote);
        assert_eq!(r.text.kind_at(r.editor.find("Later").unwrap()), Kind::Heading(1));
        let ids: Vec<_> = r.text.sections().iter().map(|s| s.id).collect();
        assert!(ids.contains(&later) && ids.contains(&intro) && ids.contains(&quote));
    }
    // And back.
    a.undo();
    assert_eq!(a.editor, "\nIntro\nbody\nLater\nmore\n");
    assert_eq!(a.text.section_range(later), Some(11..23));
    assert_eq!(a.text.kind_at(a.editor.find("more").unwrap()), Kind::Quote);
}

#[test]
fn a_move_survives_a_save() {
    let mut a = Replica::new("a");
    a.type_(0, "Title\nbody\nEnd\n");
    let (end, e) = a.text.mark_section(11, 1).unwrap();
    a.apply(e);
    let e = a.text.move_section(end, 0).unwrap();
    a.apply(e);
    let mut file = Vec::new();
    a.text.save(&mut file);
    let again = Text::open("a", &file).unwrap();
    assert_eq!(again.to_string(), a.text.to_string());
    assert_eq!(again.section_range(end), a.text.section_range(end));
    assert_eq!(again.sections().len(), a.text.sections().len());
}

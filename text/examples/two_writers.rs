//! Two writers on one document, talking in messages: the whole interface
//! in a page. `cargo run -p mamre-text --example two_writers`.

use mamre_text::{Edit, Text};

/// Carries messages between two replicas until neither has more to say,
/// printing what each editor is told.
fn talk(a: &mut Text, b: &mut Text) {
    let mut to_b = vec![a.hello()];
    let mut to_a = vec![b.hello()];
    while !to_a.is_empty() || !to_b.is_empty() {
        for m in std::mem::take(&mut to_b) {
            let got = b.receive(&m).expect("a good message");
            report("b", &got.edits);
            to_a.extend(got.reply);
        }
        for m in std::mem::take(&mut to_a) {
            let got = a.receive(&m).expect("a good message");
            report("a", &got.edits);
            to_b.extend(got.reply);
        }
    }
}

fn report(who: &str, edits: &[Edit]) {
    for e in edits {
        println!("  {who}'s editor: {e:?}");
    }
}

fn main() {
    let mut ana = Text::new("ana@laptop");
    let mut bo = Text::new("bo@tablet");

    let draft = "Chapter one\nIt was a dark night.\nChapter two\nThe end.\n";
    ana.insert(0, draft).unwrap();
    let (one, _) = ana.mark_section(0, 1).unwrap();
    let (two, _) = ana
        .mark_section(draft.find("Chapter two").unwrap(), 1)
        .unwrap();
    println!("ana writes two chapters; they meet:");
    talk(&mut ana, &mut bo);

    // Ana's caret at the end of "dark"; at the same time, Bo swaps the
    // chapters and Ana adds a word.
    let dark = draft.find("dark").unwrap() + "dark".len();
    let caret = ana.anchor(dark).unwrap();
    let two_there = bo.sections()[1].id;
    bo.move_section(two_there, 0).unwrap();
    ana.insert(dark, " and stormy").unwrap();
    println!("\nbo swaps the chapters while ana types; they meet:");
    talk(&mut ana, &mut bo);

    println!("\nboth now read:\n{ana}");
    assert_eq!(ana.to_string(), bo.to_string());
    println!("ana's caret is at byte {:?}", ana.position(&caret));
    println!(
        "chapter one runs {:?}, chapter two {:?}",
        ana.section_range(one),
        ana.section_range(two)
    );

    let mut file = Vec::new();
    ana.save(&mut file);
    let reopened = Text::open("ana@laptop", &file).unwrap();
    println!(
        "saved and opened again: {} bytes of file, same text: {}",
        file.len(),
        reopened.to_string() == ana.to_string()
    );
}

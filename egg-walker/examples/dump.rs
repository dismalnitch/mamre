//! Shows a mamre file as JSON: `cargo run --example dump -- book.mamre`.
//! Items are read as text (`char`).

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: dump FILE");
        std::process::exit(2);
    });
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    match mamre_egg_walker::file::dump::<char>(&bytes) {
        Ok(shown) => println!("{}", serde_json::to_string_pretty(&shown).unwrap()),
        Err(e) => {
            eprintln!("{path}: {e:?}");
            std::process::exit(1);
        }
    }
}

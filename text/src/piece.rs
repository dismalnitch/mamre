//! What the history's sequence is made of: the text's characters, and the
//! markers that open and close its isolates.
//!
//! A marker is an item like a character: it's put in, taken out, moved
//! and merged by the same rules. It just isn't text. The rope never holds
//! one, so nothing that reads, counts or searches the text sees it.
//!
//! On disk and on the wire a run of pieces is one UTF-8 string, as a run
//! of characters is, and with no markers in it, the very same bytes: a
//! text with no isolates costs nothing for being able to have them, and
//! files written before there were any read as they did. A marker is
//! written in the string after [`ESCAPE`], a code point Unicode sets
//! aside as never a character; a text that holds that code point itself
//! has it written twice.

use mamre::codec::{Codec, DecodeError, get_str, get_varint, put_str, put_usize, put_varint};

use crate::Direction;

/// One item of the sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piece {
    Char(char),
    /// Where isolate `id` starts, and the direction it's set in.
    Open(Direction, u64),
    /// Where isolate `id` ends.
    Close(u64),
}

/// Comes before a marker in a run's string. U+FDD0: a noncharacter.
const ESCAPE: char = '\u{fdd0}';

/// Alone, a marker is a number past the last code point, then its id.
const OPEN_LTR: u64 = 0x11_0000;
const OPEN_RTL: u64 = 0x11_0001;
const CLOSE: u64 = 0x11_0002;

impl Piece {
    fn write(self, out: &mut String) {
        use std::fmt::Write;
        let _ = match self {
            Piece::Char(ESCAPE) => write!(out, "{ESCAPE}{ESCAPE}"),
            Piece::Char(c) => write!(out, "{c}"),
            Piece::Open(Direction::LeftToRight, id) => write!(out, "{ESCAPE}L{id:x};"),
            Piece::Open(Direction::RightToLeft, id) => write!(out, "{ESCAPE}R{id:x};"),
            Piece::Close(id) => write!(out, "{ESCAPE}C{id:x};"),
        };
    }

    /// The pieces `text` was written from.
    fn read(text: &str) -> Result<Vec<Piece>, DecodeError> {
        let bad = DecodeError::Invalid("a marker that isn't one");
        let mut out = Vec::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != ESCAPE {
                out.push(Piece::Char(c));
                continue;
            }
            let kind = chars.next().ok_or(bad.clone())?;
            if kind == ESCAPE {
                out.push(Piece::Char(ESCAPE));
                continue;
            }
            let rest = chars.as_str();
            let end = rest.find(';').ok_or(bad.clone())?;
            let id = u64::from_str_radix(&rest[..end], 16).map_err(|_| bad.clone())?;
            chars = rest[end + 1..].chars();
            out.push(match kind {
                'L' => Piece::Open(Direction::LeftToRight, id),
                'R' => Piece::Open(Direction::RightToLeft, id),
                'C' => Piece::Close(id),
                _ => return Err(bad),
            });
        }
        Ok(out)
    }
}

impl Codec for Piece {
    fn encode(&self, out: &mut Vec<u8>) {
        match *self {
            Piece::Char(c) => put_varint(out, u64::from(c as u32)),
            Piece::Open(Direction::LeftToRight, id) => {
                put_varint(out, OPEN_LTR);
                put_varint(out, id);
            }
            Piece::Open(Direction::RightToLeft, id) => {
                put_varint(out, OPEN_RTL);
                put_varint(out, id);
            }
            Piece::Close(id) => {
                put_varint(out, CLOSE);
                put_varint(out, id);
            }
        }
    }

    fn decode(input: &mut &[u8]) -> Result<Self, DecodeError> {
        match get_varint(input)? {
            OPEN_LTR => Ok(Piece::Open(Direction::LeftToRight, get_varint(input)?)),
            OPEN_RTL => Ok(Piece::Open(Direction::RightToLeft, get_varint(input)?)),
            CLOSE => Ok(Piece::Close(get_varint(input)?)),
            n => u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .map(Piece::Char)
                .ok_or(DecodeError::Invalid("not a char or a marker")),
        }
    }

    fn encode_run(items: &[Self], out: &mut Vec<u8>) {
        let mut text = String::with_capacity(items.len());
        for item in items {
            item.write(&mut text);
        }
        put_str(out, &text);
    }

    /// As a run of characters does it: the string goes in first, and its
    /// length is put in front after, so a whole document is never copied
    /// anywhere but into `out`.
    fn encode_iter(items: impl IntoIterator<Item = Self>, out: &mut Vec<u8>) -> usize {
        let start = out.len();
        let mut n = 0;
        let mut one = String::new();
        for item in items {
            one.clear();
            item.write(&mut one);
            out.extend_from_slice(one.as_bytes());
            n += 1;
        }
        let mut len = Vec::new();
        put_usize(&mut len, out.len() - start);
        out.splice(start..start, len);
        n
    }

    fn decode_run(n: usize, input: &mut &[u8]) -> Result<Vec<Self>, DecodeError> {
        let pieces = Piece::read(get_str(input)?)?;
        if pieces.len() != n {
            return Err(DecodeError::Invalid("a run of pieces is the wrong length"));
        }
        Ok(pieces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(pieces: &[Piece]) -> Vec<u8> {
        let mut out = Vec::new();
        Piece::encode_run(pieces, &mut out);
        out
    }

    #[test]
    fn text_alone_is_written_as_text_is() {
        let text = "abc אבג 😀\n";
        let chars: Vec<char> = text.chars().collect();
        let pieces: Vec<Piece> = chars.iter().copied().map(Piece::Char).collect();
        let mut as_chars = Vec::new();
        char::encode_run(&chars, &mut as_chars);
        assert_eq!(run(&pieces), as_chars);
        let mut streamed = Vec::new();
        assert_eq!(
            Piece::encode_iter(pieces.iter().copied(), &mut streamed),
            pieces.len()
        );
        assert_eq!(streamed, as_chars);
        assert_eq!(
            Piece::decode_run(pieces.len(), &mut &as_chars[..]),
            Ok(pieces)
        );
    }

    #[test]
    fn markers_and_the_escape_come_back() {
        let pieces = [
            Piece::Char('a'),
            Piece::Open(Direction::RightToLeft, u64::MAX),
            Piece::Char(ESCAPE),
            Piece::Char('א'),
            Piece::Open(Direction::LeftToRight, 0),
            Piece::Char(';'),
            Piece::Close(0),
            Piece::Close(u64::MAX),
            Piece::Char('L'),
        ];
        let bytes = run(&pieces);
        assert_eq!(
            Piece::decode_run(pieces.len(), &mut &bytes[..]),
            Ok(pieces.to_vec())
        );
        assert!(Piece::decode_run(pieces.len() + 1, &mut &bytes[..]).is_err());
        for p in pieces {
            let mut one = Vec::new();
            p.encode(&mut one);
            assert_eq!(Piece::decode(&mut &one[..]), Ok(p));
        }
        // A marker cut short is an error, not text.
        let mut cut = Vec::new();
        put_str(&mut cut, &format!("a{ESCAPE}R12"));
        assert!(Piece::decode_run(2, &mut &cut[..]).is_err());
    }
}

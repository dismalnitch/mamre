# mamre-text

Collaborative text: mamre's rope, kept by its CRDT (`mamre-egg-walker`).
A `Text` is one writer's replica of a document. Edit it in UTF-8 byte
offsets; replicas talk in messages; everyone who has seen the same edits
reads the same text.

```rust
use mamre_text::Text;

let mut ana = Text::new("ana@laptop");   // writer names are unique per replica
ana.insert(0, "Hello")?;
let mut bo = Text::new("bo@tablet");
// Carry bytes between them however you like: a socket, a relay, a file.
let got = bo.receive(&ana.hello())?;     // bo: "here's what I have"
for msg in got.reply { /* send to ana */ }
```

## The interface

| | |
|---|---|
| Reading | `rope()`, `len()`, `slice(range)`, `to_string()` |
| Local edits | `insert`, `delete`, `replace`, `move_text`, all in bytes; each returns the `Edit`s it made |
| Sections | `mark_section(at, level)` → `SectionId`; `sections()`, `section_range(id)`, `move_section(id, to)`. A section is a mark on a character, not syntax in the text |
| Paragraphs | `set_kind(at, kind)`, `kind_at(at)`, `paragraphs()` → `Paragraph { starts, range, kind, id }`. A paragraph's `Kind` (`Text`, `Heading(level)`, `Quote`, `Code`, `Equation`) is a section mark on the line break that starts it, so an empty paragraph has one, a break typed at a heading's end starts plain text, and a heading's section moves with its own break. Text before the first break takes no kind (`TextError::NoBreak`): start a document with a break |
| Anchors | `anchor(at)` → `Anchor`; `position(&anchor)`; `release(anchor)`. Carets and selections that stay put through every merge |
| Isolates | `mark_isolate(range, direction)` → `IsolateId` (on an isolate's own range: sets its direction); `isolates()` → `Isolate { id, range, direction, parent }`; `insert_in(at, text, id)`; `remove_isolate(id)`; `lost_isolates()`. Stretches set in a direction of their own (Hebrew in English, an English word in the Hebrew). Markers in the history, not in the text: saved, sent and merged, and text goes inside or outside one by which side of a marker it's put. Text moved out of one keeps its direction. An editor decides what's an isolate |
| Sync | `hello()` → bytes to send; `receive(bytes)` → `Received { reply, edits }` |
| Files | `save(&mut file)` appends; `Text::open(writer, &bytes)` |

`Edit` is what the editor's layout needs, in order, each in the text as
it was just before: `Insert { at, text }`, `Delete { range }`,
`Move { range, to }` (`to` is before the move, outside the range),
`Section { section, at, level }`, `Isolates` (they changed; the text didn't), and `Lost { ranges }` (a move or section mark discarded for one made at the same time, and where its text is now).

## Wiring it into an editor

1. Hold a `Text` where you hold your buffer, and read through `rope()`.
2. When the user types, call the edit and relayout from the `Edit`s it
   returns. They're what actually happened: a move to inside itself, say,
   returns none.
3. Say `hello()` to each peer when you connect, and now and then. Send
   every `reply` from `receive` back to whoever sent the message, and
   apply its `edits` to the layout as you would local ones. Messages may
   be dropped, duplicated or reordered; another hello catches up.
4. Keep carets and selections as `Anchor`s, not offsets: read
   `position` after every batch of edits.
5. Save by appending to the same bytes you opened: `save` writes only
   what's new, and the history comes back on `open`.

`cargo run -p mamre-text --example two_writers` runs the whole thing
in a page.

## Not here yet

- Undo. The history has it: undo steps (#14), and undoing a move brings
  back what it moved (#30). `Text` has no `undo` of its own yet; it
  comes with the editor's needs in hand.
- Peers aren't checked for honesty (#9): talk only to ones you trust.
- One writer name per replica: two windows on one device need two names.

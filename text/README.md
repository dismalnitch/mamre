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
| Anchors | `anchor(at)` → `Anchor`; `position(&anchor)`; `release(anchor)`. Carets and selections that stay put through every merge |
| Sync | `hello()` → bytes to send; `receive(bytes)` → `Received { reply, edits }` |
| Files | `save(&mut file)` appends; `Text::open(writer, &bytes)` |

`Edit` is what the editor's layout needs, in order, each in the text as
it was just before: `Insert { at, text }`, `Delete { range }`,
`Move { range, to }` (`to` is before the move, outside the range),
`Section { section, at, level }`, and `Lost { ranges }`.

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

- Undo: undo groups (#14) come with the editor's needs in hand.
- `Edit::Lost` is never sent yet. Until moves become copies (#20), a move
  crossing another made at the same time is dropped silently, and a move
  whose range cuts into a moved block does nothing.
- Peers aren't checked for honesty (#9): talk only to ones you trust.
- One writer name per replica: two windows on one device need two names.

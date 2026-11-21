//! Backspaces are the one thing diamond-types stores as a reversed run.

use diamond_types::list::OpLog;

#[test]
fn backspaces_come_out_reversed_and_play() {
    let mut oplog = OpLog::new();
    let a = oplog.get_or_create_agent_id("a");
    let mut v = vec![oplog.add_insert_at(a, &[], 0, "hello world")];
    for pos in (6..11).rev() {
        v = vec![oplog.add_delete_at(a, &v, pos..pos + 1)];
    }
    let reversed = oplog
        .iter_xf_operations()
        .filter_map(|(_, op)| op)
        .filter(|op| !op.loc.fwd)
        .count();
    assert!(reversed > 0, "expected a reversed run of backspaces");
    let mut rope: mamre_rope::Rope = mamre_rope::Rope::new();
    mamre_diamond::merge(&mut rope, &oplog, &[], &oplog.local_version());
    assert_eq!(rope.to_string(), oplog.checkout_tip().content().to_string());
}

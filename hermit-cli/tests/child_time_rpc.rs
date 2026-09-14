use detcore::GlobalState;
use detcore::types::DetTid;
use detcore::types::DetTime;
use detcore::types::LogicalTime;
use detcore::types::MmId;
use reverie::GlobalTool;
use reverie::syscalls::CloneFlags;

#[test]
fn inherited_time_survives_complete_detcore_rpc_encoding() {
    type Request = <GlobalState as GlobalTool>::Request;
    let parent = DetTid::from_raw(17);
    let child = DetTid::from_raw(18);
    let mm = MmId::initial(parent);
    let mut parent_clock = DetTime::zero();
    parent_clock.add_syscall_with_cost(101);
    let mut child_clock = parent_clock.clone_for_child();
    child_clock.advance_to(child_clock.as_nanos() + LogicalTime::from_nanos(1));

    // Construct the public associated request type through serde: its enum is
    // intentionally private to Detcore. Both actual startup request shapes must
    // retain the nonzero baseline and the fields that follow the clock.
    for request in [
        serde_json::json!({"StartNewThread": [child, child, null]}),
        serde_json::json!({"CreateVforkChildThread": [
            parent, parent, child, 0,
            CloneFlags::CLONE_VFORK | CloneFlags::CLONE_VM,
            libc::SIGCHLD, 999,
        ]}),
    ] {
        let original: Request =
            serde_json::from_value(serde_json::json!([child_clock, mm, request])).unwrap();
        let encoded = bincode::serde::encode_to_vec(&original, bincode::config::legacy()).unwrap();
        let (decoded, consumed): (Request, usize) =
            bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.0.as_nanos(), LogicalTime::from_nanos(102));
        assert_eq!(decoded.0.inherited_nanos(), LogicalTime::from_nanos(101));
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }
}

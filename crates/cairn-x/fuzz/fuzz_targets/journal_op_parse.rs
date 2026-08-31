//! §15.5 fuzz target: journal op deserializer must never panic on arbitrary bytes.

#![no_main]
use libfuzzer_sys::fuzz_target;
use cairn_proto::pb::JournalOp;
use prost::Message;

fuzz_target!(|data: &[u8]| {
    if let Ok(op) = JournalOp::decode(data) {
        let mut out = Vec::new();
        op.encode(&mut out).expect("re-encode");
        assert_eq!(out, data, "prost decode/encode must round-trip");
    }
});

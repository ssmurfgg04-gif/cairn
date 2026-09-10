//! §15.5 fuzz target: journal op deserializer must never panic on arbitrary bytes.

#![no_main]
use libfuzzer_sys::fuzz_target;
use cairn_proto::pb::JournalOp;
use prost::Message;

fuzz_target!(|data: &[u8]| {
    if let Ok(op) = JournalOp::decode(data) {
        // Byte-exact round-trips are NOT a prost guarantee: unknown fields
        // are skipped (schema evolution), non-minimal varints re-encode
        // canonically, and proto3 default-valued fields are not emitted
        // (run #154: input [40, 90] = two unknown field tags decoded to the
        // default message and re-encoded as empty). The §15.5 contract is:
        // decode never panics, and re-encoding a decoded message yields
        // bytes that decode back to the same message — a logical fixed
        // point, not byte equality.
        let mut out = Vec::new();
        op.encode(&mut out).expect("re-encode must not panic");
        let re = JournalOp::decode(out.as_slice())
            .expect("re-decoding canonical bytes must not fail");
        assert_eq!(re, op, "decode/encode/decode must be a fixed point");
    }
});

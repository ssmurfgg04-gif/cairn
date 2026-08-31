//! §15.5 fuzz target: pack parser must never panic on arbitrary bytes.

#![no_main]
use libfuzzer_sys::fuzz_target;
use cairn_core::pack::parse_pack;

fuzz_target!(|data: &[u8]| {
    if let Ok(objs) = parse_pack(data) {
        // parsed packs re-serialize to the exact same bytes
        assert_eq!(cairn_core::pack::build_pack(&objs), data);
    }
});

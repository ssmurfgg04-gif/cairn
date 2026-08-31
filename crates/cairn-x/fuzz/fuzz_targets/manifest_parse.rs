//! §15.5 fuzz target: manifest parser must never panic on arbitrary bytes.

#![no_main]
use libfuzzer_sys::fuzz_target;
use cairn_core::manifest::Manifest;

fuzz_target!(|data: &[u8]| {
    if let Ok(m) = Manifest::parse(data) {
        let (h, bytes) = m.serialize();
        assert_eq!(h, cairn_core::hash::Hash::of(&bytes));
    }
});

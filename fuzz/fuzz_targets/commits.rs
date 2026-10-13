#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| carryfs_core::fuzzing::commits(data));

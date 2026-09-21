#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| aurix_fuzz::e2ee_frame(data));

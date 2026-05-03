#![no_main]
use libfuzzer_sys::fuzz_target;
use epo::validate_cql;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else { return };
    let _ = validate_cql(s);
});

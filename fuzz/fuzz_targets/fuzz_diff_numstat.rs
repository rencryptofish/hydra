#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let files = hydra::app::parse_diff_numstat(data);
    // Verify all parsed files have non-empty paths
    for f in &files {
        assert!(!f.path.is_empty());
    }
});

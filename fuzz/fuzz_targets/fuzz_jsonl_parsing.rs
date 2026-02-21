#![no_main]
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    // Write fuzzed bytes to a temp file and try to parse as JSONL stats
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fuzz.jsonl");
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(data).unwrap();
    }
    let mut stats = hydra::logs::SessionStats::default();
    let _ = hydra::logs::update_session_stats_from_path_and_last_message(&path, &mut stats);
});

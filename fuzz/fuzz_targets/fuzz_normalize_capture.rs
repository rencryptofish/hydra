#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let result = hydra::app::normalize_capture(data);
    // Output should never contain ANSI escapes or braille characters
    assert!(!result.contains('\x1b'));
    assert!(!result.chars().any(|c| ('\u{2800}'..='\u{28FF}').contains(&c)));
    // No line should have trailing whitespace
    for line in result.lines() {
        assert_eq!(line, line.trim_end());
    }
});

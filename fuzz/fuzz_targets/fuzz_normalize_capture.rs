#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let result = hydra::app::normalize_capture(data);
    // Idempotency: normalizing twice should give the same result
    let result2 = hydra::app::normalize_capture(&result);
    assert_eq!(result, result2);
});

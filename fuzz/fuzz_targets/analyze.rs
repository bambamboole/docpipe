//! Feeds arbitrary bytes to the analysis entry point; a panic is a finding.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // OCR stays off (the default), so no native runtime is needed.
    let _ = docpipe::analyze::analyze(data, &docpipe::ocr::OcrSettings::default());
});

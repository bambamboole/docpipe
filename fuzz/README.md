# Fuzzing

`analyze` feeds arbitrary bytes to `docpipe::analyze::analyze` with OCR off. A panic or a hang is a finding; an error is not.

```sh
cargo install cargo-fuzz
cargo +nightly fuzz run analyze -- -max_len=200000
```

Seed the corpus before the first run so the fuzzer starts from valid PDFs:

```sh
mkdir -p fuzz/corpus/analyze
cp tests/fixtures/*.pdf fuzz/corpus/analyze/
```

`fuzz/corpus/` and `fuzz/artifacts/` (the crashing inputs) are not committed. CI only compiles the target (`cargo +nightly fuzz build`), so it cannot drift from the library's API.

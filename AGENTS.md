# Repository Instructions

Read [README.md](README.md) first: it describes the API, how fields are read and how to run the checks. `CLAUDE.md` is a symlink to this file.

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, `cargo test` and `RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items` before committing; CI runs the same with `--locked`, plus `cargo deny check` and `cargo +nightly fuzz build`.
- The lints are part of the check: `[lints]` in `Cargo.toml` turns on `clippy::pedantic`, `missing_docs` and `clippy::unwrap_used`. No `#[allow]` without a comment that says why; `unwrap` and `expect` only in tests.
- Every `pub` item has a doc comment, fields and variants included. Internal modules (`layout`, `line_items`, `seller`, `profile`, `numbers`, `dates`, `tax`, `currency`) are `pub(crate)`; their API types are re-exported from `candidates`.
- Never commit real documents or text taken from them. Tests build their PDFs and lines in code with the helpers in `src/test_support.rs` (`pdf_with_attachments`, `item`, `german`, `english`); a helper two modules need goes there, not into a module's `tests`.
- Candidates are proposals: prefer leaving a field empty over returning a guess. Every heuristic in `src/candidates.rs` gets a test that names the layout it handles.
- `tests/sinabis_candidates.rs` is the golden test. Its thresholds are only raised; lowering one needs a comment that explains why.
- Words of a language belong in its profile in `profiles/`, never in the code; the code holds only what does not depend on the language.
- Static patterns use the `regex!` macro from `src/lib.rs`; no `Regex::new` in a function body.
- Configuration is read only in `src/config.rs`, and every variable has a row in the README's table. No module of the service reads the environment itself.
- The service is stateless. It must not write received documents to disk or log their content; no log line carries a header or a body.
- E-invoice XML is returned as is; parsing it belongs to the consuming application.
- The API is described with utoipa: a changed response type or route needs its annotations updated and `cargo run -- openapi > openapi.json`.
- The JWT tests generate their RSA keys at runtime (`auth::tests`); never commit keys.
- `lopdf` stays on the version pdf-inspector pins; bump both together.

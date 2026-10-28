# Repository Instructions

Read [README.md](README.md) first: it describes the API, how fields are read and how to run the checks. `CLAUDE.md` is a symlink to this file.

- Run `cargo fmt`, `cargo clippy --all-targets -- -D warnings` and `cargo test` before committing; CI runs the same, plus `cargo deny check`.
- Never commit real documents or text taken from them. Tests build their PDFs in code (`attachments::tests::pdf_with_attachments`) or their lines from positioned items (`layout::tests::item`).
- Candidates are proposals: prefer leaving a field empty over returning a guess. Every heuristic in `src/candidates.rs` gets a test that names the layout it handles.
- The service is stateless. It must not write received documents to disk or log their content.
- E-invoice XML is returned as is; parsing it belongs to the consuming application.
- The JWT tests generate their RSA keys at runtime (`auth::tests`); never commit keys.

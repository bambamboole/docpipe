# docpipe

A document analysis service for invoices and quotes, read in German and English and open to more languages through [language profiles](#language-profiles). It takes an incoming document and returns what a person needs to file it:

- the PDF's classification (text-based, scanned, image-based, mixed) and the pages that need OCR,
- the text of every page, line by line, and its Markdown, from the text layer or from OCR for scanned pages,
- embedded files, and the ZUGFeRD/Factur-X or XRechnung XML among them, returned as is,
- field candidates of invoices and quotes: document kind (invoice, quote, credit note), invoice kind (partial, final), number, issue date, due date, valid-until date, net/VAT/gross amounts with their tax lines, currency, seller, IBANs and VAT IDs,
- their line items: position number, text, quantity and unit, unit price, total, tax rate, section and whether a position is optional.

Text extraction and OCR build on [pdf-inspector](https://github.com/firecrawl/pdf-inspector): pages without a usable text layer go to PP-OCRv6 Small (rendered with PDFium, run on ONNX Runtime); text-based PDFs never load either. [docs/ocr-benchmark.md](docs/ocr-benchmark.md) explains why this engine and not Tesseract. E-invoice XML is not parsed here; that is left to a dedicated e-invoice library in the consuming application. Candidates are proposals for a person to confirm; a field that cannot be read with confidence stays empty.

The service is stateless and keeps nothing it receives.

## Quick start

```sh
docker run -p 8080:8080 -e AUTH_MODE=none ghcr.io/bambamboole/docpipe:latest
```

## API

[`openapi.json`](openapi.json) describes the API as OpenAPI 3.1, and every instance serves it at `GET /openapi.json` without a token. `GET /docs` renders it with the [Lattice API reference](https://www.npmjs.com/package/@lattice-php/api-reference), whose playground sends requests to the instance itself; the page loads the reference from jsDelivr, pinned by version and hash. `docpipe openapi` prints the document from the code; `cargo run -- openapi > openapi.json` regenerates the committed one, and a test fails when it is stale.

```http
POST /v1/analyze
Authorization: Bearer <access token>

<the PDF or XML as the raw request body>
```

The answer is JSON:

```jsonc
{
  "format": "pdf",                 // or "xml" for a standalone e-invoice
  "pdf_type": "text_based",        // text_based | scanned | image_based | mixed
  "page_count": 2,
  "text_source": "native",         // native | ocr | mixed | none (OCR needed but off or failed)
  "pages_needing_ocr": [],
  "pages": [{ "number": 1, "text": "…", "markdown": "…", "needs_ocr": false, "ocr": false }],
  "attachments": [{ "filename": "factur-x.xml", "mime_type": "text/xml", "size": 12345 }],
  "einvoice": { "syntax": "cii", "filename": "factur-x.xml", "xml": "<rsm:CrossIndustryInvoice …" },
  "candidates": {
    "profile": "de",               // the language profile the document was read with
    "document_kind": "invoice",    // invoice | quote | credit_note | other
    "invoice_kind": "partial",     // standard | partial | final, invoices only
    "number": { "value": "RE-2026-0042", "page": 1, "line": "Nr.: RE-2026-0042" },
    "issue_date": { "value": "2026-09-07", "page": 1, "line": "…" },
    "due_date": { "value": "2026-09-17", "page": 2, "line": "…" },
    "valid_until": null,
    "amounts": {
      "net": "3250.00", "tax": "617.50", "gross": "3867.50", "currency": "EUR",
      "tax_lines": [{ "rate": "19", "net": "3250.00", "tax": "617.50" }],
      "consistent": true
    },
    "ibans": ["DE89370400440532013000"],
    "vat_ids": ["DE123456789"],
    "seller_name": { "value": "Musterbau GmbH & Co. KG", "page": 1, "line": "Musterbau GmbH & Co. KG · Hauptstr. 1 · 12345 Musterstadt" },
    "line_items": [{
      "position": "1.2", "description": "Kran vorhalten …",
      "quantity": "3", "unit": "Wochen", "unit_price": "410.00", "total": "1230.00",
      "tax_rate": null, "optional": false, "section": "1. Baustelleneinrichtung", "page": 2
    }]
  },
  "ocr": {
    "status": "not_needed",        // not_needed | disabled | applied | failed (native result returned)
    "pages": [],                   // OCR pages: number, markdown, confidence, weak
    "error": null,
    "render_ms": 0,
    "ocr_ms": 0
  },
  "processing_ms": 12
}
```

Every error, the 413 included, answers with `{"error": "<code>", "message": "…"}`. The message is a sentence for people; match on the code:

| Status | `error` | When |
|---|---|---|
| 400 | `empty_body` | The body is empty |
| 400 | `unreadable_body` | The body could not be read to its end |
| 401 | `unauthorized` | The bearer token is missing or invalid |
| 413 | `payload_too_large` | The body is larger than `DOCPIPE_MAX_BODY_MB` |
| 415 | `unsupported_format` | Neither a PDF nor a CII/UBL XML |
| 422 | `unreadable_pdf` | The bytes start like a PDF, but it cannot be read |
| 500 | `internal_error` | The analysis panicked; the service keeps running |
| 503 | `auth_unavailable` | The issuer's JWKS cannot be fetched |
| 503 | `overloaded` | More than `DOCPIPE_MAX_CONCURRENT` plus `DOCPIPE_MAX_QUEUED` requests are in flight; retry later |
| 503 | `timeout` | No analysis slot freed up, or the analysis did not finish, within `DOCPIPE_TIMEOUT_SECS` |

The reason behind a `401` or `422` goes to the log, not to the client. The token is checked and the request admitted before its body is read, so an unauthenticated or shed request costs no memory. Every response carries an `x-request-id` header: the client's when it sent one, a UUID otherwise. The log lines of the request carry the same id. `GET /health` answers `ok` without a token.

### How fields are read

Lines are rebuilt from pdf-inspector's positioned text runs, keeping each run's horizontal position. A value is looked up after its label in the same run, then to the right on the same line, then in the label's column on the next three lines; when several labels share one run, as in a table header, the label's column is estimated from its position in the run. Amounts prefer tax lines that add up: for each tax rate the document prints (`19 %`, `7,5%`, `Sales Tax (8.625%)`), a net amount and its tax at that rate. Together, the net and tax amounts must sum to a gross amount on the document. No rate is assumed from a country, so a document that prints no rate has no tax lines. With several rates, the total net or the total tax must be printed too. This also covers layouts whose labels and values sit in separate cells. Without such tax lines, only labelled amounts are returned: none on a document that names two currencies, since their currency is open, and no net amount that is not below the gross amount. Amounts keep the sign they are printed with, so a credit note's may be negative. IBANs are checked with their ISO 13616 check digits; VAT identification numbers are read in the formats of the EU countries, the United Kingdom, Switzerland and Norway.

Numbers and dates are read in the format the document uses. Whether amounts carry a decimal comma (`1.234,56`, `1 234,56`) or a decimal point (`1,234.56`, `1'234.56`) is decided by the amounts with two decimals on the document; only when they do not tell, the language profile's mark decides. ISO dates, dotted dates (day first) and dates with a written month are unambiguous. A date with slashes takes its order from a date on the same document whose day exceeds 12; otherwise an issue and a due date take the order that puts the due date after the issue date, and when both orders do, or a date stands alone, it stays empty. The currency is the one ISO 4217 code or unambiguous symbol (`€`, `£`, `US$`) written next to the document's numbers; a bare `$`, or two currencies on one document, leave it empty.

Line items start at a line whose first span is a position number alone (`1.2`, `01.11.01.01`, `10073`) in the column of the first one. The quantity is a span of a number and a unit, or a lump-sum word (`pauschal`, `psch`), on the position's first or last line; the amounts to its right are the unit price and the total, a single one the total. Following lines in the text's column belong to the position; header and footer lines repeated on several pages, carried-over subtotals and title sums are skipped. Text between positions names their section, and a section such as `Eventualpositionen` or `Optionale Positionen (nicht im Basispreis enthalten)`, or a marker such as `Bedarf`, makes its positions optional. A rate in a span of its own on a position (`19 %`) is its tax rate when it is the rate of one of the document's tax lines; a percentage they do not confirm, such as a discount, is not. A document with fewer than two positions has none.

The seller is the company named with a legal form of the profile (`GmbH`, `GmbH & Co. KG`, `Ltd`, `(Pty) Ltd`, `LLC`, …) most often on the document, from the sender line, the page footers and the terms; a name in the first lines of the first page counts three times. A name is read within one text run, so a title beside the sender line is no part of it. A form that also abbreviates other words (`KG`, the basement; `Co`, `Inc`) only counts at the end of a name. A name under the heading of the recipient's address (`Bill To`, `Lieferanschrift`) or after it in the same run is the customer's and never the seller. A private sender has no legal form, so the field stays empty.

OCR pages have no positions in the response; their lines are rebuilt from the OCR Markdown, table cells becoming columns, so labels still find their values. The OCR confidence stays high even on poor scans and is no quality gate: rely on the field checks (consistent amounts, IBAN check digits) instead.

### Language profiles

The words fields are read by live in language profiles, one TOML file per language in [`profiles/`](profiles), compiled into the binary: the labels of each field, the words that tell invoices, quotes and credit notes apart, the vocabulary of line items, the legal forms of companies, the names of the months and the decimal mark to assume when a document does not show one. Everything else does not depend on the language: the layout, number and date formats, currencies, IBANs, VAT numbers, e-invoices. The bundled profiles are German (`de`) and English (`en`). Each document is read with the profile whose labels and document words occur most often on it, German on a tie, and the response names it in `candidates.profile`.

To add a language, copy `profiles/en.toml`, translate its patterns and word lists, add the file to `SOURCES` in `src/profile.rs`, and add tests that build documents in that language and name the layouts they cover. A profile that fails to parse or has an invalid pattern fails `cargo test`.

## Authentication

As in [pdf-ua-api](https://github.com/bambamboole/pdf-ua-api), docpipe only verifies tokens, it never issues them. `AUTH_MODE` has no default, so the service does not start until it names one of three modes:

- `jwt`: a request needs `Authorization: Bearer <JWT>` signed with RS256 by a key of the JWKS at `JWT_JWKS_URL`, named by the token's `kid`, with `JWT_ISSUER` as `iss`, an `exp` in the future, an `nbf` not in the future when present, the `aud` from `JWT_AUDIENCE` when set, and a `client_id` from `JWT_ALLOWED_CLIENTS` when set. This fits OAuth 2.0 access tokens (RFC 9068) of the client credentials grant.
- `api_key`: a request needs `Authorization: Bearer <API_KEY>`. The key is compared in constant time.
- `none`: every request is accepted, and startup logs a warning. Use it only for local development or behind a gateway that authenticates.

`/health`, `/openapi.json` and `/docs` are always public.

With `JWT_AUDIENCE` set, a token without an `aud` claim is rejected, not only one with another audience. The algorithm is pinned to RS256 before the signature is looked at, so tokens with another algorithm or `alg: none`, and tokens without a `kid`, are rejected (RFC 8725).

The JWKS is fetched at startup. When the issuer is unreachable, a warning is logged, the service starts anyway, and the first request fetches the keys again. A token that names an unknown key triggers a refresh, at most once a minute, and a failed refresh keeps the keys already cached. The fetch runs on a blocking thread, so other requests are not stalled by it.

## Configuration

Every variable is read at startup, in `src/config.rs`. A value that cannot be used stops the service from starting instead of being replaced by a default: a port that is not a number, `DOCPIPE_MAX_CONCURRENT=0`, a `DOCPIPE_OCR` that is neither `on` nor `off`, a `SENTRY_DSN` that does not parse, a model directory without its models. A blank value counts as unset.

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8080` | Listening port; `docpipe health` asks this port |
| `AUTH_MODE` | – | Required: `jwt`, `api_key` or `none` |
| `JWT_ISSUER`, `JWT_JWKS_URL` | – | Required with `jwt`: the expected issuer and the URL of its public keys |
| `JWT_AUDIENCE` | – | Expected `aud`, which the token must then carry; unset, the audience is not checked |
| `JWT_ALLOWED_CLIENTS` | – | Comma-separated `client_id` claims to accept; unset, every client of the issuer is accepted |
| `API_KEY` | – | Required with `api_key`: the static bearer token |
| `DOCPIPE_MAX_BODY_MB` | `50` | Largest accepted document, in MiB; at least 1 |
| `DOCPIPE_MAX_CONCURRENT` | CPU count | Native analyses that run at once; further requests wait for a slot; at least 1 |
| `DOCPIPE_MAX_QUEUED` | 2 × `DOCPIPE_MAX_CONCURRENT` | Requests admitted beyond the running ones to wait for a slot; further requests are shed with `503 overloaded` before their body is read. `0` sheds every request beyond the running ones |
| `DOCPIPE_TIMEOUT_SECS` | `300` | How long a request may wait for a slot and its analysis; at least 1 |
| `DOCPIPE_OCR` | `on` | `off` disables OCR; pages that need it are reported with `ocr.status: "disabled"` |
| `DOCPIPE_OCR_MODEL_DIR` | – | Complete PP-OCRv6 Small model directory, checked at startup for `pp-ocrv6_small_det.onnx`, `pp-ocrv6_small_rec.onnx` and `ppocrv6_dict.txt`; without it, startup logs a warning and models are downloaded into the cache on first use. Set in the Docker image. |
| `PDFIUM_LIB_PATH`, `ORT_DYLIB_PATH` | – | PDFium and ONNX Runtime shared libraries for OCR, checked at startup to exist. Set in the Docker image. |
| `RUST_LOG` | `info,lopdf=error,ort=warn` | Log filter; logs are JSON on stdout |
| `SENTRY_DSN` | – | Report to a Sentry-compatible server such as [Rustrak](https://github.com/rustrak/rustrak); unset, nothing is sent |
| `SENTRY_RELEASE` | `docpipe@<version>` | Release of the reports |
| `SENTRY_ENVIRONMENT` | – | Environment of the reports |
| `SENTRY_LOGS` | `off` | Lowest level sent as structured logs: `error`, `warn`, `info` or `debug` |

The OCR paths are only checked while `DOCPIPE_OCR` is `on`; the binaries without an OCR runtime start with `DOCPIPE_OCR=off`.

## Logging and error reporting

Logs are JSON on stdout, filtered by `RUST_LOG`. Every request writes one access log line at `INFO` with its method, path, request id, status and latency. No log line carries a header or a body, so neither the token nor the document reaches the log. A rejected request logs its reason at `INFO`, which the client does not get; a timed out analysis logs at `WARN`; an unreachable JWKS and a panicking analysis log at `ERROR`.

With `SENTRY_DSN` set, docpipe reports to Sentry or any server that speaks its protocol. `ERROR` events become issues with the `INFO` events before them as breadcrumbs, and panics are reported too. `SENTRY_LOGS` additionally sends every event from that level on as a structured log. `RUST_LOG` filters both stdout and Sentry. Reports carry no request data and, like the logs, no document content. Release builds keep line tables, so a backtrace names file and line.

## Development

Requires Rust 1.95. CI runs the same checks with `--locked`, plus `cargo deny check` and a nightly build of the fuzz target.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --document-private-items
cargo deny check
AUTH_MODE=none cargo run --release
curl --data-binary @invoice.pdf http://localhost:8080/v1/analyze
```

The lints are set in `[lints]` of `Cargo.toml`: `clippy::pedantic`, `missing_docs` and `clippy::unwrap_used` warn, and `-D warnings` turns them into errors. So every public item has a doc comment, `unwrap` and `expect` stay in tests (`clippy.toml`), `unsafe` is forbidden, and a lint that is wrong for one line gets an `#[allow]` with a comment that says why. `cargo doc` fails on a broken intra-doc link. `lopdf` is pinned to the version pdf-inspector uses, so the binary holds one copy of it; bump both together.

Never commit real invoices: they contain personal data. Unit tests build their PDFs and their lines in code with the helpers in `src/test_support.rs`. `tests/fixtures/` holds fictional corpora: the [Invoice Extraction Benchmark v1](tests/fixtures/README.md) PDF subset for the smoke test in `tests/external_invoice_pdfs.rs`, and the [Sinabis samples](docs/sinabis-sample.md) for scan routing and e-invoices in `tests/sinabis_corpus.rs`.

`tests/sinabis_candidates.rs` is the golden test of the field candidates. It parses the CII XML of each of the 100 Sinabis e-invoices and compares the candidates read from the PDF's text layer with it: document kind, number, issue and due date, gross, net and tax, the seller's VAT ID and name, and the number of line items. Every field has a minimum accuracy, and the fields that must not guess (`number`, `issue_date`, `due_date`, `gross`) a maximum share of wrong values. A regression fails the test; an improvement passes it and raises the threshold. `cargo test --test sinabis_candidates -- --nocapture` prints the table: per field, the documents whose XML has a value (`applicable`), the candidates that are `correct`, `missing` or `wrong`, the accuracy over the applicable documents and the share of wrong values over all 100. Below it, every mismatch is listed as `file: expected …, got …`, the candidate with the page and the line it was read from.

`fuzz/` holds a cargo-fuzz target that feeds arbitrary bytes to the analysis with OCR off; a panic or a hang is a finding. CI only compiles it. [fuzz/README.md](fuzz/README.md) says how to run it.

## Installation

Each [release](https://github.com/bambamboole/docpipe/releases) publishes a container image and binaries.

```sh
docker run -p 8080:8080 -e AUTH_MODE=api_key -e API_KEY=change-me ghcr.io/bambamboole/docpipe:latest
```

The image is built for linux/amd64 and linux/arm64 and contains everything OCR needs. It runs as an unprivileged user. Its `HEALTHCHECK` runs `docpipe health` every 30 s: the binary requests `/health` on `PORT` and exits 0 on `200`, so the image carries no curl. The [Dockerfile](Dockerfile) pins its base images by digest and keeps the cargo registry and the target directory in BuildKit cache mounts, so a rebuild compiles only what changed. The binaries (Linux x86_64 and arm64, macOS arm64) bring no OCR runtime: set `PDFIUM_LIB_PATH` and `ORT_DYLIB_PATH` to a PDFium and an ONNX Runtime library, or switch OCR off with `DOCPIPE_OCR=off`. The Dockerfile names the tested versions.

Releases come from [release-please](https://github.com/googleapis/release-please): it keeps a release pull request open that bumps the version in `Cargo.toml` and writes `CHANGELOG.md` from the conventional commits on `main`. Merging it tags `v<version>` and creates the release, and the tag builds the binaries and the image. A `feat!` or `BREAKING CHANGE` raises the minor version while docpipe is below 1.0.

## Deployment

`compose-production.yml` runs the image without a public port, for services in the same Docker network, with JWT authentication, the `docpipe health` healthcheck and a memory limit of 1.5 GiB.

The image ships PDFium `native-v7988`, ONNX Runtime 1.27.0 and the PP-OCRv6 Small models (`oar-ocr-v0.7.0`), each verified by SHA-256 at build time; it never downloads anything at runtime. CI runs the OCR corpus on native amd64 and arm64 Docker runners.

Memory is budgeted as `DOCPIPE_MAX_CONCURRENT` native extractions plus one OCR run. An admitted request holds its body, up to `DOCPIPE_MAX_BODY_MB`, while it waits for a slot, so `DOCPIPE_MAX_CONCURRENT` plus `DOCPIPE_MAX_QUEUED` bodies may be buffered at once. A native extraction loads the PDF with pdf-inspector and lopdf; one compressed stream may inflate to at most 256 MiB, so a decompression bomb does not exhaust memory. OCR runs one document at a time: after it, ONNX Runtime keeps its memory pools, and in production the container peaked at 1.12 GiB and stayed at 1.1 GiB (1.2 MiB idle before the first OCR), hence the 1.5 GiB limit. A document whose pages need OCR first goes through native extraction in one of the `DOCPIPE_MAX_CONCURRENT` slots, then waits for the single OCR slot without holding a native slot, so a burst of scans does not stall text PDFs; the wait ends with the request's `DOCPIPE_TIMEOUT_SECS`.

## Roadmap

- Positions for OCR text, so labels and values in OCR tables pair by column instead of by Markdown order.

## License

[MIT](LICENSE)

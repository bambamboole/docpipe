# docpipe

A document analysis service for German invoices and quotes. It takes an incoming document and returns what a person needs to file it:

- the PDF's classification (text-based, scanned, image-based, mixed) and the pages that need OCR,
- the text of every page, line by line, and its Markdown — from the text layer, or from OCR for scanned pages,
- embedded files, and the ZUGFeRD/Factur-X or XRechnung XML among them, returned as is,
- field candidates of German invoices and quotes: document kind (invoice, quote, credit note), invoice kind (partial, final), number, issue date, due date, valid-until date, net/VAT/gross amounts, IBANs and VAT IDs,
- their line items: position number, text, quantity and unit, unit price, total, section and whether a position is optional.

Text extraction and OCR build on [pdf-inspector](https://github.com/firecrawl/pdf-inspector): pages without a usable text layer go to PP-OCRv6 Small (rendered with PDFium, run on ONNX Runtime); text-based PDFs never load either. [docs/ocr-benchmark.md](docs/ocr-benchmark.md) explains why this engine and not Tesseract. E-invoice XML is not parsed here; that is left to a dedicated e-invoice library in the consuming application. Candidates are proposals for a person to confirm; a field that cannot be read with confidence stays empty.

The service is stateless and keeps nothing it receives.

## API

[`openapi.json`](openapi.json) describes the API as OpenAPI 3.1, and every instance serves it at `GET /openapi.json` without a token. `cargo run -- openapi > openapi.json` regenerates it from the code; a test fails when the committed document is stale.

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
    "document_kind": "invoice",    // invoice | quote | credit_note | other
    "invoice_kind": "partial",     // standard | partial | final, invoices only
    "number": { "value": "RE-2026-0042", "page": 1, "line": "Nr.: RE-2026-0042" },
    "issue_date": { "value": "2026-09-07", "page": 1, "line": "…" },
    "due_date": { "value": "2026-09-17", "page": 2, "line": "…" },
    "valid_until": null,
    "amounts": { "net": "3250.00", "tax": "617.50", "gross": "3867.50", "tax_rate": 19, "consistent": true },
    "ibans": ["DE89370400440532013000"],
    "vat_ids": ["DE123456789"],
    "seller_name": { "value": "Musterbau GmbH & Co. KG", "page": 1, "line": "Musterbau GmbH & Co. KG · Hauptstr. 1 · 12345 Musterstadt" },
    "line_items": [{
      "position": "1.2", "description": "Kran vorhalten …",
      "quantity": "3", "unit": "Wochen", "unit_price": "410.00", "total": "1230.00",
      "optional": false, "section": "1. Baustelleneinrichtung", "page": 2
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

Errors answer with `{"error": "<code>", "message": "…"}`: `401 unauthorized`, `503 auth_unavailable` (the issuer's JWKS cannot be fetched), `400 empty_body`, `415 unsupported_format` (neither PDF nor CII/UBL XML), `422 unreadable_pdf`, `503 timeout` (no analysis slot freed up, or the analysis did not finish within `DOCPIPE_TIMEOUT_SECS`). `GET /health` answers `ok` without a token. A body larger than `DOCPIPE_MAX_BODY_MB` answers `413` without JSON.

### How fields are read

Lines are rebuilt from pdf-inspector's positioned text runs, keeping each run's horizontal position. A value is looked up after its label in the same run, then to the right on the same line, then in the label's column on the next three lines; when several labels share one run, as in a table header, the label's column is estimated from its position in the run. Amounts prefer a consistent triple — net and VAT at 19 % or 7 % that add up to a gross amount on the document — which also covers layouts whose labels and values sit in separate cells. Without such a triple, only labelled amounts are returned. IBANs are checked with their ISO 13616 check digits.

Line items start at a line whose first span is a position number alone (`1.2`, `01.11.01.01`, `10073`) in the column of the first one. The quantity is a span of a number and a unit, or a lump-sum word (`pauschal`, `psch`), on the position's first or last line; the amounts to its right are the unit price and the total, a single one the total. Following lines in the text's column belong to the position; header and footer lines repeated on several pages, carried-over subtotals and title sums are skipped. Text between positions names their section, and a section such as `Eventualpositionen` or `Optionale Positionen (nicht im Basispreis enthalten)`, or a marker such as `Bedarf`, makes its positions optional. A document with fewer than two positions has none.

The seller is the company named with a legal form (`GmbH`, `GmbH & Co. KG`, `AG`, `e. K.`, …) most often on the document, from the sender line, the page footers and the terms; a name in the first lines of the first page counts three times. A bare `KG`, `AG`, `SE` or `OHG` only counts at the end of a name, since `KG` also means the basement. A private sender has no legal form, so the field stays empty.

OCR pages have no positions in the response; their lines are rebuilt from the OCR Markdown, table cells becoming columns, so labels still find their values. The OCR confidence stays high even on poor scans and is no quality gate: rely on the field checks (consistent amounts, IBAN check digits) instead.

## Authentication

As in [pdf-ua-api](https://github.com/bambamboole/pdf-ua-api), docpipe only verifies tokens, it never issues them. `AUTH_MODE` has no default, so the service does not start until it names one of three modes:

- `jwt`: a request needs `Authorization: Bearer <JWT>` signed with RS256 by a key of the JWKS at `JWT_JWKS_URL`, with `JWT_ISSUER` as `iss`, an `exp` in the future, the `aud` from `JWT_AUDIENCE` when set, and a `client_id` from `JWT_ALLOWED_CLIENTS` when set. This fits OAuth 2.0 access tokens (RFC 9068) of the client credentials grant.
- `api_key`: a request needs `Authorization: Bearer <API_KEY>`.
- `none`: every request is accepted. Use it only for local development or behind a gateway that authenticates.

`/health` is always public.

The JWKS is fetched on the first request and again when a token names an unknown key, at most once a minute.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `PORT` | `8080` | Listening port |
| `AUTH_MODE` | – | Required: `jwt`, `api_key` or `none` |
| `JWT_ISSUER`, `JWT_JWKS_URL` | – | Required with `jwt`: the expected issuer and the URL of its public keys |
| `JWT_AUDIENCE` | – | Expected `aud`; unset, the audience is not checked |
| `JWT_ALLOWED_CLIENTS` | – | Comma-separated `client_id` claims to accept; unset, every client of the issuer is accepted |
| `API_KEY` | – | Required with `api_key`: the static bearer token |
| `DOCPIPE_MAX_BODY_MB` | `50` | Largest accepted document |
| `DOCPIPE_MAX_CONCURRENT` | CPU count | Analyses that run at once; further requests wait for a slot |
| `DOCPIPE_TIMEOUT_SECS` | `300` | How long a request may wait for a slot and its analysis |
| `DOCPIPE_OCR` | on | `off` disables OCR; pages that need it are reported with `ocr.status: "disabled"` |
| `DOCPIPE_OCR_MODEL_DIR` | – | Complete PP-OCRv6 Small model directory; without it, models are downloaded into the cache on first use. Set in the Docker image. |
| `PDFIUM_LIB_PATH`, `ORT_DYLIB_PATH` | – | PDFium and ONNX Runtime shared libraries for OCR. Set in the Docker image. |
| `RUST_LOG` | `info,lopdf=error,ort=warn` | Log filter; logs are JSON on stdout |
| `SENTRY_DSN` | – | Report to a Sentry-compatible server such as [Rustrak](https://github.com/rustrak/rustrak); unset, nothing is sent |
| `SENTRY_RELEASE` | `docpipe@<version>` | Release of the reports |
| `SENTRY_ENVIRONMENT` | – | Environment of the reports |
| `SENTRY_LOGS` | `off` | Lowest level sent as structured logs: `error`, `warn`, `info` or `debug` |

## Error reporting

With `SENTRY_DSN` set, docpipe reports to Sentry or any server that speaks its protocol. `ERROR` events become issues with the `INFO` events before them as breadcrumbs, and panics are reported too. `SENTRY_LOGS` additionally sends every event from that level on as a structured log. `RUST_LOG` filters both stdout and Sentry. Reports carry no request data and, like the logs, no document content.

## Development

Requires Rust 1.95.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check
AUTH_MODE=none cargo run --release
curl --data-binary @invoice.pdf http://localhost:8080/v1/analyze
```

Never commit real invoices: they contain personal data. Tests build their PDFs in code.

## Installation

Each [release](https://github.com/bambamboole/docpipe/releases) publishes a container image and binaries.

```sh
docker run -p 8080:8080 -e AUTH_MODE=api_key -e API_KEY=change-me ghcr.io/bambamboole/docpipe:latest
```

The image is built for linux/amd64 and contains everything OCR needs. The binaries (Linux x86_64 and arm64, macOS arm64) bring no OCR runtime: set `PDFIUM_LIB_PATH` and `ORT_DYLIB_PATH` to a PDFium and an ONNX Runtime library, or switch OCR off with `DOCPIPE_OCR=off`. The [Dockerfile](Dockerfile) names the tested versions.

Releases are cut by pushing a tag `v<version>` that matches the version in `Cargo.toml`.

## Deployment

`compose-production.yml` runs the image without a public port, for services in the same Docker network, with JWT authentication.

The image ships PDFium `native-v7988`, ONNX Runtime 1.27.0 and the PP-OCRv6 Small models (`oar-ocr-v0.7.0`), each verified by SHA-256 at build time; it never downloads anything at runtime. It is built for linux/amd64, the platform pdf-inspector's OCR path is tested on.

After OCR, ONNX Runtime keeps its memory pools: in production the container peaked at 1.12 GiB and stayed at 1.1 GiB (1.2 MiB idle before the first OCR). OCR runs one document at a time, so `compose-production.yml` limits the container to 1.5 GiB.

## Roadmap

- Positions for OCR text, so labels and values in OCR tables pair by column instead of by Markdown order.

## License

[MIT](LICENSE)

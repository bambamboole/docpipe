# OCR engine comparison (2026-10-26)

Why docpipe runs OCR with PP-OCRv6 Small through pdf-inspector and not with Tesseract. All runs were local on an Apple M1 Max; no document left the machine. The corpus consists of real German invoices and quotes and is not part of this repository.

## Setup

- **A: pdf-inspector `ocr` feature** — PP-OCRv6 Small (`oar-ocr-v0.7.0`), PDFium `native-v7988`, ONNX Runtime 1.27.0, rendered at 200 dpi, `--ocr force`.
- **B: Tesseract 5.5.2** — `deu+eng`, `--psm 3`, tessdata_best; tessdata_fast and `--psm 4` as variants.
- **Corpus:** 11 text-layer PDFs (3 invoices, 8 quotes), first two pages each, 21 pages. The text layer (`pdftotext -layout`) is the ground truth.
- **Variants:** each page rendered at 200 dpi grayscale, once clean and once degraded (0.5–1.2° rotation, blur, noise, JPEG quality 60).
- **Metrics:** character and word error rate, and field recall — exact matches of the ground truth's amounts, dates, IBANs, VAT IDs, words with umlauts or ß, and € signs.

On 8 pages both engines read 30–90 % more words than the text layer contains (letterheads drawn as graphics). Error rates are therefore also given for the 13 reliable pages; field recall only counts fields in the ground truth and uses all 21 pages.

## Field recall (629 fields on 21 pages)

| Engine | Amounts | Umlaut/ß words | € | All fields |
|---|---|---|---|---|
| **A clean** | 99.4 % | 95.9 % | 100 % | **97.1 %** |
| **A degraded** | 96.0 % | 96.1 % | 99 % | **96.3 %** |
| B best, clean | 92.1 % | 86.2 % | 84 % | 88.6 % |
| B best, degraded | 90.4 % | 78.9 % | 83 % | 83.1 % |
| B fast, clean | 92.7 % | 93.0 % | 84 % | 93.2 % |
| B fast, degraded | 91.0 % | 91.3 % | 83 % | 91.4 % |

Dates, IBANs and VAT IDs were found almost completely by both. No engine produced a well-formed amount that is not on the page: a miss is a missing or malformed value, not a plausible wrong one.

## Text accuracy (13 reliable pages)

| Engine | CER | Order-independent CER | WER |
|---|---|---|---|
| A clean / degraded | 0.092 / 0.121 | 0.074 / 0.075 | 0.155 / 0.186 |
| B best clean / degraded | 0.078 / 0.084 | 0.054 / 0.072 | 0.132 / 0.152 |
| B fast clean / degraded | 0.077 / 0.082 | 0.055 / 0.066 | 0.125 / 0.140 |

Tesseract is about two points better on general text, and part of A's error is reading order in tables. But A's errors fall on ordinary words, Tesseract's on the fields that matter.

## Failure patterns

- **A:** umlaut words off by one or two characters; on degraded pages a few amounts lose a digit. Table cells come out in a different order than the text layer. Its confidence stays at 0.98–0.99 even on degraded pages: it is no quality gate.
- **B:** skips whole right-aligned number columns and totals (12–13 amounts and 13 € signs missing with every model and segmentation mode), and replaces umlauts by plain letters, more often on degraded pages. tessdata_best was worse than tessdata_fast on umlauts.

## A real scan

A two-page scanned quote without a text layer: both engines found exactly the same 13 amounts; A found more umlaut words. Through docpipe, A yields the document's total as a consistent net/VAT/gross triple.

## Cost

| | A | B |
|---|---|---|
| Time per page, warm process | 1.6–1.8 s (pages in parallel) | 1.2 s fast, 3 s best |
| CPU per page | about 6 s | about 1–3 s |
| Model load | about 1.1 s once per process | 0.12 s |
| Footprint | about 79 MB (PDFium, ONNX Runtime, models) | 20–40 MB |

## Decision

PP-OCRv6 through pdf-inspector: about 97 % field recall against 83–93 % for Tesseract, robust on poor scans, and one Rust dependency that only touches the OCR runtime when a page needs it. Run it in a long-lived process, pin the runtime versions, ship the models offline, and validate fields instead of trusting the confidence. Tesseract with tessdata_fast is the fallback if CPU or footprint must be minimal.

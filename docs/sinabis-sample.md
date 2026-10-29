# Sinabis invoice sample

[Sinabis Green Energy – Synthetic Document Dataset (1M PDFs)](https://huggingface.co/datasets/sinabis-group/sinabis-1m-synthetic-pdf) contains 1,006,000 fictional business PDFs, including 281,680 invoices. The dataset is licensed CC BY 4.0. Attribution: Sinabis GmbH (2026), *Sinabis Green Energy – Synthetic Document Dataset (1M PDFs)*, https://huggingface.co/datasets/sinabis-group/sinabis-1m-synthetic-pdf.

To fetch a reproducible sample of 100 German invoices:

```sh
python3 scripts/sample_sinabis.py --count 100 --lang de --seed 42
```

The script uses only Python's standard library. It downloads the 74 MB manifest ZIP, then reads only the ZIP index and selected PDF byte ranges from the first 9.86 GB archive. It does not download that entire archive. The first archive has 679 German invoices in the manifest: 471 vector and 208 scanned. The selection uses reservoir sampling with 50 of each and a fixed seed. Use `--output` to choose a different local directory.

The committed corpus is `tests/fixtures/sinabis-ocr/`. `selection.jsonl` records each selected file's source index, language, scan flag, document number, date, currency, total, byte count and SHA-256. The PDFs keep their original relative paths under `pdf/invoice/`.

The current 100 PDFs occupy about 9.0 MB. The 74 MB manifest stays in Git-ignored `benchmark-data/` and can be downloaded again with:

```sh
python3 scripts/sample_sinabis.py --count 100 --lang de --seed 42 \
  --output tests/fixtures/sinabis-ocr \
  --manifest benchmark-data/sinabis/Sinabis_1M_pdfs_manifest.zip
```

The manifest provides document-level metadata, not complete field-level truth. In particular, it has no VAT, due date, seller validation or line-item annotations. Its `total` must be checked against the rendered invoice before treating it as the API's gross amount. The sample is useful for PDF routing, OCR coverage and manual review of candidate fields. Never add real customer documents to this corpus.

## E-invoices with structured reference data

The separate [Sinabis 1M ZUGFeRD/XRechnung dataset](https://huggingface.co/datasets/sinabis-group/sinabis-1m-synthetic-zugferd-xrechnung) contains one million synthetic German E-invoices. Each PDF has an embedded CII XML document; the same XML is also distributed as a separate file. Its manifest contains invoice number, issue and due dates, seller, tax, totals, line count, and SHA-256 hashes. The XML contains the detailed positions. This is a separate corpus; do not pair its rows with the general PDF dataset by index.

Fetch 100 reproducible PDF/XML pairs from all seven archive parts:

```sh
python3 scripts/sample_sinabis_einvoices.py --count 100 --seed 42
```

The committed corpus is `tests/fixtures/sinabis-einvoices/`. The sampler downloads the 248 MB manifest once, chooses rows by stable random rank, then reads only selected ZIP members with HTTP range requests. It verifies the byte size and SHA-256 of every downloaded PDF and XML against the manifest. Increasing `--count` with the same seed keeps all previously selected rows. `selection.jsonl` contains the full source records. No Python packages are required beyond the standard library.

The current 100 PDF/XML pairs occupy about 6.7 MB. Regenerate them without placing the manifest in the repo:

```sh
python3 scripts/sample_sinabis_einvoices.py --count 100 --seed 42 \
  --output tests/fixtures/sinabis-einvoices \
  --manifest benchmark-data/sinabis-einvoices/Sinabis_1M_XRechnung3.0_ZUGFeRD_manifest.zip
```

For extraction benchmarking, parse the standalone XML **outside** docpipe to create expected fields and line items, then compare those with docpipe's PDF candidates. The service returns embedded XML unchanged; consuming applications own its parsing. Map `tax_basis` to taxable net, `tax_total` to tax, and `grand_total` to gross; `line_total` excludes document-level charges, and `due_payable` can differ from gross after prepayments. Validate the PDF/XML agreement on a small reviewed subset before treating the full corpus as a gold standard. The E-invoice PDFs have a text layer and therefore complement the general PDF sample's scanned OCR cases.

`cargo test --test sinabis_corpus` runs offline against all 200 PDFs. It checks scan routing and that the E-invoices return their embedded XML unchanged. The ignored `reads_all_sinabis_scans_with_ocr` test runs full OCR on the 50 scanned PDFs in the `ocr-test` Docker build stage, using the same pinned OCR runtime and models as production. CI builds and tests that stage on native amd64 and arm64 runners. The ordinary Rust test job needs no OCR assets.

The full general PDF dataset is 65.9 GB in seven ZIPs (71.8 GB unpacked). The E-invoice dataset is 49.54 GB in seven ZIPs (68.48 GB unpacked). Only the 200 selected PDFs, their 100 small XML references and selection records belong in Git. The pinned source revisions, selection seeds and manifest hashes make them reproducible. Both datasets are CC BY 4.0. Attribution for the E-invoice dataset: Sinabis GmbH (2026), *Sinabis Green Energy – 1M GoBD-compliant German E-Invoices (XRechnung 3.0 / ZUGFeRD)*, https://huggingface.co/datasets/sinabis-group/sinabis-1m-synthetic-zugferd-xrechnung.

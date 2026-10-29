# Synthetic invoice PDF corpus

The 113 PDFs in this directory are the PDF subset of [Invoice Extraction Benchmark v1](https://github.com/DrewKraken/invoice-extraction-benchmark) by InvoiceParser Pro, copied from `corpus/v1/documents/` at commit `3f8864d0d96103a279b5320000c69a71068edd14`. They are fictional test documents, not customer invoices.

The dataset is licensed [CC BY 4.0](https://github.com/DrewKraken/invoice-extraction-benchmark/blob/main/DATA_LICENSE). Attribution: InvoiceParser Pro, Invoice Extraction Benchmark v1, https://github.com/DrewKraken/invoice-extraction-benchmark.

The set contains invoices, credit notes, multi-invoice PDFs and statements. It has no quotes. Only `clean-de-grid-eu-decimals.pdf` and `multipage-5p-de-6.pdf` are explicitly German; most other documents are English. The original repository's `corpus/v1/manifest.json` and `corpus/v1/answers/` contain the field-level ground truth. This repository's smoke test checks PDF analysis, not that full answer schema.

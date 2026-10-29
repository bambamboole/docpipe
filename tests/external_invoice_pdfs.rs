//! Smoke test over the Invoice Extraction Benchmark v1 PDF subset in
//! `tests/fixtures` (see its README). The benchmark's `answers/` ground truth
//! is not in this repository, so the German documents only have to yield
//! candidates; run with `--nocapture` to eyeball their fields.

#![allow(clippy::print_stdout, clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use docpipe::analyze::{Analysis, Format, TextSource, analyze};
use docpipe::ocr::OcrSettings;

/// The benchmark's "German" invoices: German addresses, VAT IDs and decimals
/// (`1.151,18`), but English labels (`TAX INVOICE`, `Invoice Number:`,
/// `Date:`, `Due Date:`), so the German profile does not call them invoices.
const GERMAN_PDFS: &[&str] = &["clean-de-grid-eu-decimals.pdf", "multipage-5p-de-6.pdf"];

fn read(path: &Path) -> Analysis {
    let bytes = fs::read(path).unwrap();
    analyze(&bytes, &OcrSettings::default()).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn reads_invoice_extraction_benchmark_pdfs() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pdfs = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pdf"))
        .collect::<Vec<_>>();
    pdfs.sort();

    assert!(
        pdfs.len() >= 100,
        "the benchmark PDF subset is incomplete: {} PDFs",
        pdfs.len()
    );
    for name in GERMAN_PDFS {
        assert!(
            directory.join(name).is_file(),
            "{name} is missing from the benchmark subset"
        );
    }
    for path in pdfs {
        let analysis = read(&path);
        assert_eq!(analysis.format, Format::Pdf, "{}", path.display());
        assert!(analysis.page_count > 0, "{}", path.display());
        assert_eq!(analysis.pages.len(), analysis.page_count as usize, "{}", path.display());
        assert_eq!(analysis.text_source, TextSource::Native, "{}", path.display());
    }
}

#[test]
fn reads_candidates_of_the_german_benchmark_invoices() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    for name in GERMAN_PDFS {
        let analysis = read(&directory.join(name));
        let candidates = analysis.candidates.unwrap_or_else(|| panic!("{name}: no candidates"));
        println!(
            "{name}: kind {:?}, number {:?}, issue date {:?}, due date {:?}, gross {:?} (net {:?}, tax {:?}, consistent {})",
            candidates.document_kind,
            candidates.number.as_ref().map(|field| &field.value),
            candidates.issue_date.as_ref().map(|field| &field.value),
            candidates.due_date.as_ref().map(|field| &field.value),
            candidates.amounts.gross,
            candidates.amounts.net,
            candidates.amounts.tax,
            candidates.amounts.consistent,
        );
    }
}

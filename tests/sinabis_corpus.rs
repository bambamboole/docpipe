use std::fs;
use std::path::Path;

use docpipe::analyze::{Format, TextSource, analyze};
use docpipe::attachments::EInvoiceSyntax;
use docpipe::ocr::{OcrSettings, OcrStatus};
use serde_json::Value;

fn rows(directory: &Path) -> Vec<Value> {
    fs::read_to_string(directory.join("selection.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn routes_sinabis_vector_and_scanned_invoices() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sinabis-ocr");
    let records = rows(&directory);
    assert_eq!(records.len(), 100);
    assert_eq!(records.iter().filter(|record| record["scanned"] == true).count(), 50);

    for record in records {
        let path = directory.join(record["file"].as_str().unwrap());
        let analysis = analyze(&fs::read(&path).unwrap(), &OcrSettings::default())
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(analysis.format, Format::Pdf, "{}", path.display());
        assert!(analysis.page_count > 0, "{}", path.display());
        assert_eq!(analysis.pages.len(), analysis.page_count as usize, "{}", path.display());

        if record["scanned"] == true {
            assert!(!analysis.pages_needing_ocr.is_empty(), "{}", path.display());
            assert_eq!(analysis.ocr.unwrap().status, OcrStatus::Disabled, "{}", path.display());
        } else {
            assert!(analysis.pages_needing_ocr.is_empty(), "{}", path.display());
            assert_eq!(analysis.text_source, TextSource::Native, "{}", path.display());
        }
    }
}

#[test]
fn reads_sinabis_einvoices_and_returns_their_xml_unchanged() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sinabis-einvoices");
    let records = rows(&directory);
    assert_eq!(records.len(), 100);

    for record in records {
        let path = directory.join(record["file"].as_str().unwrap());
        let xml_path = directory.join(record["xml_file"].as_str().unwrap());
        let expected_xml = fs::read_to_string(&xml_path).unwrap();
        let analysis = analyze(&fs::read(&path).unwrap(), &OcrSettings::default())
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(analysis.format, Format::Pdf, "{}", path.display());
        assert_eq!(analysis.text_source, TextSource::Native, "{}", path.display());
        assert!(analysis.page_count > 0, "{}", path.display());
        assert_eq!(analysis.pages.len(), analysis.page_count as usize, "{}", path.display());
        let einvoice = analysis
            .einvoice
            .unwrap_or_else(|| panic!("{}: missing XML", path.display()));
        assert_eq!(einvoice.syntax, EInvoiceSyntax::Cii, "{}", path.display());
        assert_eq!(einvoice.xml, expected_xml, "{}", path.display());
    }
}

#[test]
#[ignore = "requires the pinned PDFium, ONNX Runtime and PP-OCRv6 models"]
fn reads_all_sinabis_scans_with_ocr() {
    let model_directory = std::env::var_os("DOCPIPE_OCR_MODEL_DIR")
        .map(Into::into)
        .expect("DOCPIPE_OCR_MODEL_DIR must point to the offline OCR models");
    let settings = OcrSettings {
        enabled: true,
        model_directory: Some(model_directory),
    };
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sinabis-ocr");
    let records = rows(&directory);

    for record in records.into_iter().filter(|record| record["scanned"] == true) {
        let path = directory.join(record["file"].as_str().unwrap());
        let analysis =
            analyze(&fs::read(&path).unwrap(), &settings).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let report = analysis.ocr.unwrap();
        assert_eq!(
            report.status,
            OcrStatus::Applied,
            "{}: {:?}",
            path.display(),
            report.error
        );
        assert_eq!(
            report.pages.len(),
            analysis.pages_needing_ocr.len(),
            "{}",
            path.display()
        );
        assert!(
            analysis
                .pages
                .iter()
                .filter(|page| page.ocr)
                .all(|page| !page.text.trim().is_empty()),
            "{}",
            path.display()
        );
    }
}

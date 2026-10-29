use std::fs;
use std::path::Path;

use docpipe::analyze::{Format, TextSource, analyze};
use docpipe::ocr::OcrSettings;

#[test]
fn reads_invoice_extraction_benchmark_pdfs() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut pdfs = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pdf"))
        .collect::<Vec<_>>();
    pdfs.sort();

    assert_eq!(pdfs.len(), 113, "the benchmark PDF subset is incomplete");
    for path in pdfs {
        let bytes = fs::read(&path).unwrap();
        let analysis =
            analyze(&bytes, &OcrSettings::default()).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(analysis.format, Format::Pdf, "{}", path.display());
        assert!(analysis.page_count > 0, "{}", path.display());
        assert_eq!(analysis.pages.len(), analysis.page_count as usize, "{}", path.display());
        assert_eq!(analysis.text_source, TextSource::Native, "{}", path.display());
    }
}

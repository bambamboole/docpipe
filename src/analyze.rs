//! One analysis of an uploaded document: a PDF, or a standalone e-invoice XML.
//!
//! An analysis has two phases. The native phase, [`analyze_native`], does
//! everything that needs no OCR: classification, the text layer with
//! positions, the per-page Markdown and the embedded files. Its result, a
//! [`NativeAnalysis`], tells which pages need OCR; the OCR phase,
//! [`crate::ocr::recognize`], reads those, and [`NativeAnalysis::finish`]
//! assembles the [`Analysis`]. The HTTP layer runs the phases separately so
//! that waiting for the single OCR slot does not block a CPU slot and can be
//! given up when the request times out. [`analyze`] runs both phases in a
//! row for callers without a runtime: tests, fuzz targets, tools.

use std::time::Instant;

use lopdf::{Document, LoadOptions};
use pdf_inspector::types::ItemType;
use pdf_inspector::{
    PagesExtractionResult, detect_pdf_mem, extract_pages_markdown_mem, extract_text_with_positions_mem,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::attachments::{self, Attachment, EInvoice};
use crate::candidates::{self, Candidates};
use crate::layout::{self, Item, Line};
use crate::ocr::{self, OcrReport, OcrSettings, OcrStatus};
use crate::profile;

/// How much one compressed stream of a PDF may inflate to while lopdf loads it
/// for the attachments. A decompression bomb inflates a few kilobytes to
/// gigabytes; lopdf decodes object and cross-reference streams eagerly, so
/// without a bound such a file exhausts memory before any of our code runs.
/// 256 MiB leaves room for any real invoice and its embedded files.
const MAX_DECOMPRESSED_STREAM_BYTES: usize = 256 * 1024 * 1024;

/// Why a document could not be analysed.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    /// The bytes are neither a PDF nor a CII/UBL e-invoice XML.
    #[error("the document is neither a PDF nor an XML e-invoice")]
    UnsupportedFormat,
    /// The bytes start like a PDF, but pdf-inspector cannot read them.
    #[error("the PDF cannot be read: {0}")]
    UnreadablePdf(String),
}

/// The kind of document that was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// A PDF, hybrid e-invoice or not.
    Pdf,
    /// A standalone CII or UBL e-invoice.
    Xml,
}

/// pdf-inspector's classification of a PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PdfType {
    /// Every page has a usable text layer.
    TextBased,
    /// Pages are scanned images without text.
    Scanned,
    /// Pages are mostly images.
    ImageBased,
    /// Some pages have text, others need OCR.
    Mixed,
}

impl From<pdf_inspector::PdfType> for PdfType {
    fn from(pdf_type: pdf_inspector::PdfType) -> Self {
        match pdf_type {
            pdf_inspector::PdfType::TextBased => Self::TextBased,
            pdf_inspector::PdfType::Scanned => Self::Scanned,
            pdf_inspector::PdfType::ImageBased => Self::ImageBased,
            pdf_inspector::PdfType::Mixed => Self::Mixed,
        }
    }
}

/// Where the pages' text comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TextSource {
    /// The PDF's own text layer.
    Native,
    /// OCR of every page.
    Ocr,
    /// The text layer for some pages, OCR for others.
    Mixed,
    /// No text: the pages need OCR, which is off or failed.
    None,
}

/// One page of a PDF.
#[derive(Debug, Serialize, ToSchema)]
pub struct Page {
    /// 1-indexed.
    pub number: u32,
    /// The page's lines, top to bottom.
    pub text: String,
    /// The page as Markdown, with tables and headings.
    pub markdown: String,
    /// Whether the page has no usable text layer.
    pub needs_ocr: bool,
    /// Whether `text` and `markdown` come from OCR.
    pub ocr: bool,
}

/// A file embedded in the PDF.
#[derive(Debug, Serialize, ToSchema)]
pub struct AttachmentInfo {
    /// The name the PDF gives the file.
    pub filename: String,
    /// The MIME type the PDF declares, when it does.
    pub mime_type: Option<String>,
    /// Size in bytes.
    pub size: usize,
}

/// The analysis of one document.
#[derive(Debug, Serialize, ToSchema)]
pub struct Analysis {
    /// What was sent.
    pub format: Format,
    /// The PDF's classification; `null` for XML.
    pub pdf_type: Option<PdfType>,
    /// `0` for XML.
    pub page_count: u32,
    /// Where the text of `pages` comes from.
    pub text_source: TextSource,
    /// 1-indexed numbers of the pages without a usable text layer.
    pub pages_needing_ocr: Vec<u32>,
    /// Every page, in order.
    pub pages: Vec<Page>,
    /// The files embedded in the PDF.
    pub attachments: Vec<AttachmentInfo>,
    /// The e-invoice XML embedded in the PDF, or the standalone XML document.
    pub einvoice: Option<EInvoice>,
    /// Field candidates; `null` without text (XML, or OCR off or failed).
    pub candidates: Option<Candidates>,
    /// What OCR did; `null` for XML.
    pub ocr: Option<OcrReport>,
    /// How long the analysis took, waiting for OCR included.
    pub processing_ms: u64,
}

/// Analyses `bytes` completely, OCR included, as `ocr_settings` allow. Both
/// phases run in a row on the calling thread; see the module docs. Nothing
/// serialises OCR here: a caller that analyses documents concurrently with
/// OCR on must do so itself, as the HTTP layer does with its single slot.
pub fn analyze(bytes: &[u8], ocr_settings: &OcrSettings) -> Result<Analysis, AnalyzeError> {
    let native = analyze_native(bytes)?;
    let report = ocr::recognize(bytes, native.pages_needing_ocr(), ocr_settings);

    Ok(native.finish(report))
}

/// The native phase: everything but OCR. See the module docs.
pub fn analyze_native(bytes: &[u8]) -> Result<NativeAnalysis, AnalyzeError> {
    let started = Instant::now();

    let document = if bytes.starts_with(b"%PDF") {
        Native::Pdf(Box::new(native_pdf(bytes)?))
    } else if let Some(xml) = standalone_xml(bytes) {
        Native::Xml(Box::new(analyze_xml(xml)?))
    } else {
        return Err(AnalyzeError::UnsupportedFormat);
    };

    Ok(NativeAnalysis { started, document })
}

/// The result of the native phase, waiting for OCR to [`finish`](Self::finish).
pub struct NativeAnalysis {
    started: Instant,
    document: Native,
}

/// Both variants are boxed: each is hundreds of bytes, and the enum moves
/// between threads.
enum Native {
    Pdf(Box<NativePdf>),
    /// XML has no pages, so its analysis is complete.
    Xml(Box<Analysis>),
}

/// What the native phase found in a PDF.
struct NativePdf {
    pdf_type: PdfType,
    page_count: u32,
    markdown: PagesExtractionResult,
    /// The text layer's lines, every page.
    lines: Vec<Line>,
    /// The text layer's text per page, from `lines`.
    texts: Vec<String>,
    embedded: Vec<Attachment>,
}

impl NativeAnalysis {
    /// 1-indexed numbers of the pages without a usable text layer, which OCR
    /// should read; empty for XML.
    pub fn pages_needing_ocr(&self) -> &[u32] {
        match &self.document {
            Native::Pdf(pdf) => &pdf.markdown.pages_needing_ocr,
            Native::Xml(_) => &[],
        }
    }

    /// Assembles the analysis with what OCR did; `report` is
    /// [`OcrReport::without`] when it did not run.
    pub fn finish(self, report: OcrReport) -> Analysis {
        let mut analysis = match self.document {
            Native::Pdf(pdf) => pdf.finish(report),
            Native::Xml(analysis) => *analysis,
        };
        analysis.processing_ms = self.started.elapsed().as_millis() as u64;

        analysis
    }
}

fn native_pdf(bytes: &[u8]) -> Result<NativePdf, AnalyzeError> {
    let unreadable = |error: pdf_inspector::PdfError| AnalyzeError::UnreadablePdf(error.to_string());

    // pdf-inspector parses the PDF once per call and exposes neither its
    // document nor its detector on the extraction results, so three calls
    // remain: detection for `pdf_type`, which no extraction result carries;
    // the per-page Markdown, which also decides which pages need OCR; and the
    // positioned text for the layout. `page_count` is the same in all of them.
    let detection = detect_pdf_mem(bytes).map_err(unreadable)?;
    let markdown = extract_pages_markdown_mem(bytes, None).map_err(unreadable)?;
    // Image and link placeholders carry their rectangle's height, which would
    // pull whole blocks into one line; form field values are text.
    let items: Vec<Item> = extract_text_with_positions_mem(bytes)
        .map_err(unreadable)?
        .iter()
        .filter(|item| matches!(item.item_type, ItemType::Text | ItemType::FormField))
        .map(Item::from)
        .collect();

    let lines = layout::lines(&items);
    let texts = layout::page_texts(&lines, detection.page_count);

    // The attachments need lopdf's object tree, which pdf-inspector does not
    // hand out, so the PDF is loaded a fourth time, with a bound on what its
    // streams may inflate to. A hybrid e-invoice is still a readable PDF when
    // its attachments are not.
    let embedded = Document::load_mem_with_options(
        bytes,
        LoadOptions::with_max_decompressed_size(MAX_DECOMPRESSED_STREAM_BYTES),
    )
    .map(|document| attachments::embedded_files(&document))
    .unwrap_or_default();

    Ok(NativePdf {
        pdf_type: detection.pdf_type.into(),
        page_count: detection.page_count,
        markdown,
        lines,
        texts,
        embedded,
    })
}

impl NativePdf {
    fn finish(self, report: OcrReport) -> Analysis {
        let markdown = &self.markdown;
        let pages: Vec<Page> = self
            .texts
            .into_iter()
            .enumerate()
            .map(|(index, text)| {
                let number = index as u32 + 1;

                if let Some(recognized) = report.pages.iter().find(|page| page.number == number) {
                    return Page {
                        number,
                        text: recognized.text.clone(),
                        markdown: recognized.markdown.clone(),
                        needs_ocr: true,
                        ocr: true,
                    };
                }

                Page {
                    number,
                    text,
                    // pdf-inspector numbers the Markdown pages from 0.
                    markdown: markdown
                        .pages
                        .iter()
                        .find(|page| page.page == index as u32)
                        .map(|page| page.markdown.clone())
                        .unwrap_or_default(),
                    needs_ocr: markdown.pages_needing_ocr.contains(&number),
                    ocr: false,
                }
            })
            .collect();

        // OCR pages replace whatever unusable text their native extraction found.
        let mut lines: Vec<Line> = self
            .lines
            .into_iter()
            .filter(|line| !report.pages.iter().any(|page| page.number == line.page))
            .chain(report.pages.iter().flat_map(ocr::lines))
            .collect();
        lines.sort_by(|a, b| a.page.cmp(&b.page).then(b.y.total_cmp(&a.y)));

        let einvoice = attachments::find_einvoice(&self.embedded);

        let has_text = lines.iter().any(|line| !line.spans().is_empty());
        let text_source = match (
            has_text,
            report.status == OcrStatus::Applied && !report.pages.is_empty(),
        ) {
            (false, _) => TextSource::None,
            (true, false) => TextSource::Native,
            (true, true) if pages.iter().all(|page| page.ocr) => TextSource::Ocr,
            (true, true) => TextSource::Mixed,
        };

        Analysis {
            format: Format::Pdf,
            pdf_type: Some(self.pdf_type),
            page_count: self.page_count,
            text_source,
            pages_needing_ocr: self.markdown.pages_needing_ocr.clone(),
            pages,
            attachments: self
                .embedded
                .iter()
                .map(|attachment| AttachmentInfo {
                    filename: attachment.filename.clone(),
                    mime_type: attachment.mime_type.clone(),
                    size: attachment.content.len(),
                })
                .collect(),
            einvoice,
            candidates: has_text.then(|| candidates::extract(&lines, profile::detect(&lines))),
            ocr: Some(report),
            processing_ms: 0,
        }
    }
}

fn analyze_xml(xml: &str) -> Result<Analysis, AnalyzeError> {
    let syntax = attachments::einvoice_syntax(xml).ok_or(AnalyzeError::UnsupportedFormat)?;

    Ok(Analysis {
        format: Format::Xml,
        pdf_type: None,
        page_count: 0,
        text_source: TextSource::None,
        pages_needing_ocr: Vec::new(),
        pages: Vec::new(),
        attachments: Vec::new(),
        einvoice: Some(EInvoice {
            syntax,
            filename: None,
            xml: xml.to_owned(),
        }),
        candidates: None,
        ocr: None,
        processing_ms: 0,
    })
}

fn standalone_xml(bytes: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.trim_start_matches('\u{feff}')
        .trim_start()
        .starts_with('<')
        .then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::EInvoiceSyntax;
    use crate::test_support::{CII, pdf_with_attachments};

    #[test]
    fn returns_the_xml_of_a_hybrid_einvoice() {
        let analysis = analyze(&pdf_with_attachments(&[("factur-x.xml", CII)]), &OcrSettings::default()).unwrap();

        assert_eq!(analysis.format, Format::Pdf);
        assert_eq!(analysis.page_count, 1);
        assert_eq!(analysis.attachments.len(), 1);
        assert_eq!(
            analysis.einvoice.map(|einvoice| einvoice.syntax),
            Some(EInvoiceSyntax::Cii)
        );
    }

    #[test]
    fn reports_a_pdf_without_text_as_needing_ocr() {
        let analysis = analyze(&pdf_with_attachments(&[]), &OcrSettings::default()).unwrap();

        assert_eq!(analysis.text_source, TextSource::None);
        assert!(analysis.candidates.is_none());
        assert_eq!(analysis.ocr.map(|report| report.status), Some(OcrStatus::Disabled));
    }

    #[test]
    fn splits_into_a_native_phase_that_names_the_pages_for_ocr() {
        let native = analyze_native(&pdf_with_attachments(&[])).unwrap();
        assert_eq!(native.pages_needing_ocr(), &[1]);

        let analysis = native.finish(OcrReport::without(OcrStatus::Disabled));
        assert_eq!(analysis.format, Format::Pdf);
        assert_eq!(analysis.pages.len(), 1);
        assert!(analysis.pages[0].needs_ocr);
        assert_eq!(analysis.ocr.map(|report| report.status), Some(OcrStatus::Disabled));

        let xml = analyze_native(CII.as_bytes()).unwrap();
        assert!(xml.pages_needing_ocr().is_empty());
        assert!(xml.finish(OcrReport::without(OcrStatus::NotNeeded)).ocr.is_none());
    }

    #[test]
    fn accepts_a_standalone_einvoice() {
        let analysis = analyze(CII.as_bytes(), &OcrSettings::default()).unwrap();

        assert_eq!(analysis.format, Format::Xml);
        assert_eq!(analysis.einvoice.unwrap().filename, None);
    }

    #[test]
    fn rejects_other_formats() {
        assert!(matches!(
            analyze(b"GIF89a", &OcrSettings::default()),
            Err(AnalyzeError::UnsupportedFormat)
        ));
        assert!(matches!(
            analyze(b"<html></html>", &OcrSettings::default()),
            Err(AnalyzeError::UnsupportedFormat)
        ));
    }
}

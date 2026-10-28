//! One analysis of an uploaded document: a PDF, or a standalone e-invoice XML.

use std::time::Instant;

use lopdf::Document;
use pdf_inspector::types::ItemType;
use pdf_inspector::{detect_pdf_mem, extract_pages_markdown_mem, extract_text_with_positions_mem};
use serde::Serialize;
use utoipa::ToSchema;

use crate::attachments::{self, EInvoice};
use crate::candidates::{self, Candidates};
use crate::layout::{self, Item, Line};
use crate::ocr::{self, OcrReport, OcrSettings, OcrStatus};
use crate::profile;

#[derive(Debug, thiserror::Error)]
pub enum AnalyzeError {
    #[error("the document is neither a PDF nor an XML e-invoice")]
    UnsupportedFormat,
    #[error("the PDF cannot be read: {0}")]
    UnreadablePdf(String),
}

/// The kind of document that was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Pdf,
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
    pub filename: String,
    pub mime_type: Option<String>,
    /// Size in bytes.
    pub size: usize,
}

/// The analysis of one document.
#[derive(Debug, Serialize, ToSchema)]
pub struct Analysis {
    pub format: Format,
    /// The PDF's classification; `null` for XML.
    pub pdf_type: Option<PdfType>,
    /// `0` for XML.
    pub page_count: u32,
    pub text_source: TextSource,
    /// 1-indexed numbers of the pages without a usable text layer.
    pub pages_needing_ocr: Vec<u32>,
    pub pages: Vec<Page>,
    pub attachments: Vec<AttachmentInfo>,
    /// The e-invoice XML embedded in the PDF, or the standalone XML document.
    pub einvoice: Option<EInvoice>,
    /// Field candidates; `null` without text (XML, or OCR off or failed).
    pub candidates: Option<Candidates>,
    /// What OCR did; `null` for XML.
    pub ocr: Option<OcrReport>,
    pub processing_ms: u64,
}

pub fn analyze(bytes: &[u8], ocr_settings: &OcrSettings) -> Result<Analysis, AnalyzeError> {
    let started = Instant::now();

    let mut analysis = if bytes.starts_with(b"%PDF") {
        analyze_pdf(bytes, ocr_settings)?
    } else if let Some(xml) = standalone_xml(bytes) {
        analyze_xml(xml)?
    } else {
        return Err(AnalyzeError::UnsupportedFormat);
    };

    analysis.processing_ms = started.elapsed().as_millis() as u64;

    Ok(analysis)
}

fn analyze_pdf(bytes: &[u8], ocr_settings: &OcrSettings) -> Result<Analysis, AnalyzeError> {
    let unreadable = |error: pdf_inspector::PdfError| AnalyzeError::UnreadablePdf(error.to_string());

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

    let native_lines = layout::lines(&items);
    let texts = layout::page_texts(&native_lines, detection.page_count);
    let report = ocr::recognize(bytes, &markdown.pages_needing_ocr, ocr_settings);

    let pages: Vec<Page> = texts
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
                markdown: markdown
                    .pages
                    .iter()
                    .find(|page| page.page == number)
                    .map(|page| page.markdown.clone())
                    .unwrap_or_default(),
                needs_ocr: markdown.pages_needing_ocr.contains(&number),
                ocr: false,
            }
        })
        .collect();

    // OCR pages replace whatever unusable text their native extraction found.
    let mut lines: Vec<Line> = native_lines
        .into_iter()
        .filter(|line| !report.pages.iter().any(|page| page.number == line.page))
        .chain(report.pages.iter().flat_map(ocr::lines))
        .collect();
    lines.sort_by(|a, b| a.page.cmp(&b.page).then(b.y.total_cmp(&a.y)));

    // A hybrid e-invoice is still a readable PDF when its attachments are not.
    let embedded = Document::load_mem(bytes)
        .map(|document| attachments::embedded_files(&document))
        .unwrap_or_default();
    let einvoice = attachments::find_einvoice(&embedded);

    let has_text = lines.iter().any(|line| !line.spans.is_empty());
    let text_source = match (
        has_text,
        report.status == OcrStatus::Applied && !report.pages.is_empty(),
    ) {
        (false, _) => TextSource::None,
        (true, false) => TextSource::Native,
        (true, true) if pages.iter().all(|page| page.ocr) => TextSource::Ocr,
        (true, true) => TextSource::Mixed,
    };

    Ok(Analysis {
        format: Format::Pdf,
        pdf_type: Some(detection.pdf_type.into()),
        page_count: detection.page_count,
        text_source,
        pages_needing_ocr: markdown.pages_needing_ocr.clone(),
        pages,
        attachments: embedded
            .iter()
            .map(|attachment| AttachmentInfo {
                filename: attachment.filename.clone(),
                mime_type: attachment.mime_type.clone(),
                size: attachment.content.len(),
            })
            .collect(),
        einvoice,
        candidates: has_text.then(|| candidates::extract(&lines, profile::fallback())),
        ocr: Some(report),
        processing_ms: 0,
    })
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
    use crate::attachments::tests::{CII, pdf_with_attachments};

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

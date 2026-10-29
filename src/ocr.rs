//! OCR for the pages of a PDF without a usable text layer.
//!
//! pdf-inspector routes only those pages to PP-OCRv6 Small, rendered with
//! `PDFium` and run on ONNX Runtime; text-based PDFs never load either library.
//! See `docs/ocr-benchmark.md` for why this engine and not Tesseract.
//!
//! ONNX Runtime keeps about 1.1 GiB of memory pools per concurrent run, so
//! the service runs one OCR at a time. That is not enforced here: [`recognize`]
//! runs whenever it is called, and the HTTP layer holds its single OCR slot,
//! an async semaphore that a request's timeout can stop waiting for, while it
//! does. [`crate::analyze::analyze`] calls it directly for callers without a
//! runtime, such as the tests and the fuzz targets.

use std::path::PathBuf;

use pdf_inspector::vision::{
    ModelDownloadPolicy, OcrMode, OcrOptions, OcrPdfOptions, PageContentSource, RenderOptions, process_pdf_with_ocr_mem,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::layout::{Line, Span};

/// Rendering resolution of routed pages; the benchmark ran at 200 dpi.
const RENDER_DPI: f32 = 200.0;

/// Horizontal distance between the cells of an OCR table row, in the nominal
/// points of [`lines`]; wider than [`CELL_WIDTH`], so cells never touch.
const CELL_PITCH: f32 = 120.0;

/// Nominal width of a cell span; a label still finds its value to the right.
const CELL_WIDTH: f32 = 100.0;

/// Baseline of the first Markdown line of a page. Lines go down from here,
/// which keeps them in reading order for any page length.
const TOP_BASELINE: f32 = 10_000.0;

/// Vertical distance between consecutive Markdown lines.
const LINE_PITCH: f32 = 12.0;

/// What the client is told when OCR fails. The library's error may name
/// files and goes to the log instead.
const FAILURE_MESSAGE: &str = "OCR failed";

/// Whether and how OCR runs.
#[derive(Debug, Clone, Default)]
pub struct OcrSettings {
    /// Whether pages without a usable text layer are read by OCR at all.
    pub enabled: bool,
    /// A pre-populated PP-OCRv6 Small model directory. With it, models are
    /// never downloaded; without it, they are fetched into the cache once.
    pub model_directory: Option<PathBuf>,
}

/// Whether OCR ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OcrStatus {
    /// No page needed OCR.
    NotNeeded,
    /// Pages needed OCR, but OCR is switched off.
    Disabled,
    /// The pages without a text layer were read by OCR.
    Applied,
    /// The OCR runtime failed; the native result is returned as is.
    Failed,
}

/// A page read by OCR.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OcrPage {
    /// 1-indexed page number.
    pub number: u32,
    /// The page's text, reconstructed from OCR Markdown.
    #[serde(skip)]
    pub text: String,
    /// The page as Markdown, tables included.
    pub markdown: String,
    /// The recognizer's mean confidence. It stays high even on poor scans and
    /// is no quality gate.
    pub confidence: Option<f32>,
    /// Whether pdf-inspector suggests a stronger document parser for the page.
    pub weak: bool,
}

/// What OCR did for the pages without a usable text layer.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OcrReport {
    /// Whether OCR ran.
    pub status: OcrStatus,
    /// The pages OCR read; empty unless `status` is `applied`.
    pub pages: Vec<OcrPage>,
    /// Why OCR failed, in one short sentence; the native result is returned
    /// instead.
    pub error: Option<String>,
    /// Time spent rendering the pages.
    pub render_ms: u64,
    /// Time spent recognizing the rendered pages.
    pub ocr_ms: u64,
}

impl OcrReport {
    /// A report of OCR that did not run, with `status` telling why.
    #[must_use]
    pub fn without(status: OcrStatus) -> Self {
        Self {
            status,
            pages: Vec::new(),
            error: None,
            render_ms: 0,
            ocr_ms: 0,
        }
    }
}

/// Why OCR will not run for a document, or `None` when it will: `needs_ocr`
/// is the native extraction's verdict and `settings` says whether OCR is on.
pub fn skipped(needs_ocr: &[u32], settings: &OcrSettings) -> Option<OcrStatus> {
    if needs_ocr.is_empty() {
        Some(OcrStatus::NotNeeded)
    } else if !settings.enabled {
        Some(OcrStatus::Disabled)
    } else {
        None
    }
}

/// Runs OCR on the pages pdf-inspector selects; `needs_ocr` is the native
/// extraction's verdict and only decides whether to start at all (see
/// [`skipped`]). The caller serialises concurrent runs; see the module docs.
pub fn recognize(bytes: &[u8], needs_ocr: &[u32], settings: &OcrSettings) -> OcrReport {
    if let Some(status) = skipped(needs_ocr, settings) {
        return OcrReport::without(status);
    }

    let mut ocr = OcrOptions::new().mode(OcrMode::Auto);
    if let Some(directory) = &settings.model_directory {
        ocr = ocr
            .model_directory(directory)
            .model_downloads(ModelDownloadPolicy::Offline);
    }

    let options = OcrPdfOptions::auto()
        .ocr(ocr)
        .render(RenderOptions::new().dpi(RENDER_DPI));

    match process_pdf_with_ocr_mem(bytes, options) {
        Ok(result) => OcrReport {
            status: OcrStatus::Applied,
            pages: result
                .pages
                .iter()
                .filter(|page| page.provenance.source != PageContentSource::Native)
                .map(|page| OcrPage {
                    number: page.page_number,
                    text: markdown_lines(&page.markdown).join("\n"),
                    markdown: page.markdown.clone(),
                    confidence: page.provenance.ocr_confidence,
                    weak: result.pages_recommending_hosted.contains(&page.page_number),
                })
                .collect(),
            error: None,
            render_ms: result.render_time_ms,
            ocr_ms: result.ocr_time_ms,
        },
        Err(error) => {
            tracing::warn!(%error, "OCR failed");
            OcrReport {
                error: Some(FAILURE_MESSAGE.to_owned()),
                ..OcrReport::without(OcrStatus::Failed)
            }
        }
    }
}

/// Lines for field extraction from an OCR page's Markdown. OCR has no
/// positions here, so table cells become spans spaced in column order and
/// labels still find values to their right.
pub(crate) fn lines(page: &OcrPage) -> Vec<Line> {
    let mut lines = Vec::new();

    for (index, raw) in page.markdown.lines().enumerate() {
        let trimmed = raw.trim();

        if trimmed.is_empty() || is_table_rule(trimmed) {
            continue;
        }

        let cells: Vec<&str> = if trimmed.starts_with('|') {
            trimmed
                .trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|cell| !cell.is_empty())
                .collect()
        } else {
            vec![strip_markdown(trimmed)]
        };

        let spans: Vec<Span> = cells
            .into_iter()
            .enumerate()
            .map(|(column, cell)| Span {
                text: strip_markdown(cell).to_owned(),
                x: column as f32 * CELL_PITCH,
                width: CELL_WIDTH,
            })
            .collect();

        if !spans.is_empty() {
            lines.push(Line::new(page.number, TOP_BASELINE - index as f32 * LINE_PITCH, spans));
        }
    }

    lines
}

fn markdown_lines(markdown: &str) -> Vec<String> {
    markdown
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !is_table_rule(line))
        .map(|line| {
            if line.starts_with('|') {
                line.trim_matches('|')
                    .split('|')
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                strip_markdown(line).to_owned()
            }
        })
        .collect()
}

fn is_table_rule(line: &str) -> bool {
    line.starts_with('|') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn strip_markdown(text: &str) -> &str {
    text.trim_start_matches('#').trim().trim_matches('*').trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(markdown: &str) -> OcrPage {
        OcrPage {
            number: 2,
            text: String::new(),
            markdown: markdown.to_owned(),
            confidence: Some(0.96),
            weak: false,
        }
    }

    #[test]
    fn turns_table_rows_into_spans_in_column_order() {
        let lines = lines(&page(
            "## Angebot\n\n| Pos | Menge | Gesamt |\n|---|---|---|\n| 1 | 2 Stk | 1.234,56 € |",
        ));

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text(), "Angebot");
        assert_eq!(lines[2].spans().len(), 3);
        assert_eq!(lines[2].spans()[2].text, "1.234,56 €");
        assert!(lines[2].spans()[2].x > lines[2].spans()[1].x);
        assert!(lines.iter().all(|line| line.page == 2));
        assert!(lines[0].y > lines[1].y);
    }

    #[test]
    fn does_not_start_without_pages_to_recognize_or_when_disabled() {
        let off = OcrSettings::default();
        let on = OcrSettings {
            enabled: true,
            model_directory: None,
        };

        assert_eq!(skipped(&[], &off), Some(OcrStatus::NotNeeded));
        assert_eq!(skipped(&[1], &off), Some(OcrStatus::Disabled));
        assert_eq!(skipped(&[1], &on), None);
        assert_eq!(recognize(b"%PDF", &[], &off).status, OcrStatus::NotNeeded);
        assert_eq!(recognize(b"%PDF", &[1], &off).status, OcrStatus::Disabled);
    }
}

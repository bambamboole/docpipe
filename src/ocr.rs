//! OCR for the pages of a PDF without a usable text layer.
//!
//! pdf-inspector routes only those pages to PP-OCRv6 Small, rendered with
//! PDFium and run on ONNX Runtime; text-based PDFs never load either library.
//! See `docs/ocr-benchmark.md` for why this engine and not Tesseract.

use std::path::PathBuf;
use std::sync::Mutex;

use pdf_inspector::vision::{
    ModelDownloadPolicy, OcrMode, OcrOptions, OcrPdfOptions, PageContentSource, RenderOptions, process_pdf_with_ocr_mem,
};
use serde::Serialize;
use utoipa::ToSchema;

use crate::layout::{Line, Span};

/// Serializes OCR runs; native extraction stays parallel.
static OCR_LOCK: Mutex<()> = Mutex::new(());

/// Rendering resolution of routed pages; the benchmark ran at 200 dpi.
const RENDER_DPI: f32 = 200.0;

#[derive(Debug, Clone, Default)]
pub struct OcrSettings {
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
    pub status: OcrStatus,
    pub pages: Vec<OcrPage>,
    /// Why OCR failed; the native result is returned instead.
    pub error: Option<String>,
    pub render_ms: u64,
    pub ocr_ms: u64,
}

impl OcrReport {
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

/// Runs OCR on the pages pdf-inspector selects; `needs_ocr` is the native
/// extraction's verdict and only decides whether to start at all.
pub fn recognize(bytes: &[u8], needs_ocr: &[u32], settings: &OcrSettings) -> OcrReport {
    if needs_ocr.is_empty() {
        return OcrReport::without(OcrStatus::NotNeeded);
    }

    if !settings.enabled {
        return OcrReport::without(OcrStatus::Disabled);
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

    // ONNX Runtime keeps about 1.1 GiB of memory pools per concurrent run;
    // one OCR at a time keeps the container within its memory limit.
    let _guard = OCR_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

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
                error: Some(error.to_string()),
                ..OcrReport::without(OcrStatus::Failed)
            }
        }
    }
}

/// Lines for field extraction from an OCR page's Markdown. OCR has no
/// positions here, so table cells become spans spaced in column order and
/// labels still find values to their right.
pub fn lines(page: &OcrPage) -> Vec<Line> {
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
                x: column as f32 * 120.0,
                width: 100.0,
            })
            .collect();

        if !spans.is_empty() {
            lines.push(Line {
                page: page.number,
                y: 10_000.0 - index as f32 * 12.0,
                size: 10.0,
                spans,
            });
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
        assert_eq!(lines[2].spans.len(), 3);
        assert_eq!(lines[2].spans[2].text, "1.234,56 €");
        assert!(lines[2].spans[2].x > lines[2].spans[1].x);
        assert!(lines.iter().all(|line| line.page == 2));
        assert!(lines[0].y > lines[1].y);
    }

    #[test]
    fn does_not_start_without_pages_to_recognize_or_when_disabled() {
        assert_eq!(
            recognize(b"%PDF", &[], &OcrSettings::default()).status,
            OcrStatus::NotNeeded
        );
        assert_eq!(
            recognize(b"%PDF", &[1], &OcrSettings::default()).status,
            OcrStatus::Disabled
        );
    }
}

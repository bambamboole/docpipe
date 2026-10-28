//! Embedded files of a PDF, and the e-invoice XML among them.
//!
//! ZUGFeRD/Factur-X and XRechnung hybrids carry their structured invoice as
//! an attached XML file. docpipe only finds and returns it; the receiving
//! application parses it with a dedicated e-invoice library.

use lopdf::{Dictionary, Document, Object};
use serde::Serialize;
use utoipa::ToSchema;

/// Embedded files larger than this are skipped rather than decompressed.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: Option<String>,
    pub content: Vec<u8>,
}

/// The syntax of an e-invoice XML document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EInvoiceSyntax {
    /// UN/CEFACT Cross Industry Invoice: ZUGFeRD, Factur-X, XRechnung CII.
    Cii,
    /// OASIS UBL 2.1: XRechnung UBL, Peppol.
    Ubl,
}

/// An e-invoice XML document, returned as is.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct EInvoice {
    pub syntax: EInvoiceSyntax,
    /// The attachment's file name, or `null` for a standalone XML document.
    pub filename: Option<String>,
    pub xml: String,
}

/// The files embedded in a PDF, through the `EmbeddedFiles` name tree and the
/// catalog's associated files (`AF`). Unreadable entries are skipped.
pub fn embedded_files(document: &Document) -> Vec<Attachment> {
    let Ok(catalog) = document.catalog() else {
        return Vec::new();
    };

    let mut specs: Vec<&Dictionary> = Vec::new();

    if let Ok(tree) = catalog
        .get_deref(b"Names", document)
        .and_then(Object::as_dict)
        .and_then(|names| names.get_deref(b"EmbeddedFiles", document))
        .and_then(Object::as_dict)
    {
        collect_name_tree(document, tree, &mut specs, 0);
    }

    if let Ok(associated) = catalog.get_deref(b"AF", document).and_then(Object::as_array) {
        specs.extend(
            associated
                .iter()
                .filter_map(|spec| document.dereference(spec).ok()?.1.as_dict().ok()),
        );
    }

    let mut attachments: Vec<Attachment> = Vec::new();

    for spec in specs {
        if let Some(attachment) = read_file_spec(document, spec)
            && !attachments.iter().any(|known| known.filename == attachment.filename)
        {
            attachments.push(attachment);
        }
    }

    attachments
}

/// The e-invoice among the attachments: a CII or UBL document, preferring the
/// file names the ZUGFeRD, Factur-X and XRechnung specifications prescribe.
pub fn find_einvoice(attachments: &[Attachment]) -> Option<EInvoice> {
    let mut candidates: Vec<(&Attachment, EInvoiceSyntax, String)> = attachments
        .iter()
        .filter_map(|attachment| {
            let xml = String::from_utf8(attachment.content.clone()).ok()?;
            Some((attachment, einvoice_syntax(&xml)?, xml))
        })
        .collect();

    candidates.sort_by_key(|(attachment, _, _)| !is_prescribed_name(&attachment.filename));

    candidates.into_iter().next().map(|(attachment, syntax, xml)| EInvoice {
        syntax,
        filename: Some(attachment.filename.clone()),
        xml,
    })
}

/// The syntax of an XML e-invoice, from its root element.
pub fn einvoice_syntax(xml: &str) -> Option<EInvoiceSyntax> {
    let root = xml.trim_start_matches('\u{feff}').split('<').find(|tag| {
        let tag = tag.trim_start();
        !tag.is_empty() && !tag.starts_with('?') && !tag.starts_with('!')
    })?;
    let name = root.split(|c: char| c.is_whitespace() || c == '>' || c == '/').next()?;
    let local = name.rsplit(':').next()?;

    match local {
        "CrossIndustryInvoice" => Some(EInvoiceSyntax::Cii),
        "Invoice" | "CreditNote" if root.contains("oasis") => Some(EInvoiceSyntax::Ubl),
        _ => None,
    }
}

fn is_prescribed_name(filename: &str) -> bool {
    matches!(
        filename.to_ascii_lowercase().as_str(),
        "factur-x.xml" | "zugferd-invoice.xml" | "xrechnung.xml" | "zugferd_invoice.xml"
    )
}

fn collect_name_tree<'a>(document: &'a Document, node: &'a Dictionary, specs: &mut Vec<&'a Dictionary>, depth: u8) {
    // Name trees are shallow; the bound protects against reference cycles.
    if depth > 16 {
        return;
    }

    if let Ok(names) = node.get_deref(b"Names", document).and_then(Object::as_array) {
        // Alternating key/value pairs: every second entry is a file spec.
        for spec in names.iter().skip(1).step_by(2) {
            if let Ok((_, Object::Dictionary(dictionary))) = document.dereference(spec) {
                specs.push(dictionary);
            }
        }
    }

    if let Ok(kids) = node.get_deref(b"Kids", document).and_then(Object::as_array) {
        for kid in kids {
            if let Ok((_, Object::Dictionary(dictionary))) = document.dereference(kid) {
                collect_name_tree(document, dictionary, specs, depth + 1);
            }
        }
    }
}

fn read_file_spec(document: &Document, spec: &Dictionary) -> Option<Attachment> {
    let filename = [b"UF".as_slice(), b"F"]
        .iter()
        .find_map(|key| spec.get_deref(key, document).ok().and_then(decode_text))?;

    let embedded = spec.get_deref(b"EF", document).ok()?.as_dict().ok()?;
    let stream = [b"UF".as_slice(), b"F"]
        .iter()
        .find_map(|key| embedded.get_deref(key, document).ok()?.as_stream().ok())?;

    let content = stream
        .decompressed_content_with_limit(MAX_ATTACHMENT_BYTES)
        .ok()
        .or_else(|| (stream.content.len() <= MAX_ATTACHMENT_BYTES).then(|| stream.content.clone()))?;

    let mime_type = stream
        .dict
        .get(b"Subtype")
        .ok()
        .and_then(|subtype| subtype.as_name().ok())
        .map(|name| String::from_utf8_lossy(name).replace("#2F", "/"));

    Some(Attachment {
        filename,
        mime_type,
        content,
    })
}

/// A PDF text string: UTF-16BE with a byte order mark, else PDFDocEncoding,
/// read here as Latin-1.
fn decode_text(object: &Object) -> Option<String> {
    let bytes = object.as_str().ok()?;

    if let Some(utf16) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units: Vec<u16> = utf16
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16(&units).ok();
    }

    Some(bytes.iter().map(|byte| *byte as char).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use lopdf::{Stream, dictionary};

    use super::*;

    pub(crate) const CII: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100"></rsm:CrossIndustryInvoice>"#;

    const UBL: &str =
        r#"<?xml version="1.0"?><Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"></Invoice>"#;

    /// A one-page PDF with the given files attached through the name tree.
    pub(crate) fn pdf_with_attachments(files: &[(&str, &str)]) -> Vec<u8> {
        let mut document = Document::with_version("1.7");
        let pages_id = document.new_object_id();
        let page_id = document.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });
        document.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! { "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1 }),
        );

        let mut names: Vec<Object> = Vec::new();
        for (name, content) in files {
            let stream_id = document.add_object(Stream::new(
                dictionary! { "Type" => "EmbeddedFile", "Subtype" => "text#2Fxml" },
                content.as_bytes().to_vec(),
            ));
            let spec_id = document.add_object(dictionary! {
                "Type" => "Filespec",
                "F" => Object::string_literal(*name),
                "UF" => Object::string_literal(*name),
                "EF" => dictionary! { "F" => stream_id },
            });
            names.push(Object::string_literal(*name));
            names.push(spec_id.into());
        }

        let catalog_id = document.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "Names" => dictionary! { "EmbeddedFiles" => dictionary! { "Names" => names } },
        });
        document.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        document.save_to(&mut bytes).expect("the test PDF is written");
        bytes
    }

    #[test]
    fn finds_the_factur_x_attachment() {
        let bytes = pdf_with_attachments(&[("notes.txt", "hello"), ("factur-x.xml", CII)]);
        let document = Document::load_mem(&bytes).unwrap();

        let attachments = embedded_files(&document);
        let einvoice = find_einvoice(&attachments).unwrap();

        assert_eq!(attachments.len(), 2);
        assert_eq!(einvoice.syntax, EInvoiceSyntax::Cii);
        assert_eq!(einvoice.filename.as_deref(), Some("factur-x.xml"));
        assert_eq!(einvoice.xml, CII);
    }

    #[test]
    fn prefers_the_prescribed_file_name() {
        let bytes = pdf_with_attachments(&[("copy.xml", UBL), ("xrechnung.xml", CII)]);
        let attachments = embedded_files(&Document::load_mem(&bytes).unwrap());

        assert_eq!(
            find_einvoice(&attachments).unwrap().filename.as_deref(),
            Some("xrechnung.xml")
        );
    }

    #[test]
    fn has_no_einvoice_without_an_invoice_xml() {
        let bytes = pdf_with_attachments(&[("other.xml", "<note>hi</note>")]);
        let attachments = embedded_files(&Document::load_mem(&bytes).unwrap());

        assert_eq!(find_einvoice(&attachments), None);
    }

    #[test]
    fn detects_the_syntax_from_the_root_element() {
        assert_eq!(einvoice_syntax(CII), Some(EInvoiceSyntax::Cii));
        assert_eq!(einvoice_syntax(UBL), Some(EInvoiceSyntax::Ubl));
        assert_eq!(einvoice_syntax("<html></html>"), None);
    }
}

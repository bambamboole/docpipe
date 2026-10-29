//! Helpers the unit tests of several modules share: positioned items and
//! lines built in code, the bundled profiles and a PDF with attachments.
//! Tests never read real documents; see `CLAUDE.md`.

use lopdf::{Document, Object, Stream, dictionary};

use crate::layout::{Item, Line, lines};
use crate::profile::{self, Profile};

/// A minimal CII e-invoice: the root element is all the service looks at.
pub(crate) const CII: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rsm:CrossIndustryInvoice xmlns:rsm="urn:un:unece:uncefact:data:standard:CrossIndustryInvoice:100"></rsm:CrossIndustryInvoice>"#;

/// Vertical distance between the lines [`lines_of`] builds.
const LINE_PITCH: f32 = 20.0;

/// A positioned text item; `width` is estimated from the length.
pub(crate) fn item(page: u32, x: f32, y: f32, text: &str) -> Item {
    Item {
        page,
        text: text.to_owned(),
        x,
        y,
        width: text.chars().count() as f32 * 5.0,
        height: 10.0,
        rotation: 0.0,
    }
}

/// One line per text, top to bottom on the first page, each in one run at
/// the left margin.
pub(crate) fn lines_of(texts: &[&str]) -> Vec<Line> {
    let items: Vec<Item> = texts
        .iter()
        .enumerate()
        .map(|(index, text)| item(1, 50.0, 700.0 - index as f32 * LINE_PITCH, text))
        .collect();

    lines(&items)
}

/// The bundled German profile.
pub(crate) fn german() -> &'static Profile {
    bundled("de")
}

/// The bundled English profile.
pub(crate) fn english() -> &'static Profile {
    bundled("en")
}

fn bundled(id: &str) -> &'static Profile {
    profile::all()
        .iter()
        .find(|profile| profile.id == id)
        .unwrap_or_else(|| panic!("the {id} profile is bundled"))
}

/// A one-page PDF with the given files attached through the name tree.
pub(crate) fn pdf_with_attachments(files: &[(&str, &str)]) -> Vec<u8> {
    let mut document = Document::with_version("1.7");
    let page_tree_id = document.new_object_id();
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => page_tree_id,
        "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
    });
    document.objects.insert(
        page_tree_id,
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
        "Pages" => page_tree_id,
        "Names" => dictionary! { "EmbeddedFiles" => dictionary! { "Names" => names } },
    });
    document.trailer.set("Root", catalog_id);

    let mut bytes = Vec::new();
    document.save_to(&mut bytes).expect("the test PDF is written");
    bytes
}

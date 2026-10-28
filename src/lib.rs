//! docpipe analyses incoming documents: it classifies PDFs, extracts their
//! text with positions, finds embedded e-invoice XML and proposes the fields
//! of German invoices and quotes.

pub mod analyze;
pub mod attachments;
pub mod auth;
pub mod candidates;
pub mod http;
pub mod layout;
pub mod line_items;
pub mod ocr;
pub mod seller;
pub mod telemetry;

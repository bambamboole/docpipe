//! docpipe analyses incoming documents: it classifies PDFs, extracts their
//! text with positions, finds embedded e-invoice XML and proposes the fields
//! of invoices and quotes with the words of a language profile.

pub mod analyze;
pub mod attachments;
pub mod auth;
pub mod candidates;
pub mod currency;
pub mod dates;
pub mod http;
pub mod layout;
pub mod line_items;
pub mod numbers;
pub mod ocr;
pub mod profile;
pub mod seller;
pub mod tax;
pub mod telemetry;

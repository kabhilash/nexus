//! JSON renderer. DD-008 §5.3.
//!
//! Rules:
//! - One JSON value per invocation (object or array), pretty-printed.
//! - `snake_case` field names — that's how `serde::Serialize` on the
//!   view types already spells them.
//! - Errors in JSON mode go to stderr as a `{"error": …}` object
//!   via [`write_error_object`].

use std::io::{self, Write};

use serde::Serialize;

use crate::errors::{NexusctlError, json_error_object};

/// Render any serializable value as pretty-printed JSON plus a
/// trailing newline. Used by every view's `render_json`.
pub fn write<T: Serialize>(value: &T, w: &mut dyn Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *w, value).map_err(io::Error::other)?;
    writeln!(w)
}

/// Serialise a [`NexusctlError`] into the DD-008 §9 JSON envelope
/// and emit it as a single line to `w` (typically `stderr`).
pub fn write_error_object(err: &NexusctlError, w: &mut dyn Write) -> io::Result<()> {
    let obj = json_error_object(err);
    serde_json::to_writer(&mut *w, &obj).map_err(io::Error::other)?;
    writeln!(w)
}

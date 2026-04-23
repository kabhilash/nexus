//! JSON output. DD-008 §5.3.
//!
//! Every command emits exactly one top-level JSON value
//! (object or array) followed by a single newline. Field names are
//! `snake_case`; missing values are `null`. The renderer is a
//! single-line wrapper around `serde_json::to_writer` — the
//! per-type schemas live on the `proxy::*` structs.

use std::io::{self, Write};

use serde::Serialize;

/// Pretty-print any serializable value as a single JSON value.
/// Always uses pretty-printing (2-space indent) — Phase 1's CLI is
/// human-first; a future `--compact-json` flag can flip this if a
/// real consumer ever cares about bytes on the wire.
pub fn write<T: Serialize>(value: &T, w: &mut dyn Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *w, value).map_err(io::Error::other)?;
    writeln!(w)
}

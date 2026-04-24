//! Output-format layer. DD-008 §5.
//!
//! # Shape
//!
//! Every command produces a *view type* (`ManagerStatus`,
//! `Vec<InterfaceSummary>`, …) that implements [`Render`]. The
//! dispatcher in [`render`] picks the right per-format method based
//! on the resolved [`OutputFormat`]. This keeps per-format code
//! colocated with the data shape it renders and keeps commands
//! completely format-agnostic — a new subcommand just assembles
//! its view and calls [`render`].
//!
//! # Format summary
//!
//! - **Human** ([`human`]): comfy-table for lists, vertical
//!   key/value for single records. Includes the DD-008 §5.1
//!   state-prefix column.
//! - **Terse** ([`terse`]): one record per line, colon-separated,
//!   `--fields` selects columns.
//! - **JSON** ([`json`]): serde_json pretty-printed. One
//!   top-level value per invocation.
//! - **Pretty** ([`pretty`]): verbose multi-line per record;
//!   blank-line separated when the view is a list.

use std::io::{self, Write};

use clap::ValueEnum;

pub mod human;
pub mod json;
pub mod pretty;
pub mod records;
pub mod terse;

/// Which renderer to dispatch to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    Human,
    Terse,
    Json,
    Pretty,
}

impl Default for OutputFormat {
    fn default() -> Self {
        OutputFormat::Human
    }
}

/// How much colour to emit. Human/pretty renderers consult this
/// to decide whether to add ANSI escapes; other formats ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[value(rename_all = "lower")]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

/// Context threaded through every render call. Holds the terse
/// configuration (fields + separator) and the colour preference.
/// Additive: new options land here without churning every
/// [`Render`] implementor's signature.
#[derive(Debug, Clone)]
pub struct RenderContext {
    /// `--fields` selection for terse mode. `None` means "every
    /// field the view defines, in its declared order".
    pub fields: Option<Vec<String>>,
    pub separator: String,
    pub color: ColorChoice,
}

impl Default for RenderContext {
    fn default() -> Self {
        Self {
            fields: None,
            separator: ":".to_owned(),
            color: ColorChoice::default(),
        }
    }
}

/// A view type that knows how to render itself in every supported
/// format. Each method is fallible so renderers that hit broken
/// pipes / invalid `--fields` selections can bubble the cause up.
pub trait Render {
    fn render_human(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()>;
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()>;
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()>;
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()>;
}

/// Dispatch to the renderer the current [`OutputFormat`] names.
pub fn render<V: Render + ?Sized>(
    view: &V,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> io::Result<()> {
    match format {
        OutputFormat::Human => view.render_human(ctx, w),
        OutputFormat::Terse => view.render_terse(ctx, w),
        OutputFormat::Json => view.render_json(w),
        OutputFormat::Pretty => view.render_pretty(ctx, w),
    }
}

/// Shared helper: escape occurrences of `sep` inside `value` with
/// a backslash (DD-008 §5.2 "Embedded separator characters in
/// field values are escaped with a backslash"). Backslashes are
/// themselves escaped so the output round-trips.
pub(crate) fn escape_terse(value: &str, sep: &str) -> String {
    if sep.is_empty() {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut remaining = value;
    while !remaining.is_empty() {
        if remaining.starts_with('\\') {
            out.push_str("\\\\");
            remaining = &remaining[1..];
            continue;
        }
        if remaining.starts_with(sep) {
            out.push('\\');
            out.push_str(sep);
            remaining = &remaining[sep.len()..];
            continue;
        }
        let mut chars = remaining.chars();
        let c = chars.next().unwrap();
        out.push(c);
        remaining = chars.as_str();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_leaves_clean_values_alone() {
        assert_eq!(escape_terse("eth0", ":"), "eth0");
    }

    #[test]
    fn escape_backslash_separator() {
        assert_eq!(escape_terse("corp:wifi", ":"), "corp\\:wifi");
    }

    #[test]
    fn escape_doubles_existing_backslashes() {
        assert_eq!(escape_terse("a\\b", ":"), "a\\\\b");
    }

    #[test]
    fn escape_tab_separator() {
        assert_eq!(escape_terse("a\tb", "\t"), "a\\\tb");
    }
}

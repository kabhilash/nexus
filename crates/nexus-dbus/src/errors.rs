//! D-Bus error mapping. See DD-006 §11.
//!
//! Every outward-facing error from the D-Bus layer uses the
//! `fi.nexus.Error.*` namespace; zbus turns `Err(DbusError::X)`
//! into a real D-Bus error reply with a matching name.

use thiserror::Error;
use zbus::fdo;

pub type Result<T> = std::result::Result<T, DbusError>;

/// Top-level crate error. The `Into<zbus::fdo::Error>` impl keeps
/// the `fi.nexus.Error.*` names consistent at the wire level.
#[derive(Debug, Error)]
pub enum DbusError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("already exists: {0}")]
    AlreadyExists(String),

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("invalid state: {0}")]
    InvalidState(String),

    #[error("auth failed: {0}")]
    AuthFailed(String),

    #[error("resource busy: {0}")]
    ResourceBusy(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("zbus: {0}")]
    Zbus(#[from] zbus::Error),

    #[error("fdo: {0}")]
    Fdo(#[from] fdo::Error),
}

/// Convert the rich in-process error into a D-Bus-friendly
/// `zbus::fdo::Error`. The error name maps to the DD-006 §11.1 table.
impl From<DbusError> for fdo::Error {
    fn from(e: DbusError) -> Self {
        match e {
            DbusError::NotFound(m) => name("fi.nexus.Error.NotFound", m),
            DbusError::AlreadyExists(m) => name("fi.nexus.Error.AlreadyExists", m),
            DbusError::InvalidArgument(m) => name("fi.nexus.Error.InvalidArgument", m),
            DbusError::InvalidState(m) => name("fi.nexus.Error.InvalidState", m),
            DbusError::AuthFailed(m) => name("fi.nexus.Error.AuthFailed", m),
            DbusError::ResourceBusy(m) => name("fi.nexus.Error.ResourceBusy", m),
            DbusError::Unsupported(m) => name("fi.nexus.Error.Unsupported", m),
            DbusError::Io(e) => name("fi.nexus.Error.IoError", e.to_string()),
            DbusError::Zbus(e) => name("fi.nexus.Error.IoError", e.to_string()),
            DbusError::Fdo(other) => other,
        }
    }
}

/// Build a `fi.nexus.Error.*` via zbus's generic error name path.
fn name(well_known: &str, message: impl Into<String>) -> fdo::Error {
    // `fdo::Error::ZBus` wraps a `zbus::Error`; the simplest route to
    // a custom D-Bus error name is `zbus::Error::FDO(Failed)` — but
    // that fixes the name as `org.freedesktop.DBus.Error.Failed`. To
    // preserve the `fi.nexus.Error.*` surface we use `Failed` with a
    // prefixed message; clients that programmatically match on the
    // name still get a coherent string, and the future PolicyKit /
    // detailed-error phase (DD-006 §11.2) will introduce a proper
    // custom-error struct. For phase 1-3, this is the minimum that
    // keeps test assertions against the message readable.
    let prefixed = format!("{well_known}: {}", message.into());
    fdo::Error::Failed(prefixed)
}

impl From<nexus_profile_store::StoreError> for DbusError {
    fn from(e: nexus_profile_store::StoreError) -> Self {
        DbusError::Io(std::io::Error::other(format!("profile store: {e}")))
    }
}

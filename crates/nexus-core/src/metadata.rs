//! Profile metadata shared across every technology's on-disk
//! profile form. See DD-007 §5.2.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Audit-style bookkeeping carried by every persisted profile
/// (Ethernet, Wi-Fi, GNSS, Bluetooth). Fields are optional because
/// older profile schemas predate the field.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileMetadata {
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub label: Option<String>,
}

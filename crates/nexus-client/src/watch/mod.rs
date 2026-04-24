//! `nexusctl watch` machinery. DD-008 §7.4.
//!
//! Four layers live here:
//!
//! 1. [`event`] — the flat `WatchEvent` type + its
//!    `FieldValue` scalar enum. Serialized to JSON with `time` and
//!    `kind` leading, the rest of the flat dict in deterministic
//!    (alphabetical) order.
//! 2. [`synthesize`] — one helper per row of the DD-008 §7.4
//!    table that produces a `WatchEvent` from a signal's typed
//!    args. Production zbus subscription calls these; tests call
//!    them directly.
//! 3. [`filter`] — `--filter 'field=glob'` matching with AND
//!    semantics across repeated flags (globset under the hood).
//! 4. [`subscribe`] — `WatchSubset` classifier (events / iface /
//!    wifi / bt / gnss) and the `WatchStream` async trait the
//!    command handler pulls events from. `MockWatchStream` sits
//!    here too, gated behind `interactive-flows-testing`.

pub mod event;
pub mod filter;
pub mod subscribe;
pub mod synthesize;

pub use event::{FieldValue, WatchEvent};
pub use filter::{Filter, passes};
pub use subscribe::{WatchStream, WatchSubset};

#[cfg(any(test, feature = "interactive-flows-testing"))]
pub use subscribe::MockWatchStream;

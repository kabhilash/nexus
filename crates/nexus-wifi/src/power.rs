//! Power-state definitions. See DD-003 §13.

/// Coarse power state. Fine-grained e.g. suspend/hibernate handling
/// is out of scope for v0.1; the three levels here are what
/// `fi.nexus.Manager.SetPowerState` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerState {
    /// Full operation. Normal scan intervals; 5 s signal polling.
    #[default]
    Active,
    /// User present but device idle. Intervals doubled; 15 s signal
    /// polling.
    Background,
    /// Device suspended / deep-idle. Scheduled scans paused; signal
    /// polling paused.
    Sleep,
}

impl PowerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerState::Active => "active",
            PowerState::Background => "background",
            PowerState::Sleep => "sleep",
        }
    }
}

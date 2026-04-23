//! Prometheus-style metrics. The authoritative list is DD-007
//! §10.4 — don't add new metrics here without updating the DD.
//!
//! This module only talks to the `metrics` crate facade; the real
//! exporter is installed elsewhere (nexus-daemon at startup).

use ::metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};

use crate::trait_def::ProfileKind;

// Metric name constants — exposed so tests can assert on identifiers.
pub const PROFILES_LOADED_TOTAL: &str = "nexus_profiles_loaded_total";
pub const PROFILES_STORED: &str = "nexus_profiles_stored";
pub const PROFILES_WRITES_TOTAL: &str = "nexus_profiles_writes_total";
pub const PROFILES_WRITE_DURATION_SECONDS: &str = "nexus_profiles_write_duration_seconds";
pub const PROFILES_CORRUPT_TOTAL: &str = "nexus_profiles_corrupt_total";
pub const PROFILES_MASTER_KEY_SOURCE: &str = "nexus_profiles_master_key_source";
pub const PROFILES_ROTATION_TOTAL: &str = "nexus_profiles_rotation_total";
pub const PROFILES_ROTATION_DURATION_SECONDS: &str = "nexus_profiles_rotation_duration_seconds";
pub const PROFILES_MIGRATION_TOTAL: &str = "nexus_profiles_migration_total";

/// Outcome label values used on the write / rotation / migration counters.
pub mod outcome {
    pub const SUCCESS: &str = "success";
    pub const LOCK_CONTENTION: &str = "lock_contention";
    pub const IO_ERROR: &str = "io_error";
    pub const CRYPTO_ERROR: &str = "crypto_error";
    pub const CORRUPT: &str = "corrupt";
}

/// Corrupt-reason labels used on [`PROFILES_CORRUPT_TOTAL`].
pub mod corrupt_reason {
    pub const TOML_PARSE: &str = "toml_parse";
    pub const DECRYPT_FAIL: &str = "decrypt_fail";
    pub const SCHEMA_VIOLATION: &str = "schema_violation";
}

/// Master-key-source labels used on [`PROFILES_MASTER_KEY_SOURCE`].
pub const KEY_SOURCES: &[&str] = &["file", "tpm", "keyring", "derived", "memory"];

/// Describe every metric with its unit and help text. Idempotent.
pub fn register() {
    describe_counter!(
        PROFILES_LOADED_TOTAL,
        "Profiles successfully loaded at startup and on reload, by kind"
    );
    describe_gauge!(PROFILES_STORED, "Current profile count, by kind");
    describe_counter!(
        PROFILES_WRITES_TOTAL,
        "Profile write operations, by kind and outcome"
    );
    describe_histogram!(
        PROFILES_WRITE_DURATION_SECONDS,
        Unit::Seconds,
        "End-to-end profile write duration including fsync"
    );
    describe_counter!(
        PROFILES_CORRUPT_TOTAL,
        "Profiles quarantined, by kind and reason"
    );
    describe_gauge!(
        PROFILES_MASTER_KEY_SOURCE,
        "Active master-key source (1 for the active source, 0 for others)"
    );
    describe_counter!(
        PROFILES_ROTATION_TOTAL,
        "Master-key rotation attempts, by outcome"
    );
    describe_histogram!(
        PROFILES_ROTATION_DURATION_SECONDS,
        Unit::Seconds,
        "Master-key rotation duration"
    );
    describe_counter!(
        PROFILES_MIGRATION_TOTAL,
        "Schema migrations, by from/to version and outcome"
    );
}

/// String label for a [`ProfileKind`].
pub fn kind_label(kind: ProfileKind) -> &'static str {
    match kind {
        ProfileKind::Ethernet => "ethernet",
        ProfileKind::Wifi => "wifi",
        ProfileKind::Gnss => "gnss",
        ProfileKind::Bluetooth => "bluetooth",
    }
}

pub fn record_profile_loaded(kind: ProfileKind) {
    counter!(PROFILES_LOADED_TOTAL, "kind" => kind_label(kind)).increment(1);
}

pub fn set_profile_count(kind: ProfileKind, count: u64) {
    gauge!(PROFILES_STORED, "kind" => kind_label(kind)).set(count as f64);
}

pub fn record_write(kind: ProfileKind, outcome: &str) {
    counter!(
        PROFILES_WRITES_TOTAL,
        "kind" => kind_label(kind),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_write_duration(kind: ProfileKind, secs: f64) {
    histogram!(PROFILES_WRITE_DURATION_SECONDS, "kind" => kind_label(kind)).record(secs);
}

pub fn record_corrupt(kind: ProfileKind, reason: &str) {
    counter!(
        PROFILES_CORRUPT_TOTAL,
        "kind" => kind_label(kind),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Set the master-key-source gauge: 1 for the active source, 0 for
/// every other known source. Call once per store open.
pub fn set_master_key_source(active: &str) {
    for source in KEY_SOURCES {
        let value = if *source == active { 1.0 } else { 0.0 };
        gauge!(PROFILES_MASTER_KEY_SOURCE, "source" => *source).set(value);
    }
}

pub fn record_rotation(outcome: &str) {
    counter!(PROFILES_ROTATION_TOTAL, "outcome" => outcome.to_owned()).increment(1);
}

pub fn record_rotation_duration(secs: f64) {
    histogram!(PROFILES_ROTATION_DURATION_SECONDS).record(secs);
}

pub fn record_migration(from_version: u32, to_version: u32, outcome: &str) {
    counter!(
        PROFILES_MIGRATION_TOTAL,
        "from_version" => from_version.to_string(),
        "to_version" => to_version.to_string(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_label_covers_every_variant() {
        assert_eq!(kind_label(ProfileKind::Ethernet), "ethernet");
        assert_eq!(kind_label(ProfileKind::Wifi), "wifi");
        assert_eq!(kind_label(ProfileKind::Gnss), "gnss");
        assert_eq!(kind_label(ProfileKind::Bluetooth), "bluetooth");
    }

    #[test]
    fn register_is_idempotent_and_helpers_do_not_panic() {
        register();
        register();
        record_profile_loaded(ProfileKind::Wifi);
        set_profile_count(ProfileKind::Wifi, 3);
        record_write(ProfileKind::Wifi, outcome::SUCCESS);
        record_write_duration(ProfileKind::Wifi, 0.01);
        record_corrupt(ProfileKind::Wifi, corrupt_reason::DECRYPT_FAIL);
        set_master_key_source("file");
        record_rotation(outcome::SUCCESS);
        record_rotation_duration(0.5);
        record_migration(1, 2, outcome::SUCCESS);
    }
}

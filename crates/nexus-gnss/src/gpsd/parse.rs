//! gpsd JSON → structured [`GnssFix`] / [`SatInfo`] translation.
//! See DD-005 §6.3.

use chrono::{DateTime, Utc};
use tracing::debug;

use super::messages::{SkyMessage, TpvMessage};
use crate::fix::{FixMode, GnssFix, SatInfo};

/// Translate a [`TpvMessage`] into a `(device_path, GnssFix)` pair.
/// Returns `None` when the `device` field is missing — gpsd always
/// stamps every TPV with a device path, so a missing one indicates
/// a malformed stream.
pub fn parse_tpv(msg: &TpvMessage) -> Option<(String, GnssFix)> {
    let device = msg.device.clone()?;
    let fix = GnssFix {
        time: parse_gpsd_time(msg.time.as_deref(), &device),
        mode: classify_mode(msg.mode, msg.alt_hae, msg.alt_msl),
        latitude: msg.lat,
        longitude: msg.lon,
        altitude_m: msg.alt_hae.or(msg.alt_msl),
        speed_mps: msg.speed,
        track_deg: msg.track,
        horizontal_error_m: msg.eph.or_else(|| match (msg.epx, msg.epy) {
            (Some(x), Some(y)) => Some((x * x + y * y).sqrt()),
            _ => None,
        }),
        vertical_error_m: msg.epv,
        satellites_used: msg.used.unwrap_or(0),
    };
    Some((device, fix))
}

/// Translate a [`SkyMessage`] into a `(device_path, Vec<SatInfo>)`.
/// Returns `None` when the `device` field is missing.
pub fn parse_sky(msg: &SkyMessage) -> Option<(String, Vec<SatInfo>)> {
    let device = msg.device.clone()?;
    let sats = msg
        .satellites
        .iter()
        .map(|s| SatInfo {
            gnss_id: s.gnssid,
            sv_id: s.svid,
            snr_db: s.ss,
            elevation_deg: s.elevation,
            azimuth_deg: s.azimuth,
            used: s.used,
        })
        .collect();
    Some((device, sats))
}

/// Parse gpsd's ISO-8601 timestamp. Missing / malformed time falls
/// back to wall clock with a debug log — see DD-005 §4.1 for the
/// drift this replaces.
fn parse_gpsd_time(raw: Option<&str>, device: &str) -> DateTime<Utc> {
    match raw {
        Some(s) => match DateTime::parse_from_rfc3339(s) {
            Ok(t) => t.with_timezone(&Utc),
            Err(e) => {
                debug!(%device, %s, error = ?e, "gpsd TPV time unparseable; using wall clock");
                Utc::now()
            }
        },
        None => {
            debug!(%device, "gpsd TPV without time; using wall clock");
            Utc::now()
        }
    }
}

/// Classify a TPV mode with DD-005 §11.1's "mode=3 without altitude
/// downgrades to 2D" defensive check.
fn classify_mode(mode: u8, alt_hae: Option<f64>, alt_msl: Option<f64>) -> FixMode {
    match mode {
        0 | 1 => FixMode::NoFix,
        2 => FixMode::Fix2D,
        3 if alt_hae.is_some() || alt_msl.is_some() => FixMode::Fix3D,
        3 => FixMode::Fix2D,
        _ => FixMode::NoFix,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_tpv() {
        let json = r#"{
            "class":"TPV","device":"/dev/ttyUSB0",
            "time":"2026-04-22T13:52:00.000Z",
            "mode":3,"lat":37.4,"lon":-122.1,
            "altHAE":100.0,"speed":0.5,"track":180.0,
            "eph":2.5,"epv":3.0,"used":9
        }"#;
        let msg: TpvMessage = serde_json::from_str(json).unwrap();
        let (device, fix) = parse_tpv(&msg).unwrap();
        assert_eq!(device, "/dev/ttyUSB0");
        assert_eq!(fix.mode, FixMode::Fix3D);
        assert_eq!(fix.latitude, Some(37.4));
        assert_eq!(fix.longitude, Some(-122.1));
        assert_eq!(fix.altitude_m, Some(100.0));
        assert_eq!(fix.horizontal_error_m, Some(2.5));
        assert_eq!(fix.satellites_used, 9);
    }

    #[test]
    fn tpv_mode_3_without_altitude_downgrades_to_2d() {
        let json = r#"{"class":"TPV","device":"/dev/ttyS0","mode":3,"lat":1.0,"lon":2.0}"#;
        let msg: TpvMessage = serde_json::from_str(json).unwrap();
        let (_, fix) = parse_tpv(&msg).unwrap();
        assert_eq!(fix.mode, FixMode::Fix2D);
    }

    #[test]
    fn tpv_synthesizes_eph_from_epx_epy() {
        let json =
            r#"{"class":"TPV","device":"/dev/x","mode":2,"lat":0.0,"lon":0.0,"epx":3.0,"epy":4.0}"#;
        let msg: TpvMessage = serde_json::from_str(json).unwrap();
        let (_, fix) = parse_tpv(&msg).unwrap();
        // sqrt(3^2 + 4^2) = 5
        assert_eq!(fix.horizontal_error_m, Some(5.0));
    }

    #[test]
    fn tpv_missing_device_is_dropped() {
        let json = r#"{"class":"TPV","mode":2,"lat":1.0,"lon":2.0}"#;
        let msg: TpvMessage = serde_json::from_str(json).unwrap();
        assert!(parse_tpv(&msg).is_none());
    }

    #[test]
    fn tpv_no_fix_mode() {
        for mode in [0u8, 1] {
            let json = format!(r#"{{"class":"TPV","device":"/dev/x","mode":{mode}}}"#);
            let msg: TpvMessage = serde_json::from_str(&json).unwrap();
            let (_, fix) = parse_tpv(&msg).unwrap();
            assert_eq!(fix.mode, FixMode::NoFix);
        }
    }

    #[test]
    fn tpv_time_falls_back_to_wall_clock_when_unparseable() {
        let json =
            r#"{"class":"TPV","device":"/dev/x","mode":2,"time":"not-a-time","lat":1.0,"lon":2.0}"#;
        let msg: TpvMessage = serde_json::from_str(json).unwrap();
        let before = Utc::now();
        let (_, fix) = parse_tpv(&msg).unwrap();
        let after = Utc::now();
        assert!(fix.time >= before && fix.time <= after);
    }

    #[test]
    fn parses_sky_with_satellites() {
        let json = r#"{
            "class":"SKY","device":"/dev/ttyUSB0",
            "satellites":[
                {"gnssid":0,"svid":5,"ss":45.0,"el":40.0,"az":120.0,"used":true},
                {"gnssid":2,"svid":12,"ss":35.5,"el":60.0,"az":90.0,"used":false}
            ]
        }"#;
        let msg: SkyMessage = serde_json::from_str(json).unwrap();
        let (device, sats) = parse_sky(&msg).unwrap();
        assert_eq!(device, "/dev/ttyUSB0");
        assert_eq!(sats.len(), 2);
        assert_eq!(sats[0].gnss_id, 0);
        assert_eq!(sats[0].sv_id, 5);
        assert!(sats[0].used);
        assert!(!sats[1].used);
    }

    #[test]
    fn sky_without_device_dropped() {
        let json = r#"{"class":"SKY","satellites":[]}"#;
        let msg: SkyMessage = serde_json::from_str(json).unwrap();
        assert!(parse_sky(&msg).is_none());
    }
}

//! gpsd JSON wire types. See DD-005 §6.3.
//!
//! Every structure here mirrors the shape gpsd 3.x emits per
//! `man 5 gpsd_json`. Every non-class field on user-visible messages
//! is optional because gpsd omits fields it doesn't have data for.

use serde::Deserialize;

/// Top-level tagged enum. gpsd marks every JSON object with a
/// `"class"` key; `#[serde(tag = "class")]` dispatches to the
/// right variant in one pass.
#[derive(Debug, Deserialize)]
#[serde(tag = "class")]
pub enum GpsdMessage {
    #[serde(rename = "VERSION")]
    Version(VersionMessage),
    #[serde(rename = "TPV")]
    Tpv(TpvMessage),
    #[serde(rename = "SKY")]
    Sky(SkyMessage),
    #[serde(rename = "DEVICES")]
    Devices(DevicesMessage),
    #[serde(rename = "DEVICE")]
    Device(DeviceMessage),
    #[serde(rename = "WATCH")]
    Watch(WatchMessage),
    #[serde(rename = "ERROR")]
    Error(ErrorMessage),
    /// Any class we don't care about (POLL, ATT, PPS, …).
    #[serde(other)]
    Other,
}

/// `{"class":"VERSION","release":"3.23.1","rev":"3.23.1","proto_major":3,"proto_minor":14}`
#[derive(Debug, Clone, Deserialize)]
pub struct VersionMessage {
    pub release: String,
    pub proto_major: u32,
    pub proto_minor: u32,
}

/// Time / position / velocity. All fields except `class` and `device`
/// are optional per gpsd's protocol.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TpvMessage {
    pub device: Option<String>,
    /// ISO-8601 string; gpsd 3.x uses `YYYY-MM-DDThh:mm:ss.sssZ`.
    pub time: Option<String>,
    /// gpsd `mode`: 0=unset, 1=no-fix, 2=2D, 3=3D.
    #[serde(default)]
    pub mode: u8,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Altitude, ellipsoid (preferred when present).
    #[serde(rename = "altHAE")]
    pub alt_hae: Option<f64>,
    /// Altitude, MSL (legacy).
    #[serde(rename = "altMSL")]
    pub alt_msl: Option<f64>,
    /// Speed over ground, m/s.
    pub speed: Option<f64>,
    /// Course over ground, degrees.
    pub track: Option<f64>,
    /// Horizontal-error estimate (95% CI), meters.
    pub eph: Option<f64>,
    /// Longitude error, m (combined with `epy` when `eph` absent).
    pub epx: Option<f64>,
    /// Latitude error, m.
    pub epy: Option<f64>,
    /// Vertical error, m.
    pub epv: Option<f64>,
    /// Satellites used in this fix.
    pub used: Option<u32>,
}

/// Satellite-view snapshot.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkyMessage {
    pub device: Option<String>,
    #[serde(default)]
    pub satellites: Vec<SkySat>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SkySat {
    /// GNSS constellation id: 0=GPS, 1=SBAS, 2=Galileo, 3=BeiDou,
    /// 5=QZSS, 6=GLONASS, 7=IRNSS.
    #[serde(default)]
    pub gnssid: u8,
    /// PRN / SV id.
    #[serde(default)]
    pub svid: u16,
    /// SNR, dB-Hz.
    pub ss: Option<f32>,
    #[serde(rename = "el")]
    pub elevation: Option<f32>,
    #[serde(rename = "az")]
    pub azimuth: Option<f32>,
    #[serde(default)]
    pub used: bool,
}

/// `?DEVICES` reply — full device list.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DevicesMessage {
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceInfo {
    pub path: String,
    /// ISO-8601 timestamp; `None` means device is deactivated.
    pub activated: Option<String>,
}

/// Single-device status change (activate / deactivate).
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceMessage {
    pub path: Option<String>,
    pub activated: Option<String>,
}

/// `?WATCH` echo.
#[derive(Debug, Clone, Deserialize)]
pub struct WatchMessage {
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub json: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorMessage {
    pub message: String,
}

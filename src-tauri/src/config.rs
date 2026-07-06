use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Feed {
    pub name: String,
    /// URL template with {lat} {lon} {nm} placeholders.
    pub point_url: String,
    /// URL template with a {sqk} placeholder.
    pub sqk_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub home_lat: f64,
    pub home_lon: f64,
    /// "What's the loud thing" radius, km.
    pub overhead_radius_km: f64,
    /// Only traffic at or below this counts as overhead, feet.
    pub overhead_ceiling_ft: f64,
    /// Poll A query radius, nautical miles (feeds cap at 250).
    pub regional_radius_nm: f64,
    /// ICAO type codes (exact) or callsign prefixes, case-insensitive.
    pub watchlist: Vec<String>,
    pub poll_local_secs: u64,
    /// Duration of one full squawk sweep (all codes, staggered).
    pub poll_sqk_secs: u64,
    /// An aircraft that leaves and re-enters a class within this window
    /// does not re-toast.
    pub alert_cooldown_secs: u64,
    pub default_zoom_km: f64,
    pub zoom_steps_km: Vec<f64>,
    pub emergency_squawks: Vec<String>,
    pub toast_sound: bool,
    /// NEXRAD weather underlay on the disc.
    pub wx_enabled: bool,
    /// Altitude band filter for the disc (display only). Ceiling at the slider
    /// max means "and above". Ground traffic reads as 0 ft.
    pub alt_filter_on: bool,
    pub alt_floor_ft: f64,
    pub alt_ceiling_ft: f64,
    /// "auto" (WorkerW, fall back to bottom) | "workerw" | "bottom" | "normal"
    pub desktop_mode: String,
    pub feeds: Vec<Feed>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            home_lat: 41.8781,
            home_lon: -87.6298,
            overhead_radius_km: 4.0,
            overhead_ceiling_ft: 6000.0,
            regional_radius_nm: 100.0,
            watchlist: vec![
                "K35R", "R135", "E3TF", "E3CF", "E6", "C130", "C17", "B52", "U2", "VC25", "A10",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            poll_local_secs: 5,
            poll_sqk_secs: 60,
            alert_cooldown_secs: 600,
            default_zoom_km: 30.0,
            zoom_steps_km: vec![10.0, 20.0, 30.0, 50.0, 100.0, 185.0, 300.0, 463.0],
            emergency_squawks: vec!["7500".into(), "7600".into(), "7700".into()],
            toast_sound: false,
            wx_enabled: false,
            alt_filter_on: false,
            alt_floor_ft: 0.0,
            alt_ceiling_ft: 50000.0,
            desktop_mode: "auto".into(),
            feeds: vec![
                Feed {
                    name: "adsb.lol".into(),
                    point_url: "https://api.adsb.lol/v2/lat/{lat}/lon/{lon}/dist/{nm}".into(),
                    sqk_url: "https://api.adsb.lol/v2/sqk/{sqk}".into(),
                },
                Feed {
                    name: "adsb.fi".into(),
                    point_url: "https://opendata.adsb.fi/api/v2/lat/{lat}/lon/{lon}/dist/{nm}"
                        .into(),
                    sqk_url: "https://opendata.adsb.fi/api/v2/sqk/{sqk}".into(),
                },
                Feed {
                    name: "airplanes.live".into(),
                    point_url: "https://api.airplanes.live/v2/point/{lat}/{lon}/{nm}".into(),
                    sqk_url: "https://api.airplanes.live/v2/squawk/{sqk}".into(),
                },
            ],
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("adsb-radar")
        .join("config.json")
}

pub fn load() -> Config {
    let p = config_path();
    match fs::read_to_string(&p) {
        // Tolerate a UTF-8 BOM — hand-edits from Windows tools add one.
        Ok(s) => serde_json::from_str(s.trim_start_matches('\u{feff}')).unwrap_or_else(|e| {
            eprintln!("config parse error ({e}), using defaults");
            Config::default()
        }),
        Err(_) => {
            let c = Config::default();
            let _ = save(&c);
            c
        }
    }
}

pub fn save(cfg: &Config) -> Result<(), String> {
    let p = config_path();
    if let Some(dir) = p.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(&p, json).map_err(|e| e.to_string())
}

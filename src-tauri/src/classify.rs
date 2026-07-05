use crate::config::Config;
use crate::feeds::Aircraft;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0088;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * a.sqrt().asin()
}

pub fn bearing_deg(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    (y.atan2(x).to_degrees() + 360.0) % 360.0
}

pub fn compass(bearing: f64) -> &'static str {
    const PTS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    PTS[(((bearing + 22.5) / 45.0) as usize) % 8]
}

fn alt_ft(ac: &Aircraft) -> Option<f64> {
    ac.alt_baro.as_ref().and_then(|v| v.as_f64())
}

fn on_ground(ac: &Aircraft) -> bool {
    matches!(&ac.alt_baro, Some(v) if v.as_str() == Some("ground"))
}

#[derive(Clone, Debug, Serialize)]
pub struct UiAircraft {
    pub hex: String,
    pub flight: Option<String>,
    pub reg: Option<String>,
    pub t: Option<String>,
    pub desc: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt_baro: serde_json::Value,
    pub gs: Option<f64>,
    pub track: Option<f64>,
    pub baro_rate: Option<f64>,
    pub squawk: Option<String>,
    pub emergency: Option<String>,
    pub category: Option<String>,
    pub db_flags: u32,
    pub seen: Option<f64>,
    pub dst_km: Option<f64>,
    pub bearing: Option<f64>,
    /// Nearest airport code, set only when the aircraft is on the ground.
    pub airport: Option<String>,
    pub airport_name: Option<String>,
    pub overhead: bool,
    pub interesting: bool,
    pub is_emergency: bool,
    pub reasons: Vec<String>,
}

pub fn classify(ac: &Aircraft, cfg: &Config) -> UiAircraft {
    let (dst_km, bearing) = match (ac.lat, ac.lon) {
        (Some(lat), Some(lon)) => (
            Some(haversine_km(cfg.home_lat, cfg.home_lon, lat, lon)),
            Some(bearing_deg(cfg.home_lat, cfg.home_lon, lat, lon)),
        ),
        _ => (None, None),
    };
    // AOG: name the field it's parked at.
    let ap = match (on_ground(ac), ac.lat, ac.lon) {
        (true, Some(lat), Some(lon)) => crate::airports::nearest(lat, lon, 10.0),
        _ => None,
    };
    let flags = ac.db_flags.unwrap_or(0);
    let mut reasons: Vec<String> = Vec::new();

    let overhead = !on_ground(ac)
        && dst_km.is_some_and(|d| d <= cfg.overhead_radius_km)
        && alt_ft(ac).map_or(true, |a| a <= cfg.overhead_ceiling_ft);
    if overhead {
        reasons.push("overhead".into());
    }

    let mut interesting = false;
    if flags & 1 != 0 {
        interesting = true;
        reasons.push("military".into());
    }
    if flags & 2 != 0 {
        interesting = true;
        reasons.push("interesting".into());
    }
    if ac.category.as_deref() == Some("B2") {
        interesting = true;
        reasons.push("balloon".into());
    }
    let type_u = ac.t.as_deref().unwrap_or("").trim().to_uppercase();
    let flight_u = ac.flight.as_deref().unwrap_or("").trim().to_uppercase();
    for w in &cfg.watchlist {
        let w = w.trim().to_uppercase();
        if w.is_empty() {
            continue;
        }
        if type_u == w || (!flight_u.is_empty() && flight_u.starts_with(&w)) {
            interesting = true;
            reasons.push(format!("watchlist:{w}"));
            break;
        }
    }

    // The `emergency` field is DO-260B priority status — a superset of the
    // 7x00 squawks — so it is the primary signal; squawk match is backup.
    let emerg = ac.emergency.as_deref().unwrap_or("none");
    let mut is_emergency = !emerg.is_empty() && emerg != "none";
    if is_emergency {
        reasons.push(format!("emergency:{emerg}"));
    }
    if let Some(sq) = &ac.squawk {
        if cfg.emergency_squawks.iter().any(|s| s == sq) {
            if !is_emergency {
                reasons.push(format!("squawk:{sq}"));
            }
            is_emergency = true;
        }
    }

    UiAircraft {
        hex: ac.hex.clone(),
        flight: ac.flight.as_ref().map(|f| f.trim().to_string()),
        reg: ac.r.clone(),
        t: ac.t.clone(),
        desc: ac.desc.clone(),
        lat: ac.lat,
        lon: ac.lon,
        alt_baro: ac.alt_baro.clone().unwrap_or(serde_json::Value::Null),
        gs: ac.gs,
        track: ac.track,
        baro_rate: ac.baro_rate,
        squawk: ac.squawk.clone(),
        emergency: ac.emergency.clone(),
        category: ac.category.clone(),
        db_flags: flags,
        seen: ac.seen,
        dst_km,
        bearing,
        airport: ap.map(|a| a.code.to_string()),
        airport_name: ap.map(|a| a.name.to_string()),
        overhead,
        interesting,
        is_emergency,
        reasons,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlertClass {
    Interesting,
    Emergency,
}

struct Entry {
    active: bool,
    last_fired: Option<Instant>,
    last_seen: Instant,
}

/// Edge-triggered alert latch, keyed on hex. One engine per alert class.
pub struct AlertEngine {
    map: HashMap<String, Entry>,
}

impl AlertEngine {
    pub fn new() -> Self {
        AlertEngine { map: HashMap::new() }
    }

    /// Record that `hex` currently matches the class. Returns true exactly
    /// when a toast should fire: on entry, and only if outside the cooldown.
    pub fn observe(&mut self, hex: &str, cooldown: Duration) -> bool {
        let now = Instant::now();
        let e = self.map.entry(hex.to_string()).or_insert(Entry {
            active: false,
            last_fired: None,
            last_seen: now,
        });
        e.last_seen = now;
        let was_active = e.active;
        e.active = true;
        if !was_active && e.last_fired.map_or(true, |t| now.duration_since(t) >= cooldown) {
            e.last_fired = Some(now);
            return true;
        }
        false
    }

    /// Deactivate everything not in `present` (exit edge), and drop entries
    /// stale enough that their cooldown memory no longer matters.
    pub fn sweep(&mut self, present: &HashSet<String>) {
        let now = Instant::now();
        for (hex, e) in self.map.iter_mut() {
            if !present.contains(hex) {
                e.active = false;
            }
        }
        self.map
            .retain(|_, e| now.duration_since(e.last_seen) < Duration::from_secs(3600));
    }
}

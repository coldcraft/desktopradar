//! Nearest-airport lookup for aircraft on the ground ("which ramp is that
//! C-17 sitting on"). Data: OurAirports (public domain), large + medium
//! airports, embedded at build time as `code|name|lat|lon` lines where code
//! is IATA when it exists, else the ICAO/GPS ident.

use crate::classify::haversine_km;
use std::sync::OnceLock;

pub struct Airport {
    pub code: &'static str,
    pub name: &'static str,
    pub lat: f64,
    pub lon: f64,
}

static DATA: &str = include_str!("../airports.dat");
static AIRPORTS: OnceLock<Vec<Airport>> = OnceLock::new();

fn airports() -> &'static [Airport] {
    AIRPORTS.get_or_init(|| {
        DATA.lines()
            .filter_map(|line| {
                let mut it = line.split('|');
                let code = it.next()?;
                let name = it.next()?;
                let lat: f64 = it.next()?.parse().ok()?;
                let lon: f64 = it.next()?.parse().ok()?;
                Some(Airport { code, name, lat, lon })
            })
            .collect()
    })
}

/// Nearest airport within `max_km`, or None. Linear scan over ~5k entries —
/// only called for on-ground aircraft, which are a handful per poll.
pub fn nearest(lat: f64, lon: f64, max_km: f64) -> Option<&'static Airport> {
    let mut best: Option<(&Airport, f64)> = None;
    for ap in airports() {
        // cheap prefilter: 1° latitude ≈ 111 km
        if (ap.lat - lat).abs() * 111.0 > max_km {
            continue;
        }
        let d = haversine_km(lat, lon, ap.lat, ap.lon);
        if d <= max_km && best.map_or(true, |(_, bd)| d < bd) {
            best = Some((ap, d));
        }
    }
    best.map(|(ap, _)| ap)
}

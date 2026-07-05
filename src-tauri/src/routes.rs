use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;

/// adsbdb origin→destination cache. Hits and confirmed unknowns are cached
/// per callsign; transport errors are not, so a blip can be retried.
pub struct RouteCache {
    map: Mutex<HashMap<String, Option<Value>>>,
}

impl RouteCache {
    pub fn new() -> Self {
        RouteCache {
            map: Mutex::new(HashMap::new()),
        }
    }
}

fn condense_airport(ap: &Value) -> Value {
    serde_json::json!({
        "iata": ap.get("iata_code"),
        "icao": ap.get("icao_code"),
        "name": ap.get("name"),
        "city": ap.get("municipality"),
        "country": ap.get("country_iso_name"),
        "lat": ap.get("latitude"),
        "lon": ap.get("longitude"),
    })
}

fn angdiff(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(360.0);
    if d > 180.0 {
        360.0 - d
    } else {
        d
    }
}

/// Sanity-check a callsign route against where the aircraft actually is and
/// where it's pointed. adsbdb maps callsign → filed route with no reality
/// check, and airlines reuse callsigns — "LGA → HOU" climbing northeast out
/// of Midway is stale data, not a scenic route. None = not enough data.
pub fn plausible(route: &Value, lat: f64, lon: f64, track: Option<f64>) -> Option<bool> {
    use crate::classify::{bearing_deg, haversine_km};
    let coord = |side: &str| -> Option<(f64, f64)> {
        let ap = route.get(side)?;
        Some((ap.get("lat")?.as_f64()?, ap.get("lon")?.as_f64()?))
    };
    let (olat, olon) = coord("origin")?;
    let (dlat, dlon) = coord("destination")?;
    let d_orig = haversine_km(lat, lon, olat, olon);
    let d_dest = haversine_km(lat, lon, dlat, dlon);
    // Near either endpoint anything goes (vectors, pattern work).
    if d_orig < 150.0 || d_dest < 150.0 {
        return Some(true);
    }
    // Corridor test: en route, origin→aircraft→destination shouldn't detour
    // meaningfully past the direct leg.
    let leg = haversine_km(olat, olon, dlat, dlon);
    if d_orig + d_dest > leg + 300.0 {
        return Some(false);
    }
    // Heading test: mid-route you fly roughly toward the destination.
    if let Some(t) = track {
        if angdiff(t, bearing_deg(lat, lon, dlat, dlon)) > 100.0 {
            return Some(false);
        }
    }
    Some(true)
}

pub async fn lookup(http: &reqwest::Client, cache: &RouteCache, callsign: &str) -> Option<Value> {
    let key: String = callsign
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    if key.is_empty() {
        return None;
    }
    if let Some(v) = cache.map.lock().unwrap().get(&key) {
        return v.clone();
    }

    let url = format!("https://api.adsbdb.com/v0/callsign/{key}");
    let resp = match http.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return None, // transport error: don't cache
    };
    let status = resp.status();
    if status.as_u16() == 404 {
        cache.map.lock().unwrap().insert(key, None);
        return None;
    }
    if !status.is_success() {
        return None; // 429/5xx: don't cache
    }
    let v: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return None,
    };
    let route = v
        .get("response")
        .and_then(|r| r.get("flightroute"))
        .map(|fr| {
            serde_json::json!({
                "callsign": fr.get("callsign"),
                "airline": fr.get("airline").and_then(|a| a.get("name")),
                "origin": fr.get("origin").map(condense_airport),
                "destination": fr.get("destination").map(condense_airport),
            })
        });
    cache.map.lock().unwrap().insert(key, route.clone());
    route
}

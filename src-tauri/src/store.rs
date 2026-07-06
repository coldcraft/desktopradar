// Append-only sightings store. One row per airframe (keyed on ICAO hex).
// Passive polling fills it; a user "Catch" click stamps `caught_at`, promoting
// a logged contact into the dex. Every view (Contacts / Dex / Milestones) is a
// query over this one table — see HANDOFF.md.
//
// Owned entirely by Rust, matching the app's "all data lives in Rust; the
// webview only renders" split. If the DB fails to open we degrade to a no-op so
// the radar keeps working (same spirit as config parse errors → defaults).

use crate::classify::UiAircraft;
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Store {
    conn: Mutex<Option<Connection>>,
}

/// One caught airframe, for the dex view. Unix-second timestamps; the UI turns
/// them into relative ages.
#[derive(Serialize)]
pub struct DexEntry {
    pub hex: String,
    pub registration: Option<String>,
    pub type_code: Option<String>,
    pub type_desc: Option<String>,
    pub operator: Option<String>,
    pub callsign: Option<String>,
    pub squawk: Option<String>,
    pub notable: bool,
    pub notable_reason: Option<String>,
    pub caught_at: i64,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    pub seen_count: i64,
    /// Self-calibrating tier from this feed's own type-frequency distribution:
    /// common | uncommon | rare | legendary. None when the type is unknown.
    pub rarity: Option<String>,
}

/// One personal achievement — unlocked by your own catches, no leaderboard.
#[derive(Serialize)]
pub struct Achievement {
    pub key: String,
    pub title: String,
    /// Description when locked-and-eventful, or "n / target" progress for
    /// threshold achievements.
    pub note: String,
    pub unlocked: bool,
    /// caught_at of the catch that unlocked it, when that moment is knowable.
    pub at: Option<i64>,
}

/// Milestones drawer payload: headline collection stats + achievements.
#[derive(Serialize, Default)]
pub struct Milestones {
    pub total_seen: i64,
    pub total_caught: i64,
    pub distinct_types: i64,
    pub distinct_operators: i64,
    pub shinies: i64,
    pub achievements: Vec<Achievement>,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Integer feet from the alt_baro value; "ground" reads as 0, unknown as NULL.
fn alt_ft(a: &UiAircraft) -> Option<i64> {
    if let Some(n) = a.alt_baro.as_f64() {
        Some(n as i64)
    } else if a.alt_baro.as_str() == Some("ground") {
        Some(0)
    } else {
        None
    }
}

/// The single most salient reason this contact is worth logging, or None if
/// it's ordinary traffic. Emergencies rank above watchlist/military flavor.
fn notable_reason(a: &UiAircraft) -> Option<String> {
    if a.is_emergency {
        for r in &a.reasons {
            if let Some(e) = r.strip_prefix("emergency:") {
                return Some(e.to_uppercase());
            }
            if let Some(s) = r.strip_prefix("squawk:") {
                return Some(format!("SQK {s}"));
            }
        }
        return Some("EMERGENCY".into());
    }
    if a.interesting {
        for r in &a.reasons {
            if r == "military" {
                return Some("military".into());
            }
            if r == "balloon" {
                return Some("balloon".into());
            }
            if let Some(w) = r.strip_prefix("watchlist:") {
                return Some(format!("watch {w}"));
            }
            if r == "interesting" {
                return Some("interesting".into());
            }
        }
        return Some("interesting".into());
    }
    None
}

/// Bucket every known type into a rarity tier from THIS feed's own frequency
/// distribution — no hardcoded table. `counts` is (type_code, distinct-airframe
/// count). Types are ranked busiest-first; a type falls in the tier where the
/// running share of all sightings crosses each threshold, so the busiest ~50%
/// of traffic is common and the long-tail ~5% is legendary. Self-recalibrates
/// as the log grows.
fn rarity_tiers(counts: &[(String, i64)]) -> HashMap<String, &'static str> {
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    let mut out = HashMap::new();
    if total == 0 {
        return out;
    }
    // busiest first
    let mut ranked: Vec<&(String, i64)> = counts.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));
    let mut cum = 0i64;
    for (ty, n) in ranked {
        cum += n;
        let share = cum as f64 / total as f64;
        let tier = if share <= 0.50 {
            "common"
        } else if share <= 0.80 {
            "uncommon"
        } else if share <= 0.95 {
            "rare"
        } else {
            "legendary"
        };
        out.insert(ty.clone(), tier);
    }
    out
}

impl Store {
    /// Open (creating if needed) the sightings DB next to config.json. Never
    /// panics: on failure it logs and returns a no-op store.
    pub fn open() -> Self {
        match Self::try_open() {
            Ok(conn) => Store {
                conn: Mutex::new(Some(conn)),
            },
            Err(e) => {
                eprintln!("sightings store disabled ({e}); catches won't persist");
                Store {
                    conn: Mutex::new(None),
                }
            }
        }
    }

    fn try_open() -> Result<Connection, String> {
        let path = crate::config::config_path()
            .parent()
            .map(|d| d.join("sightings.db"))
            .ok_or("no config dir")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS sightings (
                 hex            TEXT PRIMARY KEY,
                 first_seen_at  INTEGER NOT NULL,
                 last_seen_at   INTEGER NOT NULL,
                 registration   TEXT,
                 type_code      TEXT,
                 type_desc      TEXT,
                 operator       TEXT,
                 callsign       TEXT,
                 squawk         TEXT,
                 notable        INTEGER NOT NULL DEFAULT 0,
                 notable_reason TEXT,
                 caught_at      INTEGER,
                 lat            REAL,
                 lon            REAL,
                 alt            INTEGER,
                 seen_count     INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS idx_sightings_caught  ON sightings(caught_at);
             CREATE INDEX IF NOT EXISTS idx_sightings_notable ON sightings(notable);",
        )
        .map_err(|e| e.to_string())?;
        // Best-effort auto-backup on every launch — a rare catch should never
        // be one mishap away from gone. Never fails the open.
        if let Some(dir) = path.parent() {
            Self::backup(&conn, &dir.join("backups"));
        }
        Ok(conn)
    }

    /// Write a consolidated snapshot into `dir` (VACUUM INTO folds in the WAL),
    /// then keep only the newest few. Strictly additive w.r.t. the live DB — it
    /// creates/removes files under `dir` but never touches sightings.db.
    fn backup(conn: &Connection, dir: &Path) {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("dex backup dir failed: {e}");
            return;
        }
        let out = dir.join(format!("dex-{}.db", now_secs()));
        if out.exists() {
            return; // already snapshotted this second
        }
        // Path is our own config dir; escape quotes for the SQL string literal.
        let lit = out.to_string_lossy().replace('\'', "''");
        match conn.execute(&format!("VACUUM INTO '{lit}'"), []) {
            Ok(_) => Self::prune(dir, 10),
            Err(e) => eprintln!("dex backup failed: {e}"),
        }
    }

    /// Keep the newest `keep` `dex-<secs>.db` snapshots in `dir`, deleting older
    /// ones. Only ever removes files matching that exact pattern.
    fn prune(dir: &Path, keep: usize) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut snaps: Vec<(i64, std::path::PathBuf)> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter_map(|p| {
                let name = p.file_name()?.to_str()?.to_string();
                let ts = name.strip_prefix("dex-")?.strip_suffix(".db")?.parse::<i64>().ok()?;
                Some((ts, p))
            })
            .collect();
        snaps.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        for (_, p) in snaps.into_iter().skip(keep) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Passive log: upsert one contact. First-seen and caught_at are never
    /// overwritten; known reg/type survive a later poll that omits them; the
    /// notable flag latches on once true.
    pub fn record(&self, a: &UiAircraft) {
        if a.hex.is_empty() {
            return;
        }
        let guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_ref() else { return };
        Self::upsert(conn, a);
    }

    /// Upsert a whole poll's worth of contacts in one transaction.
    pub fn record_batch<'a, I>(&self, items: I)
    where
        I: IntoIterator<Item = &'a UiAircraft>,
    {
        let mut guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_mut() else { return };
        let tx = match conn.transaction() {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("sightings batch begin failed: {e}");
                return;
            }
        };
        for a in items {
            if a.hex.is_empty() {
                continue;
            }
            Self::upsert(&tx, a);
        }
        if let Err(e) = tx.commit() {
            eprintln!("sightings batch commit failed: {e}");
        }
    }

    fn upsert(conn: &Connection, a: &UiAircraft) {
        let now = now_secs();
        let notable = notable_reason(a);
        let callsign = a.flight.as_deref().filter(|s| !s.is_empty());
        let r = conn.execute(
            "INSERT INTO sightings
                (hex, first_seen_at, last_seen_at, registration, type_code,
                 type_desc, operator, callsign, squawk, notable, notable_reason,
                 lat, lon, alt, seen_count)
             VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1)
             ON CONFLICT(hex) DO UPDATE SET
                 last_seen_at   = ?2,
                 registration   = COALESCE(excluded.registration, registration),
                 type_code      = COALESCE(excluded.type_code, type_code),
                 type_desc      = COALESCE(excluded.type_desc, type_desc),
                 operator       = COALESCE(excluded.operator, operator),
                 callsign       = COALESCE(excluded.callsign, callsign),
                 squawk         = excluded.squawk,
                 notable        = MAX(notable, excluded.notable),
                 notable_reason = COALESCE(excluded.notable_reason, notable_reason),
                 lat            = COALESCE(excluded.lat, lat),
                 lon            = COALESCE(excluded.lon, lon),
                 alt            = COALESCE(excluded.alt, alt),
                 seen_count     = seen_count + 1",
            rusqlite::params![
                a.hex,
                now,
                a.reg,
                a.t,
                a.desc,
                a.operator,
                callsign,
                a.squawk,
                notable.is_some() as i64,
                notable,
                a.lat,
                a.lon,
                alt_ft(a),
            ],
        );
        if let Err(e) = r {
            eprintln!("sightings upsert {} failed: {e}", a.hex);
        }
    }

    /// Deliberate catch: stamp caught_at if not already set. Returns Ok(true)
    /// when this click is what promoted the row into the dex, Ok(false) if it
    /// was already caught or no such row exists.
    pub fn catch(&self, hex: &str) -> Result<bool, String> {
        let guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_ref() else {
            return Err("sightings store unavailable".into());
        };
        let changed = conn
            .execute(
                "UPDATE sightings SET caught_at = ?1 WHERE hex = ?2 AND caught_at IS NULL",
                rusqlite::params![now_secs(), hex],
            )
            .map_err(|e| e.to_string())?;
        Ok(changed > 0)
    }

    /// Hexes already in the dex, so the UI can mark caught rows.
    pub fn caught_hexes(&self) -> Vec<String> {
        let guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_ref() else {
            return Vec::new();
        };
        let mut stmt = match conn.prepare("SELECT hex FROM sightings WHERE caught_at IS NOT NULL") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| row.get::<_, String>(0));
        match rows {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The dex: every caught airframe, newest catch first. One row per hex
    /// (hex is the primary key, so "grouped by hex" is inherent).
    pub fn dex(&self) -> Vec<DexEntry> {
        let guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_ref() else {
            return Vec::new();
        };

        // Rarity is calibrated over the WHOLE log, not just caught rows — one
        // airframe per hex, so COUNT(*) per type = distinct airframes of it.
        let tiers = {
            let mut counts: Vec<(String, i64)> = Vec::new();
            if let Ok(mut stmt) = conn.prepare(
                "SELECT type_code, COUNT(*) FROM sightings
                 WHERE type_code IS NOT NULL AND type_code <> ''
                 GROUP BY type_code",
            ) {
                if let Ok(it) =
                    stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                {
                    counts = it.filter_map(|r| r.ok()).collect();
                }
            }
            rarity_tiers(&counts)
        };

        let mut stmt = match conn.prepare(
            "SELECT hex, registration, type_code, type_desc, operator, callsign,
                    squawk, notable, notable_reason, caught_at, first_seen_at,
                    last_seen_at, seen_count
             FROM sightings
             WHERE caught_at IS NOT NULL
             ORDER BY caught_at DESC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map([], |row| {
            let type_code: Option<String> = row.get(2)?;
            let notable = row.get::<_, i64>(7)? != 0;
            // an emergency/notable catch is always a shiny — top tier
            let rarity = if notable {
                Some("legendary".to_string())
            } else {
                type_code
                    .as_deref()
                    .and_then(|t| tiers.get(t))
                    .map(|s| s.to_string())
            };
            Ok(DexEntry {
                hex: row.get(0)?,
                registration: row.get(1)?,
                type_code,
                type_desc: row.get(3)?,
                operator: row.get(4)?,
                callsign: row.get(5)?,
                squawk: row.get(6)?,
                notable,
                notable_reason: row.get(8)?,
                caught_at: row.get(9)?,
                first_seen_at: row.get(10)?,
                last_seen_at: row.get(11)?,
                seen_count: row.get(12)?,
                rarity,
            })
        });
        match rows {
            Ok(it) => it.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Collection stats + personal achievements for the Milestones drawer.
    pub fn milestones(&self) -> Milestones {
        let guard = self.conn.lock().unwrap();
        let Some(conn) = guard.as_ref() else {
            return Milestones::default();
        };
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
        // MIN(caught_at) under a condition — the moment a "first" was unlocked.
        let first = |cond: &str| -> Option<i64> {
            conn.query_row(
                &format!("SELECT MIN(caught_at) FROM sightings WHERE caught_at IS NOT NULL AND {cond}"),
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
        };

        let total_seen = count("SELECT COUNT(*) FROM sightings");
        let total_caught = count("SELECT COUNT(*) FROM sightings WHERE caught_at IS NOT NULL");
        let distinct_types = count(
            "SELECT COUNT(DISTINCT type_code) FROM sightings
             WHERE caught_at IS NOT NULL AND type_code IS NOT NULL AND type_code <> ''",
        );
        let distinct_operators = count(
            "SELECT COUNT(DISTINCT operator) FROM sightings
             WHERE caught_at IS NOT NULL AND operator IS NOT NULL",
        );
        let shinies = count("SELECT COUNT(*) FROM sightings WHERE caught_at IS NOT NULL AND notable = 1");

        // Ordered catch times, so a count threshold can be dated by its Nth catch.
        let times: Vec<i64> = conn
            .prepare("SELECT caught_at FROM sightings WHERE caught_at IS NOT NULL ORDER BY caught_at")
            .and_then(|mut s| {
                s.query_map([], |r| r.get::<_, i64>(0))
                    .map(|it| it.filter_map(|r| r.ok()).collect())
            })
            .unwrap_or_default();
        let nth = |n: usize| times.get(n - 1).copied();

        let mut a: Vec<Achievement> = Vec::new();
        // Threshold achievements: unlocked once the count is hit; dated by the
        // Nth catch; otherwise show progress toward the target.
        let mut threshold = |key: &str, title: &str, have: i64, target: i64| {
            let unlocked = have >= target;
            a.push(Achievement {
                key: key.into(),
                title: title.into(),
                note: if unlocked { "unlocked".into() } else { format!("{have} / {target}") },
                unlocked,
                at: if unlocked { nth(target as usize) } else { None },
            });
        };
        threshold("first", "First Contact", total_caught, 1);
        threshold("caught10", "Getting Started", total_caught, 10);
        threshold("caught25", "Collector", total_caught, 25);
        threshold("caught50", "Spotter", total_caught, 50);
        threshold("caught100", "Centurion", total_caught, 100);
        threshold("types10", "Type Hunter", distinct_types, 10);
        threshold("types25", "Type Master", distinct_types, 25);
        threshold("ops5", "Frequent Flyer", distinct_operators, 5);
        threshold("ops10", "Airline Bingo", distinct_operators, 10);
        threshold("shiny5", "Treasure Hunter", shinies, 5);

        // Event achievements: unlocked by a specific kind of catch, dated by it.
        let mut event = |key: &str, title: &str, desc: &str, at: Option<i64>| {
            a.push(Achievement {
                key: key.into(),
                title: title.into(),
                note: desc.into(),
                unlocked: at.is_some(),
                at,
            });
        };
        event("military", "Brass", "Catch a military aircraft", first("notable_reason = 'military'"));
        event("shiny", "Shiny", "Catch a notable aircraft", first("notable = 1"));
        event("emergency", "Mayday", "Catch an emergency squawk", first("squawk IN ('7500','7600','7700')"));
        event("nordo", "Radio Silence", "Catch a NORDO — squawk 7600", first("squawk = '7600'"));
        event("hijack", "Unlawful", "Catch a hijack squawk — 7500", first("squawk = '7500'"));

        Milestones {
            total_seen,
            total_caught,
            distinct_types,
            distinct_operators,
            shinies,
            achievements: a,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_newest_snapshots_and_spares_everything_else() {
        let dir = std::env::temp_dir().join(format!("adsb_prune_{}", now_secs()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..=15 {
            std::fs::write(dir.join(format!("dex-{i}.db")), b"x").unwrap();
        }
        // Decoys that MUST survive — prune only touches dex-<n>.db.
        std::fs::write(dir.join("sightings.db"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        Store::prune(&dir, 10);

        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // decoys untouched
        assert!(left.contains(&"sightings.db".to_string()));
        assert!(left.contains(&"notes.txt".to_string()));
        // 10 newest snapshots kept (dex-6..=15), older 5 removed
        assert_eq!(left.iter().filter(|n| n.starts_with("dex-")).count(), 10);
        assert!(left.contains(&"dex-15.db".to_string()));
        assert!(left.contains(&"dex-6.db".to_string()));
        assert!(!left.contains(&"dex-5.db".to_string()));
        assert!(!left.contains(&"dex-1.db".to_string()));

        std::fs::remove_dir_all(&dir).ok();
    }
}

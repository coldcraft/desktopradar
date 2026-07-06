#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod airports;
mod classify;
mod config;
#[cfg(windows)]
mod desktop;
mod feeds;
mod operators;
mod routes;
mod store;

use classify::{classify, compass, AlertClass, AlertEngine, UiAircraft};
use config::Config;
use feeds::FeedClient;
use store::Store;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, RwLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

struct AppState {
    config: RwLock<Config>,
    client: FeedClient,
    /// Append-only sightings log + dex. Passive polls fill it; Catch stamps it.
    store: Store,
    /// Freshest UiAircraft by hex from the last local poll, so a Catch click
    /// can persist full contact detail even between polls.
    latest_ac: Mutex<HashMap<String, UiAircraft>>,
    routes: routes::RouteCache,
    /// NEXRAD tile cache: (z, x, y, 5-min bucket) → data URL.
    wx_tiles: Mutex<HashMap<(u32, u32, u32, u64), String>>,
    engine_interesting: Mutex<AlertEngine>,
    engine_emergency: Mutex<AlertEngine>,
    /// Hexes currently emergency-classified by poll A (local) and poll B
    /// (global); the emergency engine sweeps against their union so the two
    /// pollers don't clear each other's edges.
    emerg_local: Mutex<HashSet<String>>,
    emerg_global: Mutex<HashSet<String>>,
    attach_mode: Mutex<String>,
    /// Current disc zoom (km) reported by the UI; the point poll widens to
    /// cover it so zooming out actually reveals distant traffic.
    view_radius_km: Mutex<f64>,
}

#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> Config {
    state.config.read().unwrap().clone()
}

/// The UI reports its current disc zoom so the point poll can widen to cover
/// what's visible (capped at 250 NM in the feed layer).
#[tauri::command]
fn set_view_radius(km: f64, state: tauri::State<'_, AppState>) {
    *state.view_radius_km.lock().unwrap() = km.max(0.0);
}

#[tauri::command]
fn set_config(new_cfg: Config, state: tauri::State<'_, AppState>) -> Result<(), String> {
    config::save(&new_cfg)?;
    *state.config.write().unwrap() = new_cfg;
    Ok(())
}

#[tauri::command]
async fn get_route(
    callsign: String,
    lat: Option<f64>,
    lon: Option<f64>,
    track: Option<f64>,
    state: tauri::State<'_, AppState>,
) -> Result<Option<serde_json::Value>, String> {
    let route = routes::lookup(state.client.http(), &state.routes, &callsign).await;
    Ok(route.map(|mut r| {
        let verdict = match (lat, lon) {
            (Some(la), Some(lo)) => routes::plausible(&r, la, lo, track),
            _ => None,
        };
        if let Some(obj) = r.as_object_mut() {
            obj.insert(
                "plausible".into(),
                verdict.map_or(serde_json::Value::Null, serde_json::Value::Bool),
            );
        }
        r
    }))
}

/// Fires a fake interesting-traffic toast so users can confirm notifications
/// work (and look right) without waiting for real traffic. Reached from both
/// the settings panel and the tray menu.
fn send_test_toast(app: &AppHandle) {
    #[cfg(windows)]
    {
        use tauri_winrt_notification::{Duration as ToastDuration, Toast};
        let r = Toast::new(AUMID)
            .title("✈ Test contact")
            .text1("RADAR1 · B738 · 2,500 ft · 1 km N — toasts are working")
            .duration(ToastDuration::Short)
            .sound(None)
            .show();
        if let Err(e) = r {
            eprintln!("test toast failed: {e:?}");
        }
    }
    let _ = app.emit(
        "radar:toast",
        serde_json::json!({
            "class": "interesting",
            "title": "✈ Test contact",
            "body": "RADAR1 · B738 · 2,500 ft · 1 km N — toasts are working",
            "hex": "test",
        }),
    );
}

#[tauri::command]
fn test_toast(app: AppHandle) {
    send_test_toast(&app);
}

/// Open the aircraft's live track on globe.adsbexchange.com in the default
/// browser. The globe map is free to eyeball (the paid part is the API);
/// this is the "dig into a NORDO" escape hatch. Opens even for a contact
/// that has faded from local scope, since the card keeps its hex.
#[tauri::command]
fn open_globe(hex: String) -> Result<String, String> {
    let clean: String = hex
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_lowercase();
    if clean.is_empty() {
        return Err("no ICAO hex to track".into());
    }
    let url = format!("https://globe.adsbexchange.com/?icao={clean}");
    // opener uses ShellExecuteW on Windows — no console flash.
    opener::open_browser(&url).map_err(|e| e.to_string())?;
    Ok(url)
}

/// Promote a live contact into the dex: record its latest detail (so the row
/// is complete even if caught seconds after first appearing) then stamp
/// caught_at. Returns true when this click is what caught it.
#[tauri::command]
fn catch_contact(hex: String, state: tauri::State<'_, AppState>) -> Result<bool, String> {
    if let Some(a) = state.latest_ac.lock().unwrap().get(&hex).cloned() {
        state.store.record(&a);
    }
    state.store.catch(&hex)
}

/// Hexes already caught, so the Contacts drawer can mark them.
#[tauri::command]
fn dex_hexes(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.store.caught_hexes()
}

/// The full dex: every caught airframe, newest first, for the Dex drawer.
#[tauri::command]
fn dex(state: tauri::State<'_, AppState>) -> Vec<store::DexEntry> {
    state.store.dex()
}

/// Collection stats + personal achievements for the Milestones drawer.
#[tauri::command]
fn milestones(state: tauri::State<'_, AppState>) -> store::Milestones {
    state.store.milestones()
}

/// type_code -> rarity tier, so the Contacts list can flag rare birds live.
#[tauri::command]
fn rarity_map(state: tauri::State<'_, AppState>) -> std::collections::HashMap<String, String> {
    state.store.rarity_map()
}

/// NEXRAD composite tile (Iowa Environmental Mesonet, no key), returned as a
/// data URL so the webview never does HTTP and the canvas never taints.
/// Cached per 5-minute bucket, matching IEM's refresh cadence.
#[tauri::command]
async fn get_wx_tile(
    z: u32,
    x: u32,
    y: u32,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let bucket = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs()
        / 300;
    let key = (z, x, y, bucket);
    if let Some(hit) = state.wx_tiles.lock().unwrap().get(&key) {
        return Ok(hit.clone());
    }
    let url = format!(
        "https://mesonet.agron.iastate.edu/cache/tile.py/1.0.0/nexrad-n0q-900913/{z}/{x}/{y}.png"
    );
    let resp = state
        .client
        .http()
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("wx http {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    let data_url = format!("data:image/png;base64,{}", STANDARD.encode(&bytes));
    {
        let mut cache = state.wx_tiles.lock().unwrap();
        cache.retain(|k, _| k.3 == bucket); // drop stale buckets
        cache.insert(key, data_url.clone());
    }
    Ok(data_url)
}

#[tauri::command]
fn set_activatable(on: bool, window: tauri::WebviewWindow, state: tauri::State<'_, AppState>) {
    // Only relevant in bottom mode, where WS_EX_NOACTIVATE is applied.
    if *state.attach_mode.lock().unwrap() != "bottom" {
        return;
    }
    #[cfg(windows)]
    if let Ok(h) = window.hwnd() {
        desktop::set_activatable(h.0 as isize, on);
        if on {
            let _ = window.set_focus();
        }
    }
    #[cfg(not(windows))]
    let _ = (on, window);
}

/// Toast identity. Unpackaged/NSIS apps need an AppUserModelID registered in
/// HKCU (display name + icon) for toasts to show branded — otherwise the
/// fallback is PowerShell's AUMID, which brands toasts "Windows PowerShell"
/// and can spawn an empty PowerShell console when one is clicked.
#[cfg(windows)]
const AUMID: &str = "com.shavlik.adsbradar";

#[cfg(windows)]
fn register_aumid() -> Result<(), String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    // Icon shown on the toast: materialize the embedded .ico next to config.
    let icon_path = config::config_path()
        .parent()
        .map(|d| d.join("icon.ico"))
        .ok_or("no config dir")?;
    let icon_bytes: &[u8] = include_bytes!("../icons/icon.ico");
    let needs_write = std::fs::metadata(&icon_path)
        .map(|m| m.len() != icon_bytes.len() as u64)
        .unwrap_or(true);
    if needs_write {
        if let Some(dir) = icon_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&icon_path, icon_bytes).map_err(|e| e.to_string())?;
    }
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(format!("Software\\Classes\\AppUserModelId\\{AUMID}"))
        .map_err(|e| e.to_string())?;
    key.set_value("DisplayName", &"ADS-B Radar")
        .map_err(|e| e.to_string())?;
    key.set_value("IconUri", &icon_path.to_string_lossy().to_string())
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn fmt_alt(a: &UiAircraft) -> String {
    if let Some(n) = a.alt_baro.as_f64() {
        format!("{} ft", n as i64)
    } else if a.alt_baro.as_str() == Some("ground") {
        match &a.airport {
            Some(code) => format!("on ground @ {code}"),
            None => "on ground".into(),
        }
    } else {
        "alt ?".into()
    }
}

fn primary_reason(a: &UiAircraft) -> String {
    for r in &a.reasons {
        match r.as_str() {
            "military" => return "Military traffic".into(),
            "balloon" => return "Balloon".into(),
            "interesting" => return "Interesting aircraft".into(),
            _ => {}
        }
        if let Some(w) = r.strip_prefix("watchlist:") {
            return format!("Watchlist {w}");
        }
    }
    if a.overhead {
        return "Overhead".into();
    }
    "Nearby traffic".into()
}

fn send_toast(app: &AppHandle, class: AlertClass, a: &UiAircraft, feed: &str) {
    let callsign = a
        .flight
        .clone()
        .filter(|f| !f.is_empty())
        .or_else(|| a.reg.clone())
        .unwrap_or_else(|| a.hex.clone());
    let typ = a
        .t
        .clone()
        .or_else(|| a.desc.clone())
        .unwrap_or_else(|| "type ?".into());
    let alt = fmt_alt(a);
    let dist = match (a.dst_km, a.bearing) {
        (Some(d), Some(b)) => format!("{:.0} km {}", d, compass(b)),
        _ => "position unknown".into(),
    };
    let (title, body) = match class {
        AlertClass::Interesting => (
            format!("✈ {}", primary_reason(a)),
            format!("{callsign} · {typ} · {alt} · {dist}"),
        ),
        AlertClass::Emergency => {
            let what = a
                .emergency
                .clone()
                .filter(|e| e != "none" && !e.is_empty())
                .or_else(|| a.squawk.clone().map(|s| format!("squawk {s}")))
                .unwrap_or_else(|| "emergency".into());
            (
                format!("🚨 EMERGENCY — {what}"),
                format!("{callsign} · {typ} · {alt} · {dist} · via {feed}"),
            )
        }
    };

    // Mirror every toast into the UI event log regardless of OS toast success.
    let _ = app.emit(
        "radar:toast",
        serde_json::json!({
            "class": match class { AlertClass::Interesting => "interesting", AlertClass::Emergency => "emergency" },
            "title": title, "body": body, "hex": a.hex,
        }),
    );

    #[cfg(windows)]
    {
        use tauri_winrt_notification::{Duration as ToastDuration, Toast};
        let sound_on = app
            .state::<AppState>()
            .config
            .read()
            .unwrap()
            .toast_sound;
        let app2 = app.clone();
        let hex = a.hex.clone();
        let mut toast = Toast::new(AUMID)
            .title(&title)
            .text1(&body)
            .duration(ToastDuration::Short);
        if !sound_on {
            toast = toast.sound(None); // silent by requirement
        }
        let toast = toast.on_activated(move |_arg| {
            // Click peeks the gadget (it lives on the desktop layer, so a
            // plain raise can't bring it forward) and opens the card.
            if let Some(w) = app2.get_webview_window("main") {
                let _ = w.show();
                if let Ok(h) = w.hwnd() {
                    desktop::peek(h.0 as isize, 8);
                }
            }
            let _ = app2.emit("radar:focus", hex.clone());
            Ok(())
        });
        if let Err(e) = toast.show() {
            eprintln!("toast failed: {e:?}");
        }
    }
    #[cfg(not(windows))]
    let _ = feed;
}

async fn poll_local(app: AppHandle) {
    loop {
        let cfg = app.state::<AppState>().config.read().unwrap().clone();
        let result = {
            let state = app.state::<AppState>();
            // Cover the configured regional radius OR the current disc zoom,
            // whichever is larger, so zooming out reveals distant traffic.
            let view_nm = *state.view_radius_km.lock().unwrap() / 1.852;
            let eff_nm = cfg.regional_radius_nm.max(view_nm);
            state.client.point(&cfg, eff_nm).await
        };
        match result {
            Ok((feed, resp)) => {
                let mut ac: Vec<UiAircraft> = resp
                    .ac
                    .iter()
                    .filter(|a| a.seen.unwrap_or(0.0) < 60.0)
                    .map(|a| classify(a, &cfg))
                    .collect();
                ac.sort_by(|a, b| {
                    a.dst_km
                        .unwrap_or(f64::MAX)
                        .partial_cmp(&b.dst_km.unwrap_or(f64::MAX))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                // Passive log: the antenna hears everything, so every contact
                // gets an append-only row. Refresh the by-hex map so a Catch
                // click can persist full detail between polls.
                {
                    let state = app.state::<AppState>();
                    state.store.record_batch(ac.iter());
                    let mut latest = state.latest_ac.lock().unwrap();
                    latest.clear();
                    for a in &ac {
                        latest.insert(a.hex.clone(), a.clone());
                    }
                }

                let cooldown = Duration::from_secs(cfg.alert_cooldown_secs);
                let mut fire: Vec<(AlertClass, UiAircraft)> = Vec::new();
                {
                    let state = app.state::<AppState>();
                    {
                        let mut eng = state.engine_interesting.lock().unwrap();
                        let mut present = HashSet::new();
                        for a in ac.iter().filter(|a| a.overhead || a.interesting) {
                            present.insert(a.hex.clone());
                            if eng.observe(&a.hex, cooldown) {
                                fire.push((AlertClass::Interesting, a.clone()));
                            }
                        }
                        eng.sweep(&present);
                    }
                    {
                        let mut eng = state.engine_emergency.lock().unwrap();
                        let mut local = HashSet::new();
                        for a in ac.iter().filter(|a| a.is_emergency) {
                            local.insert(a.hex.clone());
                            if eng.observe(&a.hex, cooldown) {
                                fire.push((AlertClass::Emergency, a.clone()));
                            }
                        }
                        let union: HashSet<String> = local
                            .union(&state.emerg_global.lock().unwrap())
                            .cloned()
                            .collect();
                        *state.emerg_local.lock().unwrap() = local;
                        eng.sweep(&union);
                    }
                }
                for (class, a) in fire {
                    send_toast(&app, class, &a, &feed);
                }

                let events: Vec<&UiAircraft> = ac
                    .iter()
                    .filter(|a| a.overhead || a.interesting || a.is_emergency)
                    .collect();
                let mode = app.state::<AppState>().attach_mode.lock().unwrap().clone();
                let _ = app.emit(
                    "radar:update",
                    serde_json::json!({
                        "feed": feed,
                        "home": { "lat": cfg.home_lat, "lon": cfg.home_lon },
                        "mode": mode,
                        "ac": ac,
                        "events": events,
                    }),
                );
            }
            Err(e) => {
                let _ = app.emit("radar:error", format!("point poll: {e}"));
            }
        }
        tokio::time::sleep(Duration::from_secs(cfg.poll_local_secs.clamp(2, 300))).await;
    }
}

async fn poll_squawks(app: AppHandle) {
    // Payloads are usually empty; a slow staggered sweep is plenty.
    loop {
        let cfg = app.state::<AppState>().config.read().unwrap().clone();
        let squawks = cfg.emergency_squawks.clone();
        if squawks.is_empty() {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        let stagger = Duration::from_secs(
            (cfg.poll_sqk_secs.clamp(15, 600) / squawks.len() as u64).max(5),
        );
        let mut hits: HashMap<String, UiAircraft> = HashMap::new();
        let mut via = String::from("global");
        for sq in &squawks {
            let result = {
                let state = app.state::<AppState>();
                state.client.squawk(&cfg, sq).await
            };
            match result {
                Ok((feed, resp)) => {
                    via = feed;
                    for a in &resp.ac {
                        if a.seen.unwrap_or(0.0) > 120.0 {
                            continue;
                        }
                        let u = classify(a, &cfg);
                        // Everything on this endpoint wears the squawk, but
                        // classify() may not flag it if the squawk field lags;
                        // trust the endpoint.
                        let mut u = u;
                        if !u.is_emergency {
                            u.is_emergency = true;
                            u.reasons.push(format!("squawk:{sq}"));
                        }
                        hits.insert(u.hex.clone(), u);
                    }
                }
                Err(e) => {
                    let _ = app.emit("radar:error", format!("sqk {sq}: {e}"));
                }
            }
            tokio::time::sleep(stagger).await;
        }

        // Log the global emergency hits and keep them catchable from the
        // Contacts drawer even when they're outside the local disc.
        {
            let state = app.state::<AppState>();
            state.store.record_batch(hits.values());
            let mut latest = state.latest_ac.lock().unwrap();
            for (hex, u) in &hits {
                latest.insert(hex.clone(), u.clone());
            }
        }

        let cooldown = Duration::from_secs(cfg.alert_cooldown_secs);
        let mut fire: Vec<UiAircraft> = Vec::new();
        {
            let state = app.state::<AppState>();
            let mut eng = state.engine_emergency.lock().unwrap();
            let global: HashSet<String> = hits.keys().cloned().collect();
            for (hex, u) in &hits {
                if eng.observe(hex, cooldown) {
                    fire.push(u.clone());
                }
            }
            let union: HashSet<String> = global
                .union(&state.emerg_local.lock().unwrap())
                .cloned()
                .collect();
            *state.emerg_global.lock().unwrap() = global;
            eng.sweep(&union);
        }
        for a in fire {
            send_toast(&app, AlertClass::Emergency, &a, &via);
        }
        let list: Vec<&UiAircraft> = hits.values().collect();
        let _ = app.emit("radar:emergencies", serde_json::json!({ "ac": list }));
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::TrayIconBuilder;
    use tauri_plugin_autostart::ManagerExt;

    let peek = MenuItem::with_id(app, "peek", "Peek at radar", true, None::<&str>)?;
    let test = MenuItem::with_id(app, "test", "Test silent toast", true, None::<&str>)?;
    let autostart_on = app.autolaunch().is_enabled().unwrap_or(false);
    let autostart =
        CheckMenuItem::with_id(app, "autostart", "Start with Windows", true, autostart_on, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &peek,
            &test,
            &PredefinedMenuItem::separator(app)?,
            &autostart,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    let autostart_item = autostart.clone();
    TrayIconBuilder::with_id("main-tray")
        .icon(app.default_window_icon().expect("window icon").clone())
        .tooltip("ADS-B Radar")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "peek" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    #[cfg(windows)]
                    if let Ok(h) = w.hwnd() {
                        desktop::peek(h.0 as isize, 6);
                    }
                }
            }
            "test" => send_test_toast(app),
            "autostart" => {
                let al = app.autolaunch();
                let now_on = al.is_enabled().unwrap_or(false);
                let r = if now_on { al.disable() } else { al.enable() };
                if let Err(e) = r {
                    eprintln!("autostart toggle failed: {e}");
                }
                let _ = autostart_item.set_checked(al.is_enabled().unwrap_or(false));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(AppState {
            config: RwLock::new(config::load()),
            client: FeedClient::new(),
            store: Store::open(),
            latest_ac: Mutex::new(HashMap::new()),
            routes: routes::RouteCache::new(),
            wx_tiles: Mutex::new(HashMap::new()),
            engine_interesting: Mutex::new(AlertEngine::new()),
            engine_emergency: Mutex::new(AlertEngine::new()),
            emerg_local: Mutex::new(HashSet::new()),
            emerg_global: Mutex::new(HashSet::new()),
            attach_mode: Mutex::new("normal".into()),
            view_radius_km: Mutex::new(0.0),
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            get_route,
            get_wx_tile,
            test_toast,
            open_globe,
            catch_contact,
            dex_hexes,
            dex,
            milestones,
            rarity_map,
            set_view_radius,
            set_activatable
        ])
        .setup(|app| {
            let handle = app.handle().clone();

            build_tray(app)?;

            #[cfg(windows)]
            {
                if let Err(e) = register_aumid() {
                    eprintln!("AUMID registration failed ({e}); toasts may not show");
                }
                let win = app.get_webview_window("main").expect("main window");
                let mode_cfg = handle
                    .state::<AppState>()
                    .config
                    .read()
                    .unwrap()
                    .desktop_mode
                    .clone();
                if let Ok(h) = win.hwnd() {
                    let hwnd = h.0 as isize;
                    let mode = desktop::attach_to_desktop(hwnd, &mode_cfg);
                    if mode == "bottom" {
                        // WorkerW parenting keeps us glued to the desktop layer
                        // by construction; bottom mode needs periodic re-assert.
                        std::thread::spawn(move || loop {
                            desktop::pin_bottom(hwnd);
                            std::thread::sleep(Duration::from_secs(2));
                        });
                    }
                    *handle.state::<AppState>().attach_mode.lock().unwrap() = mode;
                }
            }

            let h = handle.clone();
            tauri::async_runtime::spawn(async move { poll_local(h).await });
            let h = handle.clone();
            tauri::async_runtime::spawn(async move { poll_squawks(h).await });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running adsb-radar");
}

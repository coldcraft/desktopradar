// ADS-B radar gadget — webview side. All HTTP lives in Rust; this file only
// listens to events, renders the disc, and invokes commands.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { getCurrentWindow } = window.__TAURI__.window;

// ————— state —————
let cfg = null;
let snapshot = { ac: [], events: [], home: { lat: 0, lon: 0 }, feed: "--", mode: "?" };
let globalEmerg = []; // from poll B
let lastRx = 0;
let lastError = null;
let zoomKm = 30;
let trails = new Map(); // hex -> [{lat, lon, t}]
let hitboxes = []; // [{x, y, ac}] screen-space, rebuilt each frame
let hoverHex = null;
let highlightHex = null;
let cardHex = null;

const canvas = document.getElementById("radar");
const ctx = canvas.getContext("2d");
const tooltip = document.getElementById("tooltip");

// ————— helpers —————
const KM_PER_DEG_LAT = 110.574;
function kmOffsets(ac) {
  const dx = (ac.lon - snapshot.home.lon) * 111.32 * Math.cos(snapshot.home.lat * Math.PI / 180);
  const dy = (ac.lat - snapshot.home.lat) * KM_PER_DEG_LAT;
  return { dx, dy };
}
function fmtAlt(a) {
  if (typeof a === "number") return Math.round(a).toLocaleString() + " ft";
  if (a === "ground") return "on ground";
  return "alt ?";
}
function fmtAltAc(ac) {
  if (ac.alt_baro === "ground" && ac.airport) return "on ground @ " + ac.airport;
  return fmtAlt(ac.alt_baro);
}
function altColor(a) {
  if (a === "ground") return "hsl(210, 8%, 62%)";
  if (typeof a !== "number") return "hsl(140, 70%, 65%)";
  const t = Math.max(0, Math.min(1, a / 40000));
  return `hsl(${25 + t * 185}, 95%, ${58 + t * 8}%)`;
}
function compass(b) {
  return ["N","NE","E","SE","S","SW","W","NW"][Math.round(((b % 360) + 360) % 360 / 45) % 8];
}
function callsignOf(ac) {
  return (ac.flight || "").trim() || ac.reg || ac.hex;
}
function vsArrow(rate) {
  if (rate == null || Math.abs(rate) < 100) return "";
  return rate > 0 ? " ▲" : " ▼";
}
const SURFACE_CATS = { C1: "surface vehicle (emergency)", C2: "surface vehicle (service)", C3: "obstruction", C4: "obstruction", C5: "obstruction" };
function typeLabel(ac) {
  return ac.t || SURFACE_CATS[ac.category] || "";
}
function reasonLabel(ac) {
  for (const r of ac.reasons || []) {
    if (r === "surface-vehicle") return "vehicle";
    if (r.startsWith("emergency:")) return r.slice(10).toUpperCase();
    if (r.startsWith("squawk:")) return "SQK " + r.slice(7);
    if (r === "military") return "military";
    if (r === "balloon") return "balloon";
    if (r === "interesting") return "interesting";
    if (r.startsWith("watchlist:")) return "watch " + r.slice(10);
    if (r === "overhead") return "overhead";
  }
  return "";
}

// ————— canvas sizing —————
function sizeCanvas() {
  const rect = canvas.parentElement.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  canvas.width = rect.width * dpr;
  canvas.height = rect.height * dpr;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
}
window.addEventListener("resize", sizeCanvas);

// ————— render loop —————
function draw(tms) {
  const w = canvas.clientWidth, h = canvas.clientHeight;
  const cx = w / 2, cy = h / 2;
  const R = Math.min(cx, cy) - 4;
  const scale = R / zoomKm;
  ctx.clearRect(0, 0, w, h);

  ctx.save();
  ctx.beginPath();
  ctx.arc(cx, cy, R, 0, Math.PI * 2);
  ctx.clip();

  // grid: rings + crosshair + degree ticks
  ctx.strokeStyle = "rgba(90, 220, 140, 0.22)";
  ctx.fillStyle = "rgba(140, 235, 175, 0.5)";
  ctx.lineWidth = 1;
  ctx.font = "9px Consolas, monospace";
  for (let i = 1; i <= 3; i++) {
    const r = (R * i) / 3;
    ctx.beginPath();
    ctx.arc(cx, cy, r, 0, Math.PI * 2);
    ctx.stroke();
    const km = (zoomKm * i) / 3;
    ctx.fillText(km >= 10 ? Math.round(km) : km.toFixed(1), cx + 3, cy - r + 10);
  }
  ctx.beginPath();
  ctx.moveTo(cx - R, cy); ctx.lineTo(cx + R, cy);
  ctx.moveTo(cx, cy - R); ctx.lineTo(cx, cy + R);
  ctx.stroke();
  for (let d = 0; d < 360; d += 30) {
    const a = (d - 90) * Math.PI / 180;
    ctx.beginPath();
    ctx.moveTo(cx + Math.cos(a) * (R - 6), cy + Math.sin(a) * (R - 6));
    ctx.lineTo(cx + Math.cos(a) * R, cy + Math.sin(a) * R);
    ctx.stroke();
  }
  ctx.fillStyle = "rgba(160, 245, 190, 0.75)";
  ctx.font = "bold 11px Consolas, monospace";
  ctx.fillText("N", cx - 4, cy - R + 16);

  // home dot
  ctx.fillStyle = "#c8ffdd";
  ctx.beginPath();
  ctx.arc(cx, cy, 2.5, 0, Math.PI * 2);
  ctx.fill();

  // sweep: conic afterglow TRAILING the beam. The beam rotates clockwise and
  // conic gradients also run clockwise from their start angle, so the bright
  // stop goes at 1.0 (just behind the beam), fading off counterclockwise.
  const sweepAng = ((tms % 4000) / 4000) * Math.PI * 2; // radians, 0 = north, cw
  const beam = sweepAng - Math.PI / 2;
  const grad = ctx.createConicGradient(beam, cx, cy);
  grad.addColorStop(0, "rgba(70, 220, 130, 0.0)");
  grad.addColorStop(0.70, "rgba(70, 220, 130, 0.0)");
  grad.addColorStop(0.90, "rgba(90, 235, 150, 0.10)");
  grad.addColorStop(1, "rgba(120, 255, 170, 0.30)");
  ctx.fillStyle = grad;
  ctx.fillRect(0, 0, w, h);
  ctx.strokeStyle = "rgba(190, 255, 215, 0.85)";
  ctx.lineWidth = 1.5;
  ctx.beginPath();
  ctx.moveTo(cx, cy);
  ctx.lineTo(cx + Math.cos(beam) * R, cy + Math.sin(beam) * R);
  ctx.stroke();

  // trails
  for (const [hex, pts] of trails) {
    if (pts.length < 2) continue;
    const ac = snapshot.ac.find(a => a.hex === hex);
    const col = ac ? altColor(ac.alt_baro) : "hsl(140,70%,60%)";
    for (let i = 1; i < pts.length; i++) {
      const p0 = kmOffsets(pts[i - 1]), p1 = kmOffsets(pts[i]);
      ctx.strokeStyle = col;
      ctx.globalAlpha = 0.06 + (i / pts.length) * 0.30;
      ctx.lineWidth = 1.5;
      ctx.beginPath();
      ctx.moveTo(cx + p0.dx * scale, cy - p0.dy * scale);
      ctx.lineTo(cx + p1.dx * scale, cy - p1.dy * scale);
      ctx.stroke();
    }
  }
  ctx.globalAlpha = 1;

  // blips
  hitboxes = [];
  const visible = snapshot.ac.filter(a => a.lat != null && a.lon != null && a.dst_km != null && a.dst_km <= zoomKm * 1.06);
  const drawLabels = visible.length <= 18;
  for (const ac of visible) {
    const { dx, dy } = kmOffsets(ac);
    const x = cx + dx * scale, y = cy - dy * scale;
    hitboxes.push({ x, y, ac });

    // phosphor afterglow: brightest just after the beam passes
    const blipAng = Math.atan2(dx, dy); // 0 = north, cw — matches sweepAng
    let behind = sweepAng - blipAng;
    behind = ((behind % (Math.PI * 2)) + Math.PI * 2) % (Math.PI * 2);
    const glow = Math.max(0.45, 1.15 - (behind / (Math.PI * 2)) * 1.5);

    const col = ac.is_emergency ? "#ff5546" : altColor(ac.alt_baro);
    ctx.save();
    ctx.translate(x, y);
    ctx.globalAlpha = glow;
    ctx.shadowColor = col;
    ctx.shadowBlur = 7;
    ctx.fillStyle = col;
    if (ac.track != null) {
      ctx.rotate((ac.track * Math.PI) / 180);
      ctx.beginPath(); // little dart, nose forward
      ctx.moveTo(0, -6.5);
      ctx.lineTo(5.5, 2);
      ctx.lineTo(1.6, 1);
      ctx.lineTo(2.4, 5);
      ctx.lineTo(0, 3.8);
      ctx.lineTo(-2.4, 5);
      ctx.lineTo(-1.6, 1);
      ctx.lineTo(-5.5, 2);
      ctx.closePath();
      ctx.fill();
    } else {
      ctx.beginPath();
      ctx.arc(0, 0, 3.5, 0, Math.PI * 2);
      ctx.fill();
    }
    ctx.restore();

    // status rings
    if (ac.is_emergency) {
      const pr = 10 + Math.sin(tms / 160) * 2.5;
      ctx.strokeStyle = "rgba(255, 85, 70, 0.9)";
      ctx.lineWidth = 1.5;
      ctx.beginPath(); ctx.arc(x, y, pr, 0, Math.PI * 2); ctx.stroke();
    } else if (ac.interesting) {
      ctx.strokeStyle = "rgba(255, 195, 43, 0.75)";
      ctx.lineWidth = 1.2;
      ctx.beginPath(); ctx.arc(x, y, 9, 0, Math.PI * 2); ctx.stroke();
    }
    if (ac.hex === highlightHex || ac.hex === hoverHex) {
      ctx.strokeStyle = "rgba(255,255,255,0.85)";
      ctx.setLineDash([3, 3]);
      ctx.lineWidth = 1;
      ctx.beginPath(); ctx.arc(x, y, 12, 0, Math.PI * 2); ctx.stroke();
      ctx.setLineDash([]);
    }
    if (drawLabels) {
      ctx.globalAlpha = 0.75 * glow;
      ctx.fillStyle = "#bfe8cd";
      ctx.font = "9px Consolas, monospace";
      ctx.fillText(callsignOf(ac), x + 8, y - 7);
      ctx.globalAlpha = 1;
    }
  }

  ctx.restore();
  requestAnimationFrame(draw);
}

// ————— route lookup (lazy, cached) —————
// Rust caches per callsign against adsbdb; this layer avoids re-invoking and
// lets tooltips/list rows show "ORD → DEN" once known.
const routeCache = new Map(); // callsign -> {text, ok} | null (null = none/pending)
function routeEntryFor(ac) {
  const cs = (ac.flight || "").trim();
  return (cs && routeCache.get(cs)) || null;
}
// only trustworthy routes — for places without room for a caveat
function routeTextFor(ac) {
  const e = routeEntryFor(ac);
  return e && e.ok ? e.text : null;
}
function ensureRoute(ac, onReady) {
  const cs = (ac.flight || "").trim();
  if (!cs || routeCache.has(cs)) return;
  routeCache.set(cs, null); // pending/negative until proven otherwise
  invoke("get_route", { callsign: cs, lat: ac.lat, lon: ac.lon, track: ac.track }).then(r => {
    if (r && (r.origin || r.destination)) {
      const code = ap => (ap && (ap.iata || ap.icao)) || "?";
      routeCache.set(cs, {
        text: `${code(r.origin)} → ${code(r.destination)}`,
        ok: r.plausible !== false,
      });
      if (onReady) onReady();
    }
  }).catch(() => routeCache.delete(cs));
}

// ————— hover / click —————
function nearestBlip(ev) {
  const rect = canvas.getBoundingClientRect();
  const mx = ev.clientX - rect.left, my = ev.clientY - rect.top;
  let best = null, bestD = 15;
  for (const hb of hitboxes) {
    const d = Math.hypot(hb.x - mx, hb.y - my);
    if (d < bestD) { bestD = d; best = hb; }
  }
  return best;
}

let tooltipAnchor = { x: 0, y: 0 };
function renderTooltip(a) {
  const re = routeEntryFor(a);
  const routeHtml = re && re.text
    ? (re.ok ? `<span class="tt-route">${re.text}</span><br>`
             : `<span class="tt-route stale"><s>${re.text}</s> ⚠</span><br>`)
    : "";
  tooltip.innerHTML =
    `<div class="tt-cs">${callsignOf(a)}</div>` +
    `${typeLabel(a)} ${a.desc ? "· " + a.desc : ""}<br>` +
    routeHtml +
    `${fmtAltAc(a)}${vsArrow(a.baro_rate)} · ${a.gs != null ? Math.round(a.gs) + " kt" : "spd ?"}<br>` +
    `${a.dst_km != null ? a.dst_km.toFixed(1) + " km " + compass(a.bearing || 0) : ""}` +
    `${a.squawk ? " · sqk " + a.squawk : ""}`;
  tooltip.classList.remove("hidden");
  const pad = 14;
  let tx = tooltipAnchor.x + pad, ty = tooltipAnchor.y + pad;
  const tw = tooltip.offsetWidth, th = tooltip.offsetHeight;
  if (tx + tw > window.innerWidth - 4) tx = tooltipAnchor.x - tw - pad;
  if (ty + th > window.innerHeight - 4) ty = tooltipAnchor.y - th - pad;
  tooltip.style.left = tx + "px";
  tooltip.style.top = ty + "px";
}

canvas.addEventListener("mousemove", ev => {
  const hit = nearestBlip(ev);
  hoverHex = hit ? hit.ac.hex : null;
  canvas.style.cursor = hit ? "pointer" : "crosshair";
  if (!hit) { tooltip.classList.add("hidden"); return; }
  const a = hit.ac;
  tooltipAnchor = { x: ev.clientX, y: ev.clientY };
  renderTooltip(a);
  // route arrives async; re-render if the cursor is still on this blip
  ensureRoute(a, () => { if (hoverHex === a.hex) renderTooltip(a); });
});
canvas.addEventListener("mouseleave", () => {
  hoverHex = null;
  tooltip.classList.add("hidden");
});
canvas.addEventListener("click", ev => {
  const hit = nearestBlip(ev);
  if (hit) openCard(hit.ac.hex);
});

// ————— zoom —————
function setZoomIdx(idx) {
  const steps = cfg.zoom_steps_km;
  const i = Math.max(0, Math.min(steps.length - 1, idx));
  zoomKm = steps[i];
  document.getElementById("zoom-label").textContent =
    zoomKm >= 100 ? Math.round(zoomKm) + " KM" : zoomKm + " KM";
}
function zoomIdx() {
  let best = 0, bd = Infinity;
  cfg.zoom_steps_km.forEach((s, i) => {
    const d = Math.abs(s - zoomKm);
    if (d < bd) { bd = d; best = i; }
  });
  return best;
}
document.getElementById("zoom-in").addEventListener("click", () => setZoomIdx(zoomIdx() - 1));
document.getElementById("zoom-out").addEventListener("click", () => setZoomIdx(zoomIdx() + 1));
canvas.addEventListener("wheel", ev => {
  ev.preventDefault();
  setZoomIdx(zoomIdx() + (ev.deltaY > 0 ? 1 : -1));
}, { passive: false });

// ————— aircraft card —————
function findAc(hex) {
  return snapshot.ac.find(a => a.hex === hex) || globalEmerg.find(a => a.hex === hex) || null;
}

async function openCard(hex) {
  cardHex = hex;
  highlightHex = hex;
  const card = document.getElementById("card");
  const body = document.getElementById("card-body");
  const a = findAc(hex);
  document.getElementById("settings").classList.add("hidden");
  card.classList.remove("hidden");
  if (!a) {
    document.getElementById("card-callsign").textContent = hex;
    body.innerHTML = `<i>contact faded from scope</i>`;
    return;
  }
  document.getElementById("card-callsign").textContent = callsignOf(a);
  const rows = [
    ["Type", `${typeLabel(a) || "?"}${a.desc ? " — " + a.desc : ""}`],
    ["Registration", a.reg || "—"],
    ["Altitude", a.alt_baro === "ground" && a.airport
      ? `on ground @ ${a.airport_name || ""} (${a.airport})`.replace("  ", " ")
      : fmtAlt(a.alt_baro)],
    ["Vert rate", a.baro_rate != null ? Math.round(a.baro_rate) + " ft/min" + vsArrow(a.baro_rate) : "—"],
    ["Ground spd", a.gs != null ? Math.round(a.gs) + " kt" : "—"],
    ["Distance", a.dst_km != null ? `${a.dst_km.toFixed(1)} km ${compass(a.bearing || 0)} (brg ${Math.round(a.bearing || 0)}°)` : "position unknown"],
    ["Squawk", a.squawk || "—"],
    ["Category", a.category || "—"],
    ["ICAO hex", a.hex],
  ];
  const chips = (a.reasons || []).map(r => {
    const cls = r.startsWith("emergency") || r.startsWith("squawk") ? "emergency" : (r === "military" ? "mil" : "");
    return `<span class="chip ${cls}">${r}</span>`;
  }).join("");
  body.innerHTML =
    `<table>${rows.map(([k, v]) => `<tr><td>${k}</td><td>${v}</td></tr>`).join("")}</table>` +
    (chips ? `<div class="chips">${chips}</div>` : "") +
    `<div class="route-line" id="route-line">route: looking up…</div>`;

  const rl = document.getElementById("route-line");
  const callsign = (a.flight || "").trim();
  if (!callsign) { rl.textContent = "route: no callsign broadcast"; return; }
  try {
    const route = await invoke("get_route", { callsign, lat: a.lat, lon: a.lon, track: a.track });
    if (cardHex !== hex) return; // user moved on
    if (route && (route.origin || route.destination)) {
      const f = ap => ap ? `${ap.iata || ap.icao || "?"} ${ap.city || ap.name || ""}`.trim() : "?";
      rl.innerHTML = `${f(route.origin)} <b>→</b> ${f(route.destination)}` +
        (route.airline ? `<br><span style="color:#8fd9a8">${route.airline}</span>` : "");
      if (route.plausible === false) {
        rl.classList.add("stale");
        rl.innerHTML += `<br><span class="stale-note">⚠ doesn't match position/heading — callsign route data likely stale</span>`;
      }
    } else {
      rl.textContent = "route: unknown to adsbdb";
    }
  } catch {
    rl.textContent = "route: lookup failed";
  }
}
document.getElementById("card-close").addEventListener("click", () => {
  document.getElementById("card").classList.add("hidden");
  cardHex = null;
  highlightHex = null;
});

// ————— contacts list —————
function renderEvents() {
  const ul = document.getElementById("event-list");
  const seen = new Set();
  const items = [];
  for (const a of [...(snapshot.events || []), ...globalEmerg]) {
    if (seen.has(a.hex)) continue;
    seen.add(a.hex);
    items.push(a);
  }
  items.sort((x, y) =>
    (y.is_emergency - x.is_emergency) ||
    ((x.dst_km ?? 1e9) - (y.dst_km ?? 1e9)));
  if (!items.length) {
    ul.innerHTML = `<li class="ev-empty">nothing interesting aloft</li>`;
    return;
  }
  ul.innerHTML = items.map(a => {
    const dot = a.is_emergency ? "emergency" : (a.interesting ? "interesting" : "overhead");
    const where = a.dst_km != null
      ? (a.dst_km > 999 ? Math.round(a.dst_km).toLocaleString() + " km" : a.dst_km.toFixed(0) + " km " + compass(a.bearing || 0))
      : "pos ?";
    // route (when known) beats repeating the type code; AOG shows the field
    const aog = a.alt_baro === "ground" && a.airport ? "@ " + a.airport : null;
    const detail = [routeTextFor(a), aog, a.t].filter(Boolean).slice(0, 2).join(" · ");
    return `<li data-hex="${a.hex}">` +
      `<span class="ev-dot ${dot}"></span>` +
      `<span class="ev-cs">${callsignOf(a)}</span>` +
      `<span class="ev-why">${reasonLabel(a)}${detail ? " · " + detail : ""}</span>` +
      `<span class="ev-where">${where}</span></li>`;
  }).join("");
  // warm route lookups for listed contacts; next render shows them
  items.forEach(a => ensureRoute(a));
  ul.querySelectorAll("li[data-hex]").forEach(li => {
    li.addEventListener("click", () => openCard(li.dataset.hex));
    li.addEventListener("mouseenter", () => { highlightHex = li.dataset.hex; });
    li.addEventListener("mouseleave", () => { if (cardHex !== li.dataset.hex) highlightHex = cardHex; });
  });
}

// ————— status LCD + LED —————
setInterval(() => {
  const age = lastRx ? (Date.now() - lastRx) / 1000 : null;
  document.getElementById("lcd-age").textContent =
    age == null ? "UPD --" : "UPD " + (age < 10 ? age.toFixed(0) + "S" : Math.round(age) + "S");
  const led = document.getElementById("led");
  led.className = "led " + (lastError ? "led-err" : (age == null || age > 20 ? "led-warn" : "led-ok"));
  led.title = lastError || (age == null ? "waiting for data" : "feed ok");
}, 1000);

// ————— settings —————
const S = id => document.getElementById(id);
function openSettings() {
  invoke("set_activatable", { on: true }).catch(() => {});
  S("card").classList.add("hidden");
  S("s-lat").value = cfg.home_lat;
  S("s-lon").value = cfg.home_lon;
  S("s-ohr").value = cfg.overhead_radius_km;
  S("s-ohc").value = cfg.overhead_ceiling_ft;
  S("s-reg").value = cfg.regional_radius_nm;
  S("s-watch").value = cfg.watchlist.join(", ");
  S("s-pla").value = cfg.poll_local_secs;
  S("s-plb").value = cfg.poll_sqk_secs;
  S("s-cool").value = cfg.alert_cooldown_secs;
  S("s-sqk").value = cfg.emergency_squawks.join(",");
  S("s-sound").checked = cfg.toast_sound;
  S("s-zoom").value = cfg.default_zoom_km;
  S("s-steps").value = cfg.zoom_steps_km.join(",");
  S("s-mode").value = cfg.desktop_mode;
  S("s-mode-now").textContent = "currently attached as: " + snapshot.mode;
  S("s-feeds").value = JSON.stringify(cfg.feeds, null, 1);
  S("settings-msg").textContent = "";
  S("settings").classList.remove("hidden");
}
function closeSettings() {
  S("settings").classList.add("hidden");
  invoke("set_activatable", { on: false }).catch(() => {});
}
S("btn-settings").addEventListener("click", openSettings);
S("settings-close").addEventListener("click", closeSettings);

S("settings-save").addEventListener("click", async () => {
  const msg = S("settings-msg");
  msg.className = "";
  try {
    const csv = v => v.split(",").map(s => s.trim()).filter(Boolean);
    const newCfg = {
      ...cfg,
      home_lat: parseFloat(S("s-lat").value),
      home_lon: parseFloat(S("s-lon").value),
      overhead_radius_km: parseFloat(S("s-ohr").value),
      overhead_ceiling_ft: parseFloat(S("s-ohc").value),
      regional_radius_nm: Math.min(250, parseFloat(S("s-reg").value)),
      watchlist: csv(S("s-watch").value),
      poll_local_secs: parseInt(S("s-pla").value, 10),
      poll_sqk_secs: parseInt(S("s-plb").value, 10),
      alert_cooldown_secs: parseInt(S("s-cool").value, 10),
      emergency_squawks: csv(S("s-sqk").value),
      toast_sound: S("s-sound").checked,
      default_zoom_km: parseFloat(S("s-zoom").value),
      zoom_steps_km: csv(S("s-steps").value).map(Number).filter(n => n > 0),
      desktop_mode: S("s-mode").value,
      feeds: JSON.parse(S("s-feeds").value),
    };
    for (const [k, v] of Object.entries(newCfg)) {
      if (typeof v === "number" && !isFinite(v)) throw new Error("bad number in " + k);
    }
    if (!newCfg.zoom_steps_km.length) throw new Error("zoom steps empty");
    if (!Array.isArray(newCfg.feeds) || !newCfg.feeds.length) throw new Error("feeds empty");
    await invoke("set_config", { newCfg });
    cfg = newCfg;
    setZoomIdx(zoomIdx());
    msg.className = "ok";
    msg.textContent = "saved — polling picks it up next cycle";
  } catch (e) {
    msg.textContent = "not saved: " + (e.message || e);
  }
});

// ————— window chrome —————
document.getElementById("btn-close").addEventListener("click", () => {
  getCurrentWindow().close();
});

// ————— tauri events —————
listen("radar:update", ev => {
  snapshot = ev.payload;
  lastRx = Date.now();
  lastError = null;
  const now = Date.now();
  for (const a of snapshot.ac) {
    if (a.lat == null || a.lon == null) continue;
    let t = trails.get(a.hex);
    if (!t) { t = []; trails.set(a.hex, t); }
    const last = t[t.length - 1];
    if (!last || last.lat !== a.lat || last.lon !== a.lon) {
      t.push({ lat: a.lat, lon: a.lon, t: now });
      if (t.length > 40) t.shift();
    }
  }
  // expire trails of contacts gone > 5 min
  const liveHex = new Set(snapshot.ac.map(a => a.hex));
  for (const [hex, t] of trails) {
    if (!liveHex.has(hex) && now - t[t.length - 1].t > 300000) trails.delete(hex);
  }
  document.getElementById("lcd-feed").textContent = "FEED " + snapshot.feed.toUpperCase();
  document.getElementById("lcd-count").textContent = snapshot.ac.length + " AC";
  renderEvents();
  if (cardHex && !document.getElementById("card").classList.contains("hidden")) {
    // live-refresh the open card without resetting the route line
    const a = findAc(cardHex);
    if (a) {
      /* cheap approach: re-open only when altitude row would change is
         overkill — just leave the card; user re-clicks for fresh numbers */
    }
  }
});

listen("radar:emergencies", ev => {
  globalEmerg = ev.payload.ac || [];
  renderEvents();
});

listen("radar:error", ev => {
  lastError = ev.payload;
});

listen("radar:focus", ev => {
  openCard(ev.payload);
});

// ————— boot —————
(async function init() {
  cfg = await invoke("get_config");
  zoomKm = cfg.default_zoom_km;
  setZoomIdx(zoomIdx());
  sizeCanvas();
  requestAnimationFrame(draw);
})();

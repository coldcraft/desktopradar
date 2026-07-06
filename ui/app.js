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
// PPI state: what's DRAWN is frozen at the last beam crossing, not the poll.
let latestByHex = new Map(); // hex -> freshest aircraft data from poll A
let painted = new Map(); // hex -> { ac, t } — snapshot taken as the beam passed
let prevSweep = 0;
const SWEEP_MS = 4000;
const TAU = Math.PI * 2;
// Altitude band filter (display only). ceiling === ALT_MAX means "and above".
const ALT_MAX = 50000;
const altFilter = { on: false, floor: 0, ceiling: ALT_MAX };
function altPass(ac) {
  if (!altFilter.on) return true;
  const a = ac.alt_baro === "ground" ? 0 : (typeof ac.alt_baro === "number" ? ac.alt_baro : null);
  if (a == null) return true; // unknown altitude: never hide
  if (a < altFilter.floor) return false;
  if (altFilter.ceiling < ALT_MAX && a > altFilter.ceiling) return false;
  return true;
}
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

  // NEXRAD underlay — dim, beneath the grid, so the phosphor vibe survives
  if (wx.on && wx.tiles.length) {
    ctx.globalAlpha = 0.5;
    for (const t of wx.tiles) {
      const nw = kmOffsets({ lat: t.n, lon: t.w });
      const se = kmOffsets({ lat: t.s, lon: t.e });
      const x0 = cx + nw.dx * scale, y0 = cy - nw.dy * scale;
      const x1 = cx + se.dx * scale, y1 = cy - se.dy * scale;
      ctx.drawImage(t.img, x0, y0, x1 - x0, y1 - y0);
    }
    ctx.globalAlpha = 1;
  }

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

  // PPI paint pass: a blip's drawn position refreshes only when the beam
  // crosses its latest bearing — dots update under the sweep, not in unison.
  {
    const step = (sweepAng - prevSweep + TAU) % TAU;
    if (step > 0) {
      for (const [hex, a] of latestByHex) {
        const off = kmOffsets(a);
        const ang = (Math.atan2(off.dx, off.dy) + TAU) % TAU;
        const d = (ang - prevSweep + TAU) % TAU;
        if (d > 0 && d <= step) {
          painted.set(hex, { ac: a, t: tms });
          let tr = trails.get(hex);
          if (!tr) { tr = []; trails.set(hex, tr); }
          const last = tr[tr.length - 1];
          if (!last || last.lat !== a.lat || last.lon !== a.lon) {
            tr.push({ lat: a.lat, lon: a.lon, t: Date.now() });
            if (tr.length > 40) tr.shift();
          }
        }
      }
    }
    prevSweep = sweepAng;
  }

  // trails
  for (const [hex, pts] of trails) {
    if (pts.length < 2) continue;
    const src = painted.get(hex);
    const col = src ? altColor(src.ac.alt_baro) : "hsl(140,70%,60%)";
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

  // blips — drawn from their last-painted state, decaying until repainted;
  // contacts with no fresh data ghost out over ~16 s
  hitboxes = [];
  const visible = [];
  for (const [hex, p] of painted) {
    const live = latestByHex.has(hex);
    const age = tms - p.t;
    if (!live && age > 16000) {
      painted.delete(hex);
      continue;
    }
    if (p.ac.dst_km == null || p.ac.dst_km > zoomKm * 1.06) continue;
    if (!altPass(p.ac)) continue;
    visible.push({ ac: p.ac, live, age });
  }
  const drawLabels = visible.length <= 18;
  for (const { ac, live, age } of visible) {
    const { dx, dy } = kmOffsets(ac);
    const x = cx + dx * scale, y = cy - dy * scale;
    hitboxes.push({ x, y, ac });

    // phosphor decay: bright at paint, dimming until the next pass
    let glow = Math.min(1, Math.max(0.45, 1.15 - (age / SWEEP_MS) * 1.5));
    if (!live) glow *= Math.max(0, 1 - Math.max(0, age - 8000) / 8000);
    if (glow <= 0.02) continue;

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

// ————— NEXRAD weather underlay —————
// Tiles come through Rust (get_wx_tile) as data URLs: the webview does no
// HTTP and the canvas never taints. IEM refreshes ~5-minutely; so do we.
const wx = { on: false, tiles: [], sig: "", lastFetch: 0, fetching: false };

function tile2lon(x, z) { return (x / 2 ** z) * 360 - 180; }
function tile2lat(y, z) {
  return (180 / Math.PI) * Math.atan(Math.sinh(Math.PI - (2 * Math.PI * y) / 2 ** z));
}

async function wxRefresh(force) {
  if (!wx.on || wx.fetching || !cfg) return;
  const lat = cfg.home_lat, lon = cfg.home_lon;
  const lat2ty = (la, z) => {
    const r = (la * Math.PI) / 180;
    return Math.floor(((1 - Math.log(Math.tan(r) + 1 / Math.cos(r)) / Math.PI) / 2) * 2 ** z);
  };
  // pick a zoom where the disc spans a handful of tiles; back off if the
  // math ever asks for a silly number of fetches
  let z = Math.max(4, Math.min(10,
    Math.round(Math.log2((40075 * Math.cos((lat * Math.PI) / 180)) / zoomKm)) - 1));
  let x0, x1, y0, y1;
  for (;;) {
    const latSpan = zoomKm / 110.574;
    const lonSpan = zoomKm / (111.32 * Math.cos((lat * Math.PI) / 180));
    x0 = Math.floor(((lon - lonSpan + 180) / 360) * 2 ** z);
    x1 = Math.floor(((lon + lonSpan + 180) / 360) * 2 ** z);
    y0 = lat2ty(lat + latSpan, z);
    y1 = lat2ty(lat - latSpan, z);
    if ((x1 - x0 + 1) * (y1 - y0 + 1) <= 16 || z <= 4) break;
    z--;
  }
  const sig = `${z}/${x0}-${x1}/${y0}-${y1}`;
  const now = Date.now();
  if (!force && sig === wx.sig && now - wx.lastFetch < 300000) return;
  wx.fetching = true;
  try {
    const tiles = [];
    for (let x = x0; x <= x1; x++) {
      for (let y = y0; y <= y1; y++) {
        const url = await invoke("get_wx_tile", { z, x, y });
        const img = new Image();
        await new Promise((res, rej) => { img.onload = res; img.onerror = rej; img.src = url; });
        tiles.push({ img, n: tile2lat(y, z), s: tile2lat(y + 1, z), w: tile2lon(x, z), e: tile2lon(x + 1, z) });
      }
    }
    wx.tiles = tiles;
    wx.sig = sig;
    wx.lastFetch = now;
  } catch {
    // tile hiccup: keep the previous frame, retry next cycle
  } finally {
    wx.fetching = false;
  }
}
setInterval(() => wxRefresh(false), 60000);

document.getElementById("wx-toggle").addEventListener("click", () => {
  wx.on = !wx.on;
  document.getElementById("wx-toggle").classList.toggle("latched", wx.on);
  if (wx.on) {
    wx.sig = ""; // force re-fetch for current view
    wxRefresh(true);
  } else {
    wx.tiles = [];
  }
  if (cfg) {
    cfg.wx_enabled = wx.on;
    invoke("set_config", { newCfg: cfg }).catch(() => {});
  }
});

// ————— altitude band filter —————
function fmtFt(ft) {
  if (ft <= 0) return "GND";
  if (ft >= ALT_MAX) return "50k+";
  return ft >= 1000 ? (ft / 1000) + "k" : ft + "";
}
function renderAltReadout() {
  document.getElementById("alt-readout").textContent =
    `${fmtFt(altFilter.floor)} – ${fmtFt(altFilter.ceiling)} ft`;
}
function persistAlt() {
  if (!cfg) return;
  cfg.alt_filter_on = altFilter.on;
  cfg.alt_floor_ft = altFilter.floor;
  cfg.alt_ceiling_ft = altFilter.ceiling;
  invoke("set_config", { newCfg: cfg }).catch(() => {});
}
document.getElementById("alt-toggle").addEventListener("click", () => {
  altFilter.on = !altFilter.on;
  document.getElementById("alt-toggle").classList.toggle("latched", altFilter.on);
  document.getElementById("alt-panel").classList.toggle("hidden", !altFilter.on);
  persistAlt();
});
const floorEl = document.getElementById("alt-floor");
const ceilEl = document.getElementById("alt-ceil");
function onAltInput() {
  let floor = +floorEl.value, ceil = +ceilEl.value;
  if (floor > ceil) { // keep the thumbs from crossing
    if (document.activeElement === floorEl) ceil = floor, ceilEl.value = ceil;
    else floor = ceil, floorEl.value = floor;
  }
  altFilter.floor = floor;
  altFilter.ceiling = ceil;
  renderAltReadout();
}
floorEl.addEventListener("input", onAltInput);
ceilEl.addEventListener("input", onAltInput);
floorEl.addEventListener("change", persistAlt);
ceilEl.addEventListener("change", persistAlt);

// ————— zoom —————
function setZoomIdx(idx) {
  const steps = cfg.zoom_steps_km;
  const i = Math.max(0, Math.min(steps.length - 1, idx));
  zoomKm = steps[i];
  document.getElementById("zoom-label").textContent =
    zoomKm >= 100 ? Math.round(zoomKm) + " KM" : zoomKm + " KM";
  // tell Rust to widen the poll so this zoom has data out to its edge
  invoke("set_view_radius", { km: zoomKm }).catch(() => {});
  wxRefresh(true);
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
    ["Operator", a.operator || "—"],
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
document.getElementById("card-globe").addEventListener("click", () => {
  if (cardHex) invoke("open_globe", { hex: cardHex }).catch(() => {});
});

// ————— drawers (Contacts / Dex / Milestones) —————
// One append-only sightings table lives in Rust; each drawer is a view over
// it. Passive polling fills the log; a Catch click promotes a contact into the
// dex. Contacts renders live (this pass); Dex/Milestones land next.
let activeDrawer = null;
let caughtHexes = new Set(); // hexes already in the dex, from Rust
let rarityMap = {};          // type_code -> tier, so live contacts show rarity

function openDrawer(name) {
  if (activeDrawer === name) { closeDrawer(); return; }
  activeDrawer = name;
  document.querySelectorAll(".rail-btn").forEach(b =>
    b.classList.toggle("latched", b.dataset.drawer === name));
  document.getElementById("drawer-title").textContent = name.toUpperCase();
  document.getElementById("drawer").classList.add("open");
  renderDrawer();
}
function closeDrawer() {
  activeDrawer = null;
  document.querySelectorAll(".rail-btn").forEach(b => b.classList.remove("latched"));
  document.getElementById("drawer").classList.remove("open");
}
function renderDrawer() {
  if (activeDrawer === "contacts") renderContacts();
  else if (activeDrawer === "dex") renderDex();
  else if (activeDrawer === "milestones") renderMilestones();
}
function renderPlaceholder(html) {
  document.getElementById("drawer-body").innerHTML = `<div class="drawer-empty">${html}</div>`;
}

function relTime(sec) {
  const d = Date.now() / 1000 - sec;
  if (d < 60) return "just now";
  if (d < 3600) return Math.floor(d / 60) + "m ago";
  if (d < 86400) return Math.floor(d / 3600) + "h ago";
  return Math.floor(d / 86400) + "d ago";
}

// Milestones: collection stats + personal achievements over the whole log.
// Achievements against yourself — no leaderboard. Unlocked first (most recent
// up top), then locked ones showing progress toward their target.
async function renderMilestones() {
  const body = document.getElementById("drawer-body");
  let m;
  try { m = await invoke("milestones"); } catch { renderPlaceholder("milestones unavailable"); return; }
  if (activeDrawer !== "milestones") return;
  const stat = (n, label) => `<div class="ms-stat"><span class="ms-num">${n}</span><span class="ms-lab">${label}</span></div>`;
  const stats = `<div class="ms-stats">` +
    stat(m.total_caught, "CAUGHT") + stat(m.distinct_types, "TYPES") +
    stat(m.distinct_operators, "OPS") + stat(m.shinies, "★") +
    stat(m.total_seen, "SEEN") + `</div>`;
  const all = m.achievements || [];
  const unlocked = all.filter((a) => a.unlocked).sort((x, y) => (y.at ?? 0) - (x.at ?? 0));
  const locked = all.filter((a) => !a.unlocked);
  const row = (a) => `<li class="ms-ach ${a.unlocked ? "unlocked" : "locked"}">` +
    `<span class="ms-ico">${a.unlocked ? "★" : "○"}</span>` +
    `<span class="ms-title">${a.title}</span>` +
    `<span class="ms-note">${a.note}</span>` +
    `<span class="ms-when">${a.unlocked && a.at ? relTime(a.at) : ""}</span></li>`;
  body.innerHTML = stats + `<ul class="ms-list">` + [...unlocked, ...locked].map(row).join("") + `</ul>`;
}

// The dex: your caught airframes (caught_at IS NOT NULL), newest catch first.
// One row per hex — the collection you deliberately claimed off the passive log.
async function renderDex() {
  const body = document.getElementById("drawer-body");
  let entries;
  try { entries = await invoke("dex"); } catch { renderPlaceholder("dex unavailable"); return; }
  if (activeDrawer !== "dex") return; // user toggled away while the query ran
  if (!entries.length) {
    renderPlaceholder(`Your dex is empty.<br>Open <b>CONTACTS</b> and <b>catch</b> something.`);
    return;
  }
  const types = new Set(entries.map(e => e.type_code).filter(Boolean));
  const shinies = entries.filter(e => e.notable).length;
  const stat = `${entries.length} CAUGHT · ${types.size} TYPE${types.size === 1 ? "" : "S"}` +
    (shinies ? ` · ${shinies} ★` : "");
  body.innerHTML = `<div class="dex-stat">${stat}</div><ul class="contact-list">` +
    entries.map(e => {
      const name = (e.callsign || "").trim() || e.registration || e.hex.toUpperCase();
      const type = [e.type_code, e.type_desc].filter(Boolean).join(" · ");
      const sub = [e.operator, type].filter(Boolean).join(" · ") || "—";
      const badge = e.notable ? `<span class="dex-badge">${e.notable_reason || "NOTABLE"}</span>` : "";
      const rar = e.rarity ? `<span class="rar rar-${e.rarity}">${e.rarity}</span>` : "";
      const dot = e.rarity || (e.notable ? "legendary" : "common");
      return `<li data-hex="${e.hex}">` +
        `<span class="ev-dot rd-${dot}"></span>` +
        `<span class="ev-cs">${name}</span>` +
        `<span class="ev-why">${rar}${sub}${badge}</span>` +
        `<span class="ev-where">${relTime(e.caught_at)}</span></li>`;
    }).join("") + `</ul>`;
  body.querySelectorAll("li[data-hex]").forEach(li =>
    li.addEventListener("click", () => openCard(li.dataset.hex)));
}

// Live, unfiltered — every airframe the antenna hears, so common airliners are
// catchable too (early dopamine), not just the interesting/emergency traffic.
function renderContacts() {
  const body = document.getElementById("drawer-body");
  const seen = new Set();
  const items = [];
  for (const a of [...(snapshot.ac || []), ...globalEmerg]) {
    if (!a.hex || seen.has(a.hex)) continue;
    // surface vehicles broadcast ADS-B but aren't aircraft — not catchable
    if ((a.reasons || []).includes("surface-vehicle")) continue;
    seen.add(a.hex);
    items.push(a);
  }
  items.sort((x, y) =>
    (y.is_emergency - x.is_emergency) ||
    ((x.dst_km ?? 1e9) - (y.dst_km ?? 1e9)));
  if (!items.length) {
    renderPlaceholder("nothing aloft");
    return;
  }
  body.innerHTML = `<ul class="contact-list">` + items.map(a => {
    const dot = a.is_emergency ? "emergency" : (a.interesting ? "interesting" : (a.overhead ? "overhead" : "plain"));
    const where = a.dst_km != null
      ? (a.dst_km > 999 ? Math.round(a.dst_km).toLocaleString() + " km" : a.dst_km.toFixed(0) + " km " + compass(a.bearing || 0))
      : "pos ?";
    const aog = a.alt_baro === "ground" && a.airport ? "@ " + a.airport : null;
    const detail = [a.operator, routeTextFor(a), aog, a.t].filter(Boolean).slice(0, 2).join(" · ");
    const caught = caughtHexes.has(a.hex);
    // flag rarer-than-common types so a good catch stands out in the list
    const tier = a.t ? rarityMap[a.t] : null;
    const rar = (tier && tier !== "common") ? `<span class="rar rar-${tier}">${tier}</span>` : "";
    return `<li data-hex="${a.hex}">` +
      `<span class="ev-dot ${dot}"></span>` +
      `<span class="ev-cs">${callsignOf(a)}</span>` +
      `<span class="ev-why">${rar}${detail || reasonLabel(a) || ""}</span>` +
      `<span class="ev-where">${where}</span>` +
      `<button class="catch-btn ${caught ? "caught" : ""}" data-catch="${a.hex}">${caught ? "✓ GOT" : "CATCH"}</button>` +
      `</li>`;
  }).join("") + `</ul>`;
  // warm routes only for notable traffic — warming all ~200 contacts would
  // hammer adsbdb every poll; a plain airliner's route can wait for its card
  items.filter(a => a.interesting || a.is_emergency).forEach(a => ensureRoute(a));
  body.querySelectorAll("li[data-hex]").forEach(li => {
    li.addEventListener("click", () => openCard(li.dataset.hex));
    li.addEventListener("mouseenter", () => { highlightHex = li.dataset.hex; });
    li.addEventListener("mouseleave", () => { if (cardHex !== li.dataset.hex) highlightHex = cardHex; });
  });
  body.querySelectorAll(".catch-btn").forEach(btn => {
    btn.addEventListener("click", ev => {
      ev.stopPropagation(); // don't open the card
      doCatch(btn.dataset.catch, btn);
    });
  });
}

async function doCatch(hex, btn) {
  if (caughtHexes.has(hex)) return;
  try {
    const newly = await invoke("catch_contact", { hex });
    caughtHexes.add(hex);
    btn.classList.add("caught");
    btn.textContent = "✓ GOT";
    if (newly) { btn.classList.add("flash"); btn.addEventListener("animationend", () => btn.classList.remove("flash"), { once: true }); }
  } catch (e) {
    // store unavailable — leave the button as-is so the user can retry
  }
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
S("settings-test").addEventListener("click", () => invoke("test_toast").catch(() => {}));

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

// ————— drawer rail —————
document.querySelectorAll(".rail-btn").forEach(b =>
  b.addEventListener("click", () => openDrawer(b.dataset.drawer)));
document.getElementById("drawer-close").addEventListener("click", closeDrawer);

// ————— window chrome —————
document.getElementById("btn-close").addEventListener("click", () => {
  getCurrentWindow().close();
});

// ————— tauri events —————
listen("radar:update", ev => {
  snapshot = ev.payload;
  lastRx = Date.now();
  lastError = null;
  // Fresh data feeds the paint pass; the DRAWN state only changes when the
  // sweep passes each contact (see the PPI block in draw()).
  latestByHex = new Map();
  for (const a of snapshot.ac) {
    if (a.lat == null || a.lon == null) continue;
    latestByHex.set(a.hex, a);
  }
  // expire trails of contacts gone > 5 min
  const now = Date.now();
  for (const [hex, t] of trails) {
    if (!latestByHex.has(hex) && now - t[t.length - 1].t > 300000) trails.delete(hex);
  }
  document.getElementById("lcd-feed").textContent = "FEED " + snapshot.feed.toUpperCase();
  document.getElementById("lcd-count").textContent = snapshot.ac.length + " AC";
  if (activeDrawer === "contacts") renderContacts();
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
  if (activeDrawer === "contacts") renderContacts();
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
  try { caughtHexes = new Set(await invoke("dex_hexes")); } catch { /* store off */ }
  try { rarityMap = await invoke("rarity_map"); } catch { /* store off */ }
  // the rarity distribution self-calibrates as the log grows; refresh slowly
  setInterval(async () => {
    try {
      rarityMap = await invoke("rarity_map");
      if (activeDrawer === "contacts") renderContacts();
    } catch { /* store off */ }
  }, 120000);
  zoomKm = cfg.default_zoom_km;
  // valid projection center before the first poll lands
  snapshot.home = { lat: cfg.home_lat, lon: cfg.home_lon };
  wx.on = !!cfg.wx_enabled;
  document.getElementById("wx-toggle").classList.toggle("latched", wx.on);
  // restore altitude filter
  altFilter.on = !!cfg.alt_filter_on;
  altFilter.floor = cfg.alt_floor_ft ?? 0;
  altFilter.ceiling = cfg.alt_ceiling_ft ?? ALT_MAX;
  floorEl.value = altFilter.floor;
  ceilEl.value = altFilter.ceiling;
  renderAltReadout();
  document.getElementById("alt-toggle").classList.toggle("latched", altFilter.on);
  document.getElementById("alt-panel").classList.toggle("hidden", !altFilter.on);
  setZoomIdx(zoomIdx());
  sizeCanvas();
  requestAnimationFrame(draw);
})();

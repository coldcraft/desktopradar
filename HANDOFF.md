# adsb-radar — design handoff (2026-07-06)

Carry-over brief from a planning conversation. Restart Claude in this folder
(`T:\documents\code\adsb-radar`) and start from "First move" below.

## What this is
Tauri desktop radar gadget (glossy bezel + green scope, Vista-gadget style).
Pulls ADS-B from airplanes.live / adsb.fi. Shows live contacts and fires silent
"toast" alerts for interesting/emergency aircraft. Sits on the desktop like a
Winamp/Vista widget.

## Why we're adding this
Passive spotting turned up more than expected — America250 mil traffic (Ospreys,
C-21s, Guard Black Hawks converging), and genuine rare events: real **7600
(NORDO / radio-fail)** squawks plus emergency codes. Right now those scroll past
and are gone. We want to (a) keep a permanent record of notable sightings and
(b) turn it into a low-key collection game.

Terminology note baked into the design: **true NORDO = squawk 7600 and is rare**
— treat it as a top-tier event. Do NOT conflate it with "no callsign / no Flight
ID" or squawk 1200 (VFR), which are common and benign. Flag those separately.

## Core architectural decision
**The notable-sightings log and the "pokédex" collection are ONE append-only
table, not two features.**

- Background logging is passive and complete — the antenna hears everything.
- A **"catch" is a deliberate user click** that promotes a logged contact into
  the dex. Same row, just set a `caught_at` timestamp (null until clicked).
- Passive observation fills the log; the click fills the collection. (eBird's
  "detected vs. I claimed it" distinction — this is what makes a catch feel earned.)

Every view is a query over this one table:
- **Contacts drawer** = live, unfiltered, with a Catch button per row.
- **Dex drawer** = rows where `caught_at IS NOT NULL`, grouped by `hex`.
- **Milestones drawer** = aggregate queries over the same data.
- **Notable-sightings review** = rows where a `notable` flag/reason is set.

Unique catch key = **ICAO hex** (unique per airframe). First-seen timestamp per
hex is the catch record; registration and type fall out of it.

## Rarity / game design
- **Rarity is self-calibrating** — compute from your OWN feed frequency, don't
  hardcode a table. Type seen 400× = common; seen once = legendary. A central-IL
  feed ranks airframes totally differently than one near an ANG base — that's the charm.
- **7600 / emergency squawks are the "shinies"** — auto-flag as top tier. A 7600
  is simultaneously a notable-log entry AND a legendary dex entry. The two
  features fuse here.
- **Catch tiers, ascending rarity:** operator/airline seen → type (ICAO type
  code) seen → specific tail/hex seen. Airlines fill fast (early dopamine);
  individual hexes are the long-tail grind.
- **Personal milestones, no leaderboard:** "first Osprey," "all 50 states' ANG
  tails," "every C-130 variant," "first 7600." Achievements against yourself —
  the fun part of SkyCards minus the competitive side he bounced off.

## UI direction — Vista gadgets + Winamp
- **Default view = just the scope in its bezel. Nothing else.** Cut the contacts
  table that's currently bolted underneath — it becomes a drawer.
- **Three drawer toggles on the bottom control rail** (next to ALT / WX / range):
  Contacts, Dex, Milestones.
- Clicking a toggle **slides a panel out** (CSS `transform: translate` + easing in
  the webview — trivial). One drawer open at a time. Panels reuse the
  brushed-metal / green-LCD skin for continuity.
- Reference points: Winamp's main window with playlist/EQ sliding out; Vista's
  compact face + flip-to-configure.

## First move (do this before anything else)
Add the append-only sightings table — it unlocks every other feature.

Suggested columns (adapt to whatever persistence the app already uses — check
`src-tauri/` for existing DB/state first):
- `id`
- `hex`            (ICAO 24-bit, the catch key)
- `first_seen_at`
- `last_seen_at`
- `registration`
- `type_code`      (e.g. H60, V22, LJ35)
- `type_desc`
- `operator`       (airline / mil branch, when derivable)
- `callsign`       (nullable — blank Flight ID is normal, not an error)
- `squawk`
- `notable`        (bool/flag) + `notable_reason` (7600, 7700, rare-type, etc.)
- `caught_at`      (nullable — set on user Catch click; this is the dex membership test)
- last known lat/lon/alt for review context

Then: (1) wire passive inserts/updates on every contact, (2) add the Catch
button in the Contacts drawer that stamps `caught_at`, (3) build Dex and
Milestones as read-only views. Drawer animation last — it's cosmetic once the
data model is right.

## Open questions to settle with future-me
- What's the current frontend stack / does a DB already exist in `src-tauri`?
- Dedupe window: same hex seen days apart = one row updated, or session-scoped rows?
- Does "notable" get auto-set on ingest (squawk match) or only surfaced in review?

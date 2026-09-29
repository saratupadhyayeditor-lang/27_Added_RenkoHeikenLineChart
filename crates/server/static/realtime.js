// Realtime Trading Engine tab (Rust engine, `/api/rt/*`).
//
// This mirrors the old Python app's "Realtime Trading Engine" tab: an
// AI Smart Trading header with a live P&L summary, an ORDER PLACEMENT METHOD
// block (Normal / Super / Forever-GTT / Slice-Iceberg name-only checkboxes; the
// selected method plus all of its order options live in the single Risk &
// Quantity row), the universal + AI-risk + order-rate engine
// controls, and the Running / Closed trade tables.
//
// Every decision (signals, quantity, trail, arm gating, order routing) lives in
// the Rust engine. Nothing here talks to Dhan directly, and no order can be sent
// while the engine is disarmed.

import { istTime } from "./ist.js?v=1";

const API = {
  snapshot: () => get("/api/rt/snapshot"),
  closed: () => get("/api/rt/closed"),
  pool: (force) => get("/api/rt/pool" + (force ? "?refresh=1" : "")),
  movers: () => get("/api/rt/movers"),
  trend: () => get("/api/rt/trend"),
  commodities: () => get("/api/rt/commodities"),
  templates: () => get("/api/rt/templates"),
  template: (v) => post("/api/rt/templates", v),
  selection: () => get("/api/rt/selection"),
  selectionSet: (v) => post("/api/rt/selection", v),
  staging: () => get("/api/rt/staging"),
  stagingSet: (v) => post("/api/rt/staging", v),
  entryTiming: () => get("/api/rt/entry-timing"),
  entryTimingSet: (v) => post("/api/rt/entry-timing", v),
  final: () => get("/api/rt/final"),
  finalSet: (v) => post("/api/rt/final", v),
  container: () => get("/api/rt/container"),
  stats: (range) => get("/api/rt/stats?range=" + encodeURIComponent(range || "today")),
  settings: (v) => post("/api/rt/settings", v),
  method: (v) => post("/api/rt/method", v),
  strategySave: (v) => post("/api/rt/strategies", v),
  strategyDelete: (v) => post("/api/rt/strategy/delete", v),
  engine: (v) => post("/api/rt/engine", v),
  tick: () => post("/api/rt/tick", {}),
  arm: (v) => post("/api/rt/arm", v),
  autoLots: (v) => post("/api/rt/autolots", v),
  entry: (v) => post("/api/rt/entry", v),
  close: (v) => post("/api/rt/close", v),
  squareOff: () => post("/api/rt/square_off", {}),
  reset: (v) => post("/api/rt/reset", v),
  freeze: (sid, underlying) =>
    get(`/api/rt/freeze_qty?securityId=${encodeURIComponent(sid)}&underlying=${encodeURIComponent(underlying || "")}`),
  account: () => get("/api/rt/account"),
  logs: () => get("/api/rt/logs"),
};

// Paper Trade runs this exact module inside an isolated frame that pre-sets
// `window.__PAPER__`; only the wording of the operator-facing confirmations and
// status pills changes, so a paper user is never told real orders will fire.
const PAPER = (() => {
  try { return !!window.__PAPER__; } catch (_) { return false; }
})();

async function get(url) {
  const r = await fetch(url);
  return r.json();
}
async function post(url, body) {
  const r = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
  return r.json();
}

// Same method catalogue + per-method options as the old realtimeorders.js.
const METHODS = [
  {
    key: "normal", name: "Normal Order", sdk: "place_order",
    desc: "Ek single entry order (MARKET / LIMIT / SL / SL-M).",
    extras: [
      { k: "orderType", label: "Order type", type: "select", opts: [["MARKET", "MARKET"], ["LIMIT", "LIMIT"], ["SL", "SL"], ["SL-M", "SL-M"]], def: "MARKET" },
    ],
  },
  {
    key: "super", name: "Super Order", sdk: "place_super_order",
    desc: "Entry + broker-side target + stop-loss + trailing (LIMIT / MARKET).",
    extras: [
      { k: "orderType", label: "Entry type", type: "select", opts: [["LIMIT", "LIMIT"], ["MARKET", "MARKET"]], def: "LIMIT" },
      { k: "autoSlice", label: "Auto Order Slicing", type: "check", def: false },
      { k: "sliceQty", label: "Slice qty (0=freeze)", type: "number", def: 0, cls: "rtom-slice-qty" },
    ],
  },
  {
    key: "forever", name: "Forever / GTT", sdk: "place_forever",
    desc: "Good-till-triggered order, single ya OCO leg.",
    extras: [
      { k: "orderType", label: "Order type", type: "select", opts: [["LIMIT", "LIMIT"], ["MARKET", "MARKET"]], def: "LIMIT" },
      { k: "flag", label: "Leg flag", type: "select", opts: [["SINGLE", "SINGLE"], ["OCO", "OCO"]], def: "SINGLE" },
      { k: "validity", label: "Validity", type: "select", opts: [["DAY", "DAY"], ["IOC", "IOC"]], def: "DAY" },
      { k: "triggerPrice", label: "Trigger px (0=auto)", type: "number", def: 0 },
    ],
  },
  {
    key: "slice", name: "Slice / Iceberg", sdk: "place_slice_order",
    desc: "Badi qty ko slices me todkar bhejta hai; disclosed qty dikhai deti hai.",
    extras: [
      { k: "orderType", label: "Order type", type: "select", opts: [["MARKET", "MARKET"], ["LIMIT", "LIMIT"]], def: "MARKET" },
      { k: "disclosedQty", label: "Disclosed qty", type: "number", def: 0 },
    ],
  },
];

let STATE = null;
// Guards against the 1s snapshot poll clobbering UI the user just touched.
// `refreshSeq` drops out-of-order snapshot responses; `filtersBusy` freezes the
// Bullish/Bearish checkbox row while its save round-trips, so a snapshot that
// was fetched before the click can never revert the tick.
let refreshSeq = 0;
// A snapshot response can be large and the poll is only 1s. Without this guard a
// slow/tunneled link lets a new poll start before the previous response lands, so
// every response looks "stale" and is dropped -> the whole pane freezes. Keep one
// snapshot in flight at a time; a request made while one is running is queued and
// run immediately after, so a button-driven refresh never gets lost.
let refreshInFlight = false;
let refreshQueued = false;
let filtersBusy = false;
// The strike-selection base ("Only +green premium strikes" + "Opposite side
// fallback") must stay ON because the "Execute Trade In" dropdown is designed to
// run on it. If a legacy snapshot arrives with either off, it is turned back on
// (with a one-time notice) instead of silently shipping a mismatched config.
let strikeBaseNormalized = false;
// The per-second snapshot only carries the newest slice of the closed ledger;
// the full (uncapped) ledger is paged in on demand via `/closed` and cached here.
let CLOSED_CACHE = null;
let closedLoading = false;
let closedLoadedAt = 0;
// "Show all": the Closed Trades wrapper clips the (already fully rendered) ledger
// at 230px. Toggling it lifts the cap so the whole closed book is visible at once.
// Preference persists across reloads; localStorage may be unavailable in a
// sandboxed frame, so every access is guarded.
function readClosedExpanded() {
  try { return localStorage.getItem("rtClosedExpand") === "1"; } catch (e) { return false; }
}
let closedExpanded = readClosedExpanded();
function applyClosedExpand() {
  const wrap = document.getElementById("rtClosedWrap");
  const btn = document.getElementById("rtClosedExpand");
  if (wrap) wrap.classList.toggle("rt-scroll-open", closedExpanded);
  if (btn) btn.textContent = closedExpanded ? "Collapse" : "Show all";
}
// Signature of what is currently painted in the big tables. The 1s poll rebuilds
// the whole DOM for the closed ledger (thousands of rows) and the condition log;
// doing that every second is what made the pane hang. Skip the rebuild when the
// underlying data has not changed since the last paint.
let closedRenderSig = "";
let logRenderSig = "";
// Day-scoped P&L summary for the engine panes: the summary strip shows TODAY
// only, while the full history lives in the Trade Stats tabs. Loaded on a
// throttle (and whenever a trade closes), never on the 1s poll.
let DAY_STATS = null;
let dayStatsAt = 0;
let dayStatsSig = "";
let dayStatsLoading = false;
let scannerPollAt = 0;
const CLOSED_SNAPSHOT_SLICE = 100;
let CATALOG = { indicators: [], timeframes: [], symbols: [] };
let activeMethod = "normal";
let methodInit = false;
// Per-method "Est. premium" auto-fill flag: stays true (live chart LTP) until the
// user types a value, then respects the manual entry for that method.
let priceAuto = {};
let marginReqTimer = null;
// Lot size resolved from Dhan scrip-master for the active chart instrument.
let chartLot = 0;
let timer = null;
let booted = false;
// Old-app AST selection + entry timing diagnostics.
let AST = { selected: {}, entryTiming: [] };
let astBusy = false;
let commodityPopulated = false;
let COMM_NAMES = {};
let astSelectsPopulated = false;

// Registry ids of the straight-line family. Only these may drive the NIFTY
// Trend Following "Confirm indicators" picker: a rising line assigns to the Top
// Gainer (CE) leg, a falling line to the Top Loser (PE) leg.
const STRAIGHT_LINE_IDS = [
  "slconsensus", "ovlconsensus", "autotrend", "projline", "zzline",
  "trendmaster", "panemaster", "srema", "supline", "resline",
  "pitchfork", "fibfan", "gannfan", "supplydemand", "wavefib",
  "pastruct", "vl",
];

// Full indicator-filter catalogue from the old AST engine (Bullish / Bearish
// sections). Keys are the old id minus the "astFilter" prefix, so "BullEmaTrend9"
// maps to `astFilterBullEmaTrend9` in the Python app. Every toggle is persisted
// to the Rust engine's `settings.filters` map.
const FILTER_BULL_LEGACY = [
  ["IncUp", "Detect bullish trend"],
  ["GapUp", "Gap increasing"],
  ["IncUpAll", "Detect bullish trend (all)"],
  ["CrossUp", "Crossed above"],
  ["GtUp", "Greater than"],
  ["LtUp", "Less than"],
  ["PaneCrossUp", "Pane crossover"],
  ["PaneIncUpAll", "Detect bullish trend (pane lines, all)"],
  ["BullBbwInc", "BBW increasing"],
  ["BullSmf", "Smart Money Flow bullish (SMF above signal)"],
  ["BullAsr", "Support gap widening (price rising away from support)"],
  ["BullOit", "Detect bullish trend (OI Trend)"],
  ["BullBbCrossAbove", "Close (EMA1) crossed above BB middle band (upper+lower expanding, overall trend vote: BB middle)"],
  ["BullPcCrossAbove", "Close (EMA1) crossed above price channel middle line (overall trend vote: PC middle)"],
  ["BullEma9_21", "EMA 9 crossed above EMA 21 (overall trend vote: EMA 21)"],
  ["BullEma21_35", "EMA 21 crossed above EMA 35 (overall trend vote: EMA 35)"],
  ["BullEma35_50", "EMA 35 crossed above EMA 50 (overall trend vote: EMA 50)"],
  ["BullEma50_100", "EMA 50 crossed above EMA 100 (overall trend vote: EMA 100)"],
  ["BullEma100_200", "EMA 100 crossed above EMA 200 (overall trend vote: EMA 200)"],
  ["BullEma200_300", "EMA 200 crossed above EMA 300 (overall trend vote: EMA 300)"],
  ["BullEma1_9", "EMA 1 crossed above EMA 9 (overall trend vote: EMA 9)"],
  ["BullEma1_21", "EMA 1 crossed above EMA 21 (overall trend vote: EMA 21)"],
  ["BullEma1_35", "EMA 1 crossed above EMA 35 (overall trend vote: EMA 35)"],
  ["BullEma1_50", "EMA 1 crossed above EMA 50 (overall trend vote: EMA 50)"],
  ["BullEma1_100", "EMA 1 crossed above EMA 100 (overall trend vote: EMA 100)"],
  ["BullEma1_200", "EMA 1 crossed above EMA 200 (overall trend vote: EMA 200)"],
  ["BullEma1_300", "EMA 1 crossed above EMA 300 (overall trend vote: EMA 300)"],
  ["BullEmaTrend9", "EMA 9 bullish (line rising)"],
  ["BullEmaTrend21", "EMA 21 bullish (line rising)"],
  ["BullEmaTrend35", "EMA 35 bullish (line rising)"],
  ["BullEmaTrend50", "EMA 50 bullish (line rising)"],
  ["BullEmaTrend100", "EMA 100 bullish (line rising)"],
  ["BullEmaTrend200", "EMA 200 bullish (line rising)"],
  ["BullEmaTrend300", "EMA 300 bullish (line rising)"],
  ["BullSt10_1_2", "Supertrend(10,1) crossed above Supertrend(10,2) (overall trend vote: Supertrend(10,2))"],
  ["BullSt10_2_3", "Supertrend(10,2) crossed above Supertrend(10,3) (overall trend vote: Supertrend(10,3))"],
  ["BullSt1CloseCrossAbove", "Close (EMA1) crossed above ST (overall trend vote: Supertrend(10,1))"],
  ["BullVwapCloseCrossAbove", "Close (EMA1) crossed above VWAP (overall trend vote: VWAP)"],
  ["BullPbrAo", "Detect bullish trend (AO line)"],
  ["BullPbrSmiio", "Detect bullish trend (SMI line)"],
  ["BullPbrDpo", "Detect bullish trend (DPO line)"],
  ["BullPbrMfi", "Detect bullish trend (MFI line)"],
  ["BullPbrUo", "Detect bullish trend (UO line)"],
  ["BullPbrWilliamsR", "Detect bullish trend (Williams %R line)"],
  ["BullPbrBbpct", "Detect bullish trend (BB%b line)"],
  ["BullPbrPvt", "Detect bullish trend (PVT line)"],
  ["BullPbrAd", "Detect bullish trend (A/D line)"],
  ["BullPbrSmf", "Detect bullish trend (SMF line)"],
  ["BullPbrBbwUp", "BBW line direction upward (bands expanding)"],
  ["BullPbrAtrUp", "ATR line direction upward (wide candles)"],
  ["BullPbrVoloscUp", "VolOsc line direction upward (volume picking up)"],
  ["BullMeetEma9_21", "EMA 9 above EMA 21 (level, overall trend vote: EMA 21)"],
  ["BullMeetEma21_35", "EMA 21 above EMA 35 (level, overall trend vote: EMA 35)"],
  ["BullMeetEma35_50", "EMA 35 above EMA 50 (level, overall trend vote: EMA 50)"],
  ["BullMeetEma50_100", "EMA 50 above EMA 100 (level, overall trend vote: EMA 100)"],
  ["BullMeetEma100_200", "EMA 100 above EMA 200 (level, overall trend vote: EMA 200)"],
  ["BullMeetEma200_300", "EMA 200 above EMA 300 (level, overall trend vote: EMA 300)"],
  ["BullMeetSt10_1_2", "Supertrend(10,1) above Supertrend(10,2) (level, overall trend vote: Supertrend(10,2))"],
  ["BullMeetSt10_2_3", "Supertrend(10,2) above Supertrend(10,3) (level, overall trend vote: Supertrend(10,3))"],
  ["BullMeetCloseSt", "Close (EMA1) above Supertrend (level, overall trend vote: Supertrend(10,1))"],
  ["BullMeetCloseVwap", "Close (EMA1) above VWAP (level, overall trend vote: VWAP)"],
  ["BullMeetPaneCross", "Pane crossed line pair still above (level)"],
  ["BullMeetCross", "Crossed-above indicator pair still above (level)"],
  ["BullMeetCloseBb", "Close (EMA1) above BB middle (level, overall trend vote: BB middle)"],
  ["BullMeetClosePc", "Close (EMA1) above PC middle line (level, overall trend vote: PC middle)"],
  ["BullMeetVl", "Volume Line above Signal (level)"],
  ["BullCandle", "Candlestick patterns"],
  ["BullElliott", "Elliott Wave"],
  ["BullIndicator", "Indicators"],
  ["BullPane", "Pane indicators"],
  ["BullSymmetry", "Symmetry"],
  ["BullStructure", "Chart structure"],
  ["BullAtr", "ATR / Volatility"],
];

const FILTER_BEAR_LEGACY = [
  ["IncDown", "Detect bearish trend"],
  ["GapDown", "Gap decreasing"],
  ["IncDownAll", "Detect bearish trend (all)"],
  ["CrossDown", "Crossed below"],
  ["GtDown", "Greater than"],
  ["LtDown", "Less than"],
  ["PaneCrossDown", "Pane crossover"],
  ["PaneIncDownAll", "Detect bearish trend (pane lines, all)"],
  ["BearBbwInc", "BBW increasing"],
  ["BearSmf", "Smart Money Flow bearish (SMF below signal)"],
  ["BearAsr", "Resistance gap widening (price falling away below resistance)"],
  ["BearOit", "Detect bearish trend (OI Trend)"],
  ["BearBbCrossBelow", "Close (EMA1) crossed below BB middle band (upper+lower expanding, overall trend vote: BB middle)"],
  ["BearPcCrossBelow", "Close (EMA1) crossed below price channel middle line (overall trend vote: PC middle)"],
  ["BearEma9_21", "EMA 9 crossed below EMA 21 (overall trend vote: EMA 21)"],
  ["BearEma21_35", "EMA 21 crossed below EMA 35 (overall trend vote: EMA 35)"],
  ["BearEma35_50", "EMA 35 crossed below EMA 50 (overall trend vote: EMA 50)"],
  ["BearEma50_100", "EMA 50 crossed below EMA 100 (overall trend vote: EMA 100)"],
  ["BearEma100_200", "EMA 100 crossed below EMA 200 (overall trend vote: EMA 200)"],
  ["BearEma200_300", "EMA 200 crossed below EMA 300 (overall trend vote: EMA 300)"],
  ["BearEma1_9", "EMA 1 crossed below EMA 9 (overall trend vote: EMA 9)"],
  ["BearEma1_21", "EMA 1 crossed below EMA 21 (overall trend vote: EMA 21)"],
  ["BearEma1_35", "EMA 1 crossed below EMA 35 (overall trend vote: EMA 35)"],
  ["BearEma1_50", "EMA 1 crossed below EMA 50 (overall trend vote: EMA 50)"],
  ["BearEma1_100", "EMA 1 crossed below EMA 100 (overall trend vote: EMA 100)"],
  ["BearEma1_200", "EMA 1 crossed below EMA 200 (overall trend vote: EMA 200)"],
  ["BearEma1_300", "EMA 1 crossed below EMA 300 (overall trend vote: EMA 300)"],
  ["BearEmaTrend9", "EMA 9 bearish (line falling)"],
  ["BearEmaTrend21", "EMA 21 bearish (line falling)"],
  ["BearEmaTrend35", "EMA 35 bearish (line falling)"],
  ["BearEmaTrend50", "EMA 50 bearish (line falling)"],
  ["BearEmaTrend100", "EMA 100 bearish (line falling)"],
  ["BearEmaTrend200", "EMA 200 bearish (line falling)"],
  ["BearEmaTrend300", "EMA 300 bearish (line falling)"],
  ["BearSt10_1_2", "Supertrend(10,1) crossed below Supertrend(10,2) (overall trend vote: Supertrend(10,2))"],
  ["BearSt10_2_3", "Supertrend(10,2) crossed below Supertrend(10,3) (overall trend vote: Supertrend(10,3))"],
  ["BearSt1CloseCrossBelow", "Close (EMA1) crossed below ST (overall trend vote: Supertrend(10,1))"],
  ["BearVwapCloseCrossBelow", "Close (EMA1) crossed below VWAP (overall trend vote: VWAP)"],
  ["BearPbrAo", "Detect bearish trend (AO line)"],
  ["BearPbrSmiio", "Detect bearish trend (SMI line)"],
  ["BearPbrDpo", "Detect bearish trend (DPO line)"],
  ["BearPbrMfi", "Detect bearish trend (MFI line)"],
  ["BearPbrUo", "Detect bearish trend (UO line)"],
  ["BearPbrWilliamsR", "Detect bearish trend (Williams %R line)"],
  ["BearPbrBbpct", "Detect bearish trend (BB%b line)"],
  ["BearPbrPvt", "Detect bearish trend (PVT line)"],
  ["BearPbrAd", "Detect bearish trend (A/D line)"],
  ["BearPbrSmf", "Detect bearish trend (SMF line)"],
  ["BearPbrBbwUp", "BBW line direction upward (bands expanding)"],
  ["BearPbrAtrUp", "ATR line direction upward (wide candles)"],
  ["BearPbrVoloscUp", "VolOsc line direction upward (volume picking up)"],
  ["BearMeetEma9_21", "EMA 9 below EMA 21 (level, overall trend vote: EMA 21)"],
  ["BearMeetEma21_35", "EMA 21 below EMA 35 (level, overall trend vote: EMA 35)"],
  ["BearMeetEma35_50", "EMA 35 below EMA 50 (level, overall trend vote: EMA 50)"],
  ["BearMeetEma50_100", "EMA 50 below EMA 100 (level, overall trend vote: EMA 100)"],
  ["BearMeetEma100_200", "EMA 100 below EMA 200 (level, overall trend vote: EMA 200)"],
  ["BearMeetEma200_300", "EMA 200 below EMA 300 (level, overall trend vote: EMA 300)"],
  ["BearMeetSt10_1_2", "Supertrend(10,1) below Supertrend(10,2) (level, overall trend vote: Supertrend(10,2))"],
  ["BearMeetSt10_2_3", "Supertrend(10,2) below Supertrend(10,3) (level, overall trend vote: Supertrend(10,3))"],
  ["BearMeetCloseSt", "Close (EMA1) below Supertrend (level, overall trend vote: Supertrend(10,1))"],
  ["BearMeetCloseVwap", "Close (EMA1) below VWAP (level, overall trend vote: VWAP)"],
  ["BearMeetPaneCross", "Pane crossed line pair still below (level)"],
  ["BearMeetCross", "Crossed-below indicator pair still below (level)"],
  ["BearMeetCloseBb", "Close (EMA1) below BB middle (level, overall trend vote: BB middle)"],
  ["BearMeetClosePc", "Close (EMA1) below PC middle line (level, overall trend vote: PC middle)"],
  ["BearMeetVl", "Volume Line below Signal (level)"],
  ["BearCandle", "Candlestick patterns"],
  ["BearElliott", "Elliott Wave"],
  ["BearIndicator", "Indicators"],
  ["BearPane", "Pane indicators"],
  ["BearSymmetry", "Symmetry"],
  ["BearStructure", "Chart structure"],
  ["BearAtr", "ATR / Volatility"],
];

// Complete AST indicator-filter catalogue, SECTION-WISE, exactly like the old
// app's Bullish/Bearish filter sections (static rows + the dynamically built
// Pane-extra / Momentum-Gap / Overlay / Overlay-Meet / Straight-Line groups).
// Keys are the old element id minus the "astFilter" prefix.
function _filterLayout(isBull) {
  const B = isBull ? "Bull" : "Bear";
  const above = isBull ? "above" : "below";
  const bull = isBull ? "bullish" : "bearish";
  const rise = isBull ? "rising" : "falling";
  const pbG1 = (name) => `Detect ${bull} trend (${name} line)`;
  const emaCross = (a, b) => `EMA ${a} crossed ${above} EMA ${b} (overall trend vote: EMA ${b})`;
  const meetLevel = (a, b) => `EMA ${a} ${above} EMA ${b} (level, overall trend vote: EMA ${b})`;
  const pbgSimple = (name) => `${name} main line crossed ${above} signal line (level holding - no gap logic)`;
  const pbgGap = (name) => `${name} gap holding/widening ${isBull ? "upward" : "downward"} (level)`;
  const obr = (name) => pbG1(name);
  const ovl = (name) => `Close (EMA1) ${above} ${name} (level)`;
  const sl = (name) => `${name} ${bull} trend`;

  const groups = [];

  // Base rows (side-neutral element ids without a Bull/Bear prefix for the
  // first eight; the rest carry the side prefix).
  groups.push({
    head: "",
    rows: [
      [isBull ? "IncUp" : "IncDown", `Detect ${bull} trend`],
      [isBull ? "GapUp" : "GapDown", isBull ? "Gap increasing" : "Gap decreasing"],
      [isBull ? "IncUpAll" : "IncDownAll", `Detect ${bull} trend (all)`],
      [isBull ? "CrossUp" : "CrossDown", isBull ? "Crossed above" : "Crossed below"],
      [isBull ? "GtUp" : "GtDown", "Greater than"],
      [isBull ? "LtUp" : "LtDown", "Less than"],
      [isBull ? "PaneCrossUp" : "PaneCrossDown", "Pane crossover"],
      [isBull ? "PaneIncUpAll" : "PaneIncDownAll", `Detect ${bull} trend (pane lines, all)`],
      [`${B}BbwInc`, "BBW increasing"],
      [`${B}Smf`, `Smart Money Flow ${bull} (SMF ${above} signal)`],
      [`${B}Asr`, isBull ? "Support gap widening (price rising away from support)" : "Resistance gap widening (price falling away below resistance)"],
      [`${B}Oit`, `Detect ${bull} trend (OI Trend)`],
      [`${B}${isBull ? "BbCrossAbove" : "BbCrossBelow"}`, `Close (EMA1) crossed ${above} BB middle band (upper+lower expanding, overall trend vote: BB middle)`],
      [`${B}${isBull ? "PcCrossAbove" : "PcCrossBelow"}`, `Close (EMA1) crossed ${above} price channel middle line (overall trend vote: PC middle)`],
      ...[["9", "21"], ["21", "35"], ["35", "50"], ["50", "100"], ["100", "200"], ["200", "300"]].map(([a, b]) => [`${B}Ema${a}_${b}`, emaCross(a, b)]),
    ],
  });

  // EMA & EMA 1 crossover.
  groups.push({
    head: "EMA & EMA 1 crossover (Close / EMA1 vs each EMA - cross event)",
    rows: [9, 21, 35, 50, 100, 200, 300].map((b) => [`${B}Ema1_${b}`, `EMA 1 crossed ${above} EMA ${b} (overall trend vote: EMA ${b})`]),
  });

  // EMA trend (line slope).
  groups.push({
    head: "EMA trend (each EMA line's own slope)",
    rows: [9, 21, 35, 50, 100, 200, 300].map((n) => [`${B}EmaTrend${n}`, `EMA ${n} ${bull} (line ${rise})`]),
  });

  // Supertrend / VWAP crosses.
  groups.push({
    head: "",
    rows: [
      [`${B}St10_1_2`, `Supertrend(10,1) crossed ${above} Supertrend(10,2) (overall trend vote: Supertrend(10,2))`],
      [`${B}St10_2_3`, `Supertrend(10,2) crossed ${above} Supertrend(10,3) (overall trend vote: Supertrend(10,3))`],
      [`${B}${isBull ? "St1CloseCrossAbove" : "St1CloseCrossBelow"}`, `Close (EMA1) crossed ${above} ST (overall trend vote: Supertrend(10,1))`],
      [`${B}${isBull ? "VwapCloseCrossAbove" : "VwapCloseCrossBelow"}`, `Close (EMA1) crossed ${above} VWAP (overall trend vote: VWAP)`],
    ],
  });

  // Pane Indicator Behaviour - Group 1 (direction mirror).
  groups.push({
    head: "Pane Indicator Behaviour - Group 1 (direction mirror - line behaviour)",
    rows: [
      ["Ao", "AO"], ["Smiio", "SMI"], ["Dpo", "DPO"], ["Mfi", "MFI"], ["Uo", "UO"],
      ["WilliamsR", "Williams %R"], ["Bbpct", "BB%b"], ["Pvt", "PVT"], ["Ad", "A/D"], ["Smf", "SMF"],
    ].map(([tok, name]) => [`${B}Pbr${tok}`, pbG1(name)]),
  });

  // Pane-extra direction rows (inserted right before the Momentum Gap group).
  groups.push({
    head: "",
    rows: [
      ["Cmf", "CMF"], ["Cci", "CCI"], ["Sqzmom", "Squeeze Mom."],
    ].map(([tok, name]) => [`${B}Pbr${tok}`, pbG1(name)]).concat([
      [`${B}PbrElderforce`, "Force Index soft-confirm (confirmation only - never blocks entry)"],
    ]),
  });

  // Multi-Line Momentum Gap (level).
  groups.push({
    head: `Multi-Line Momentum Gap (level - main line above signal and gap holding/widening ${isBull ? "up" : "down"})`,
    rows: [
      ["Macd", "MACD"], ["Ppo", "PPO"], ["Tsi", "TSI"], ["Stochrsi", "Stoch RSI"],
      ["Rsi", "RSI"], ["Obv", "OBV"], ["Fisher", "Fisher"],
    ].map(([tok, name]) => [`${B}Pbg${tok}`, pbgSimple(name)]).concat([
      [`${B}PbgSmiio`, isBull
        ? "SMI & Signal below Histogram, both direction downward & histogram falling (level)"
        : "SMI & Signal above Histogram, both direction upward & histogram rising (level)"],
      [`${B}PbgSmf`, pbgGap("SMF")],
      [`${B}PbgAroon`, isBull
        ? "Aroon Up above Aroon Down (level - line direction not required, gap holding/widening)"
        : "Aroon Up below Aroon Down (level - line direction not required, gap holding/widening)"],
      [`${B}PbgVortex`, isBull
        ? "Vortex VI+ above VI-, VI+ direction upward & VI- direction downward, gap widening (level)"
        : "Vortex VI- above VI+, VI- direction upward & VI+ direction downward, gap widening (level)"],
      [`${B}PbgAdx`, isBull
        ? "ADX direction upward, +DI above -DI, +DI direction upward & -DI direction downward, +DI/-DI gap widening (level)"
        : "ADX direction downward, +DI below -DI, +DI direction downward & -DI direction upward, +DI/-DI gap widening (level)"],
    ]),
  });

  // Group 2 & 3 (strength / participation).
  groups.push({
    head: "Pane Indicator Behaviour - Group 2 & 3 (strength / participation - same in both lists)",
    rows: [
      [`${B}PbrBbwUp`, "BBW line direction upward (bands expanding)"],
      [`${B}PbrAtrUp`, "ATR line direction upward (wide candles)"],
      [`${B}PbrVoloscUp`, "VolOsc line direction upward (volume picking up)"],
    ],
  });

  // Meet Condition (level).
  groups.push({
    head: "Meet Condition (level - active while relationship holds, cross not required)",
    rows: [
      ...[["9", "21"], ["21", "35"], ["35", "50"], ["50", "100"], ["100", "200"], ["200", "300"]].map(([a, b]) => [`${B}MeetEma${a}_${b}`, meetLevel(a, b)]),
      [`${B}MeetSt10_1_2`, `Supertrend(10,1) ${above} Supertrend(10,2) (level, overall trend vote: Supertrend(10,2))`],
      [`${B}MeetSt10_2_3`, `Supertrend(10,2) ${above} Supertrend(10,3) (level, overall trend vote: Supertrend(10,3))`],
      [`${B}MeetCloseSt`, `Close (EMA1) ${above} Supertrend (level, overall trend vote: Supertrend(10,1))`],
      [`${B}MeetCloseVwap`, `Close (EMA1) ${above} VWAP (level, overall trend vote: VWAP)`],
      [`${B}MeetPaneCross`, `Pane crossed line pair still ${above} (level)`],
      [`${B}MeetCross`, `Crossed-${above} indicator pair still ${above} (level)`],
      [`${B}MeetCloseBb`, `Close (EMA1) ${above} BB middle (level, overall trend vote: BB middle)`],
      [`${B}MeetClosePc`, `Close (EMA1) ${above} PC middle line (level, overall trend vote: PC middle)`],
      [`${B}MeetVl`, `Volume Line ${above} Signal (level)`],
    ],
  });

  // Overlay Meet Condition rows.
  groups.push({
    head: "",
    rows: [["Hma", "HMA"], ["Tenkan", "Ichimoku Tenkan-sen"], ["Kijun", "Ichimoku Kijun-sen"], ["Keltner", "Keltner middle"], ["Donchian", "Donchian middle"], ["TrendCore", "Trend Core"]]
      .map(([tok, name]) => [`${B}MeetOvl${tok}`, ovl(name)]),
  });

  // Overlay Indicator Behaviour (direction mirror).
  groups.push({
    head: "Overlay Indicator Behaviour (direction mirror - overlay line behaviour)",
    rows: [["Hma", "HMA"], ["Tenkan", "Ichimoku Tenkan-sen"], ["Kijun", "Ichimoku Kijun-sen"], ["SenkouA", "Ichimoku Senkou A"], ["Keltner", "Keltner middle"], ["Donchian", "Donchian middle"], ["TrendCore", "Trend Core"]]
      .map(([tok, name]) => [`${B}Obr${tok}`, obr(name)]),
  });

  // Straight Line Indicators (+ the Volume Line row).
  groups.push({
    head: "Straight Line Indicators",
    rows: [
      ["ElliottWave", "Elliott wave line"], ["SupplyDemand", "Supply Demand line"], ["PriceAction", "Price Action Trend line"],
      ["ZigZag", "ZigZag Trendline"], ["ComboMaster", "Combo Master line"], ["PaneConsensus", "Pane Consensus Signal line"],
      ["AutoTrendline", "Auto Trendline"], ["Pitchfork", "Pitchfork median line"], ["TrendProjection", "Trend Projection line"],
      ["GannFan", "Gann Fan line"], ["FibFan", "Fibonacci Fan line"], ["Srema", "S/R EMA Reversal line"],
    ].map(([tok, name]) => [`${B}Sl${tok}`, sl(name)]).concat([
      [`${B}Vl`, sl("Volume line")],
      [`${B}SlConsensus`, sl("Straight Line Consensus line")],
      [`${B}SlSupport`, sl("Support Trendline")],
      [`${B}SlResistance`, sl("Resistance Trendline")],
    ]),
  });

  // Arrow detection: trade the instant a fresh up/down arrow prints on the
  // selected indicator line. Bull side detects the bullish up arrow, Bear side
  // the bearish down arrow, using the exact same direction as the trend filters.
  groups.push({
    head: "Arrow Detection (fresh up/down arrow -> trade)",
    rows: [
      ["ElliottWave", "Elliott wave line arrow"], ["SupplyDemand", "Supply Demand line arrow"],
      ["PriceAction", "Price Action Trend line arrow"], ["ZigZag", "ZigZag Trendline arrow"],
      ["ComboMaster", "Combo Master line arrow"], ["PaneConsensus", "Pane Consensus Signal line arrow"],
      ["AutoTrendline", "Auto Trendline arrow"], ["Pitchfork", "Pitchfork median line arrow"],
      ["TrendProjection", "Trend Projection line arrow"], ["GannFan", "Gann Fan line arrow"],
      ["FibFan", "Fibonacci Fan line arrow"], ["Srema", "S/R EMA Reversal line arrow"],
      ["TrendCore", "Trend Core line arrow"], ["Oit", "OI Trend arrow"], ["Vl", "Volume line arrow"],
      ["Consensus", "Straight Line Consensus line arrow"],
      ["Support", "Support Trendline arrow"], ["Resistance", "Resistance Trendline arrow"],
    ].map(([tok, name]) => [`${B}Arrow${tok}`, name]),
  });

  // Research stream scoping.
  groups.push({
    head: "Research stream (run only this group on this side)",
    rows: [
      [`${B}Candle`, "Candlestick patterns"],
      [`${B}Elliott`, "Elliott Wave"],
      [`${B}Indicator`, "Indicators"],
      [`${B}Pane`, "Pane indicators"],
      [`${B}Symmetry`, "Symmetry"],
      [`${B}Structure`, "Chart structure"],
      [`${B}Atr`, "ATR / Volatility"],
    ],
  });

  return groups;
}

const FILTER_LAYOUT_BULL = _filterLayout(true);
const FILTER_LAYOUT_BEAR = _filterLayout(false);
const FILTER_BULL = FILTER_LAYOUT_BULL.flatMap((s) => s.rows);
const FILTER_BEAR = FILTER_LAYOUT_BEAR.flatMap((s) => s.rows);

// 1:1 key map between the Bullish and Bearish filter catalogues. Both layouts
// are produced by the same builder in the same row order, so index i in one list
// mirrors index i in the other. This also covers the first rows whose keys carry
// no Bull/Bear prefix (IncUp <-> IncDown, CrossUp <-> CrossDown, GtUp <-> GtDown,
// ...), which a naive "Bull" -> "Bear" string swap missed - making the Mirror
// button silently do nothing for exactly those rows.
const FILTER_MIRROR_TO_BEAR = {};
const FILTER_MIRROR_TO_BULL = {};
FILTER_BULL.forEach((row, i) => {
  const bear = FILTER_BEAR[i];
  if (!bear) return;
  FILTER_MIRROR_TO_BEAR[row[0]] = bear[0];
  FILTER_MIRROR_TO_BULL[bear[0]] = row[0];
});

function filterSectionHTML(side) {
  const bull = side === "Bull";
  const layout = bull ? FILTER_LAYOUT_BULL : FILTER_LAYOUT_BEAR;
  const color = bull ? "#00d4aa" : "#ef5350";
  const border = bull ? "#1f4a35" : "#4a1f1f";
  const bg = bull ? "#0e231a" : "#230e0e";
  const body = layout
    .map((sec) => {
      const head = sec.head
        ? `<div style="border-top:1px solid #2d2d50;margin-top:2px;padding-top:3px;font-size:8px;color:#ffd700">${esc(sec.head)}</div>`
        : "";
      const rows = sec.rows
        .map((f) => {
          let gear = "";
          if (/SlConsensus$/.test(f[0]) || /ArrowConsensus$/.test(f[0])) {
            gear = `<span class="sc-gear" title="Straight Line Consensus settings" style="cursor:pointer;color:#ffd700;font-size:11px;line-height:1">&#9881;</span>`;
          } else if (/SlSupport$/.test(f[0])) {
            gear = `<span class="pt-gear" data-pt="support" title="Support Trendline settings" style="cursor:pointer;color:#26a69a;font-size:11px;line-height:1">&#9881;</span>`;
          } else if (/SlResistance$/.test(f[0])) {
            gear = `<span class="pt-gear" data-pt="resistance" title="Resistance Trendline settings" style="cursor:pointer;color:#ef5350;font-size:11px;line-height:1">&#9881;</span>`;
          }
          return `<div style="display:flex;align-items:center;gap:3px"><label style="font-size:9px;color:#ccc;display:flex;align-items:center;gap:3px;flex:1"><input type="checkbox" data-filter="${f[0]}"> ${esc(f[1])}</label>${gear}</div>`;
        })
        .join("");
      return head + rows;
    })
    .join("");
  return `<div id="rtFilterSection${side}" style="border:1px solid ${border};border-radius:4px;padding:4px 8px;margin-right:10px;background:${bg};min-width:230px;max-width:340px">
    <label style="font-size:9px;color:${color};display:flex;align-items:center;gap:3px;font-weight:700">
      <input type="checkbox" data-master="${side}" style="accent-color:${color}"> ${bull ? "Bullish" : "Bearish"} <span style="font-weight:400;color:#666;font-size:8px">(select all)</span></label>
    <div style="margin-top:4px;display:flex;flex-direction:column;gap:3px;max-height:260px;overflow-y:auto">
      ${body}
    </div>
  </div>`;
}

function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function num(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}
function fmtMoney(n) {
  if (n == null || isNaN(n)) return "--";
  return "\u20b9" + Number(n).toLocaleString("en-IN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function isTruthy(v) {
  return v === true || v === 1 || v === "1" || v === "true";
}
function el(id) {
  return document.getElementById(id);
}

// In-page dialogs. Native `confirm()`/`alert()` are silently suppressed when
// the pane runs inside a sandboxed preview frame (the call just returns false),
// which made every guarded action look dead. These render a real modal so the
// confirm/alert gates work identically in the standalone page, the RT tab and
// the Paper Trade frame.
function ensureDialog() {
  let back = el("rtDialogBackdrop");
  if (back) return back;
  back = document.createElement("div");
  back.id = "rtDialogBackdrop";
  back.innerHTML =
    '<div id="rtDialog" role="dialog" aria-modal="true">' +
    '<div class="rt-dialog-title" id="rtDialogTitle"></div>' +
    '<div class="rt-dialog-msg" id="rtDialogMsg"></div>' +
    '<div class="rt-dialog-actions">' +
    '<button class="btn-action" id="rtDialogCancel">Cancel</button>' +
    '<button class="btn-action" id="rtDialogOk">OK</button>' +
    "</div></div>";
  document.body.appendChild(back);
  return back;
}

function uiDialog(msg, opts) {
  opts = opts || {};
  return new Promise((resolve) => {
    const back = ensureDialog();
    el("rtDialogTitle").textContent = opts.title || (opts.alertOnly ? "Notice" : "Confirm");
    el("rtDialogMsg").textContent = msg;
    const cancel = el("rtDialogCancel");
    const ok = el("rtDialogOk");
    cancel.style.display = opts.alertOnly ? "none" : "";
    ok.classList.toggle("danger", !!opts.danger);
    ok.textContent = opts.okText || (opts.alertOnly ? "OK" : "Confirm");
    const done = (v) => {
      back.classList.remove("show");
      back.onclick = null;
      ok.onclick = null;
      cancel.onclick = null;
      document.removeEventListener("keydown", onKey);
      resolve(v);
    };
    const onKey = (e) => {
      if (e.key === "Escape") done(false);
      else if (e.key === "Enter") done(true);
    };
    back.onclick = (e) => { if (e.target === back) done(false); };
    cancel.onclick = () => done(false);
    ok.onclick = () => done(true);
    document.addEventListener("keydown", onKey);
    back.classList.add("show");
    setTimeout(() => { try { ok.focus(); } catch (_) {} }, 30);
  });
}
function uiConfirm(msg, opts) {
  return uiDialog(msg, opts);
}
function uiAlert(msg) {
  return uiDialog(msg, { alertOnly: true });
}

// Single working "Square Off All Trades" action shared by both tabs (shared
// module: the Paper frame rewrites /api/rt -> /api/paper, so the same call
// squares off the simulated book there). It closes every running engine trade at
// market and, on the real tab, also flattens any broker manual/residual qty.
async function squareOffAllTrades() {
  const pos = (STATE && STATE.positions) || [];
  const n = pos.length;
  const msg = PAPER
    ? "Square Off ALL running paper trades at market?\n\nSimulated exits - koi real Dhan order nahi jayega." +
      (n ? "\n\n" + n + " running trade(s) close honge." : "\n\nAbhi koi running trade nahi hai.")
    : "Square Off ALL running trades at market?\n\nExits WILL be sent to Dhan." +
      (n ? "\n\n" + n + " running trade(s) close honge." : "");
  if (!(await uiConfirm(msg, { danger: true }))) return;
  try {
    await API.squareOff();
  } catch (e) {
    await uiAlert("Square Off All Trades failed: " + (e && e.message ? e.message : e));
    return;
  }
  if (typeof refresh === "function") refresh();
}

// Per-filter settings for the "Straight Line Consensus" indicator filter. These
// are the values the engine's Consensus filter rows use (default 0 = flip the
// instant the majority vote flips). Kept separate from the chart indicator's own
// settings; the values live in settings + hidden [data-set] inputs so a generic
// settings save never erases them.
function openConsensusSettings() {
  const s = (STATE && STATE.settings) || {};
  const v = (k, d) => (s[k] == null || s[k] === "" ? d : s[k]);
  let back = document.getElementById("rtScBackdrop");
  if (back) back.remove();
  back = document.createElement("div");
  back.id = "rtScBackdrop";
  back.style.cssText = "position:fixed;inset:0;background:rgba(0,0,0,.55);z-index:100000;display:flex;align-items:center;justify-content:center";
  back.innerHTML =
    '<div style="background:#12122a;border:1px solid #3d3d6b;border-radius:8px;padding:14px 16px;min-width:310px;color:#eaeaf5">' +
    '<div style="font-weight:700;color:#ffd700;margin-bottom:8px">Straight Line Consensus Settings</div>' +
    '<div style="display:grid;grid-template-columns:1fr 100px;gap:6px;align-items:center;font-size:11px">' +
    '<span>Min net votes</span><input type="number" id="scMinAgree" min="0" max="12" step="1" value="' + v("scMinAgree", 0) + '">' +
    '<span>Confirm bars</span><input type="number" id="scConfirm" min="0" max="30" step="1" value="' + v("scConfirm", 0) + '">' +
    '<span>Swing strength</span><input type="number" id="scStrength" min="0" max="50" step="1" value="' + v("scStrength", 0) + '">' +
    '<span>Bull color</span><input type="color" id="scUpColor" value="' + v("scUpColor", "#00e676") + '">' +
    '<span>Bear color</span><input type="color" id="scDownColor" value="' + v("scDownColor", "#ff5252") + '">' +
    '<span>Flat color</span><input type="color" id="scFlatColor" value="' + v("scFlatColor", "#6b6b88") + '">' +
    '<span>Line width</span><input type="number" id="scLineWidth" min="1" max="5" step="1" value="' + v("scLineWidth", 2) + '">' +
    "</div>" +
    '<div style="font-size:9px;color:#888;margin-top:8px">0 = Min votes 0, Confirm 0, Swing strength 0 par filter turant vote flip par chalta hai.</div>' +
    '<div style="display:flex;gap:8px;justify-content:flex-end;margin-top:10px">' +
    '<button class="btn-action" id="scCancel">Cancel</button>' +
    '<button class="btn-action" id="scSave" style="background:#00d4aa;color:#0a0a18;font-weight:700">Save</button>' +
    "</div></div>";
  document.body.appendChild(back);
  const close = () => back.remove();
  back.onclick = (e) => { if (e.target === back) close(); };
  document.getElementById("scCancel").onclick = close;
  document.getElementById("scSave").onclick = () => {
    const g = (id) => document.getElementById(id).value;
    const fields = {
      scMinAgree: num(g("scMinAgree")),
      scConfirm: num(g("scConfirm")),
      scStrength: num(g("scStrength")),
      scUpColor: g("scUpColor"),
      scDownColor: g("scDownColor"),
      scFlatColor: g("scFlatColor"),
      scLineWidth: num(g("scLineWidth")),
    };
    Object.keys(fields).forEach((k) => {
      const hi = document.querySelector('#tab-realtime [data-set="' + k + '"]');
      if (hi) hi.value = fields[k];
    });
    close();
    API.settings(Object.assign({}, (STATE && STATE.settings) || {}, fields)).then(refresh);
  };
}

// Per-filter settings for the Support / Resistance Trendline indicator filters.
// These are the geometry inputs the engine's SlSupport / SlResistance filter rows
// use to fit the line (Pivot strength / ATR period / Min tol % / Tol ATR mult /
// Pivots to scan / Forward bars / Full span). Colours / line width are kept for
// parity with the chart indicator's own settings panel. Values live in settings +
// hidden [data-set] inputs so a generic settings save never erases them.
const PT_DEFAULTS = {
  strength: 5,
  atrPeriod: 14,
  minPct: 0.05,
  tolMult: 0.5,
  look: 12,
  fwd: 10,
  fullSpan: false,
  upColor: "#26a69a",
  downColor: "#ef5350",
  lineWidth: 2,
};

function openPivotTrendSettings(kind) {
  const sup = kind === "support";
  const p = sup ? "sup" : "res";
  const cap = sup ? "Support" : "Resistance";
  const s = (STATE && STATE.settings) || {};
  const key = (f) => p + f.charAt(0).toUpperCase() + f.slice(1);
  const v = (f) => (s[key(f)] == null || s[key(f)] === "" ? PT_DEFAULTS[f] : s[key(f)]);
  const backId = "rtPtBackdrop";
  let back = document.getElementById(backId);
  if (back) back.remove();
  back = document.createElement("div");
  back.id = backId;
  back.style.cssText = "position:fixed;inset:0;background:rgba(0,0,0,.55);z-index:100000;display:flex;align-items:center;justify-content:center";
  back.innerHTML =
    '<div style="background:#12122a;border:1px solid #3d3d6b;border-radius:8px;padding:14px 16px;min-width:310px;color:#eaeaf5">' +
    '<div style="display:flex;align-items:center;justify-content:space-between;margin-bottom:8px"><span style="font-weight:700;color:' + (sup ? "#26a69a" : "#ef5350") + '">' + cap + ' Trendline Settings</span><span id="ptClose" style="cursor:pointer;color:#888;font-size:14px;line-height:1">&#10005;</span></div>' +
    '<div style="display:grid;grid-template-columns:1fr 100px;gap:6px;align-items:center;font-size:11px">' +
    '<span>Pivot strength</span><input type="number" id="ptStrength" min="2" max="50" step="1" value="' + v("strength") + '">' +
    '<span>ATR period</span><input type="number" id="ptAtrPeriod" min="2" max="200" step="1" value="' + v("atrPeriod") + '">' +
    '<span>Min tol %</span><input type="number" id="ptMinPct" min="0" max="5" step="0.05" value="' + v("minPct") + '">' +
    '<span>Tol ATR mult</span><input type="number" id="ptTolMult" min="0" max="10" step="0.1" value="' + v("tolMult") + '">' +
    '<span>Pivots to scan</span><input type="number" id="ptLook" min="3" max="400" step="1" value="' + v("look") + '">' +
    '<span>Forward bars</span><input type="number" id="ptFwd" min="0" max="200" step="1" value="' + v("fwd") + '">' +
    '<span>Full span</span><input type="checkbox" id="ptFullSpan"' + (v("fullSpan") ? " checked" : "") + ">" +
    '<span>Rising color</span><input type="color" id="ptUpColor" value="' + v("upColor") + '">' +
    '<span>Falling color</span><input type="color" id="ptDownColor" value="' + v("downColor") + '">' +
    '<span>Line width</span><input type="number" id="ptLineWidth" min="1" max="5" step="1" value="' + v("lineWidth") + '">' +
    "</div>" +
    '<div style="display:flex;gap:8px;justify-content:flex-end;margin-top:10px">' +
    '<button class="btn-action" id="ptReset">Reset</button>' +
    '<button class="btn-action" id="ptCancel">Cancel</button>' +
    '<button class="btn-action" id="ptSave" style="background:#00d4aa;color:#0a0a18;font-weight:700">Save</button>' +
    "</div></div>";
  document.body.appendChild(back);
  const close = () => back.remove();
  const g = (id) => document.getElementById(id);
  const setFields = (src) => {
    g("ptStrength").value = src.strength;
    g("ptAtrPeriod").value = src.atrPeriod;
    g("ptMinPct").value = src.minPct;
    g("ptTolMult").value = src.tolMult;
    g("ptLook").value = src.look;
    g("ptFwd").value = src.fwd;
    g("ptFullSpan").checked = !!src.fullSpan;
    g("ptUpColor").value = src.upColor;
    g("ptDownColor").value = src.downColor;
    g("ptLineWidth").value = src.lineWidth;
  };
  back.onclick = (e) => { if (e.target === back) close(); };
  g("ptClose").onclick = close;
  g("ptCancel").onclick = close;
  g("ptReset").onclick = () => setFields(PT_DEFAULTS);
  g("ptSave").onclick = () => {
    const fields = {
      [key("strength")]: num(g("ptStrength").value),
      [key("atrPeriod")]: num(g("ptAtrPeriod").value),
      [key("minPct")]: num(g("ptMinPct").value),
      [key("tolMult")]: num(g("ptTolMult").value),
      [key("look")]: num(g("ptLook").value),
      [key("fwd")]: num(g("ptFwd").value),
      [key("fullSpan")]: g("ptFullSpan").checked,
      [key("upColor")]: g("ptUpColor").value,
      [key("downColor")]: g("ptDownColor").value,
      [key("lineWidth")]: num(g("ptLineWidth").value),
    };
    Object.keys(fields).forEach((k) => {
      const hi = document.querySelector('#tab-realtime [data-set="' + k + '"]');
      if (hi) {
        if (hi.type === "checkbox") hi.checked = !!fields[k];
        else hi.value = fields[k];
      }
    });
    close();
    API.settings(Object.assign({}, (STATE && STATE.settings) || {}, fields)).then(refresh);
  };
}

// ---------------------------------------------------------------------------
// Styles
// ---------------------------------------------------------------------------
function style() {
  const css = `
  #tab-realtime { overflow-y: auto; }
  #tab-realtime.active { display: block; }
  #tab-realtime .account-section { padding: 8px; }
  #tab-realtime .rt-title { font-size: 11px; color: #888; text-transform: uppercase; letter-spacing: 1px; margin: 0; }
  #tab-realtime .rt-strip { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; padding: 6px 8px; background: #12122a; border: 1px solid var(--border2, #2d2d50); border-radius: 4px; margin: 0 8px 6px; font-size: 11px; }
  #tab-realtime .rt-pill { font-size: 11px; padding: 3px 9px; border-radius: 10px; background: #22224a; }
  #tab-realtime .rt-pill.on { background: #124a2a; color: #7CFFB2; }
  #tab-realtime .rt-pill.off { background: #4a1212; color: #ff9d9d; }
  #tab-realtime .rt-pill.armed { background: #5a3a00; color: #ffd479; }
  #tab-realtime .account-table { width: 100%; border-collapse: collapse; font-size: 10.5px; }
  #tab-realtime .account-table th { text-align: left; color: #8888b8; font-weight: 500; padding: 4px 5px; border-bottom: 1px solid #2a2a4a; position: sticky; top: 0; background: #111127; }
  #tab-realtime .account-table td { padding: 4px 5px; border-bottom: 1px solid #1c1c38; white-space: nowrap; }
  #tab-realtime .rt-scroll { max-height: 230px; overflow-y: scroll; border: 1px solid var(--border, #1e1e40); border-radius: 3px; scrollbar-width: thin; scrollbar-color: #4a4a80 #12122a; }
  #tab-realtime .rt-scroll.rt-scroll-open { max-height: none; overflow-y: visible; }
  #tab-realtime .rt-scan-fade { display: inline-flex; align-items: center; gap: 10px; flex-wrap: wrap; opacity: .26; transition: opacity .35s ease; pointer-events: none; }
  #tab-realtime .rt-scan-fade.on { opacity: 1; pointer-events: auto; }
  #tab-realtime .rt-scan-fade input:disabled, #tab-realtime .rt-scan-fade select:disabled { opacity: .5; }
  #tab-realtime .rt-pos { color: #7CFFB2; } #tab-realtime .rt-neg { color: #ff8888; }
  #tab-realtime .rtom-chip { user-select: none; }
  #tab-realtime .rtom-chip:hover { border-color: #b39ddb !important; }
  #tab-realtime .rtom-f { display: flex; align-items: center; gap: 5px; font-size: 11px; color: #b8b8c8; }
  #tab-realtime .rtom-f > span:first-child { min-width: 88px; }
  #tab-realtime input:not([type=checkbox]), #tab-realtime select { background: #1a1a35; border: 1px solid #35356a; color: #eaeaf5; border-radius: 3px; padding: 4px 6px; font-size: 12px; }
  #tab-realtime input[type=checkbox] { accent-color: #b39ddb; width: 15px; height: 15px; }
  #tab-realtime button { cursor: pointer; }
  #tab-realtime .rt-engine-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; padding: 6px 8px; border: 1px solid #1e1e40; border-radius: 4px; margin: 4px 0; background: #0d0d1e; }
  #tab-realtime .rt-engine-row b.rt-cap { color: #ffd700; font-size: 10px; white-space: nowrap; }
  #tab-realtime .rt-cond { display: grid; grid-template-columns: 1.4fr .8fr .8fr .6fr auto; gap: 4px; margin-bottom: 4px; }
  #tab-realtime .rt-log { font-family: monospace; font-size: 10px; line-height: 1.45; color: #bfbfd8; padding: 4px 6px; }
  #tab-realtime .rt-badge { font-size: 9px; padding: 1px 5px; border-radius: 8px; background: #333; }
  #rtDialogBackdrop { position: fixed; inset: 0; background: rgba(0,0,0,.6); display: none; align-items: center; justify-content: center; z-index: 9999; }
  #rtDialogBackdrop.show { display: flex; }
  #rtDialog { width: min(430px, 92vw); background: #14142c; border: 1px solid #3a3a60; border-radius: 6px; padding: 14px 16px; box-shadow: 0 8px 40px rgba(0,0,0,.6); }
  #rtDialog .rt-dialog-title { font-size: 12px; font-weight: 700; color: #e6e6ff; margin-bottom: 6px; }
  #rtDialog .rt-dialog-msg { font-size: 12px; color: #c0c0d8; white-space: pre-wrap; line-height: 1.5; }
  #rtDialog .rt-dialog-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 14px; }
  #rtDialog .btn-action { width: auto; padding: 5px 16px; margin: 0; font-size: 11px; }
  #rtDialog #rtDialogOk.danger { background: #5e2d2d; border-color: #7e3d3d; }
  `;
  const e = document.createElement("style");
  e.textContent = css;
  document.head.appendChild(e);
}

// ---------------------------------------------------------------------------
// Shell (mirrors the old app's Realtime Trading Engine layout)
// ---------------------------------------------------------------------------
function shell() {
  const rt = document.getElementById("tab-realtime");
  if (!rt) return;
  rt.className = "tab-content";
  rt.innerHTML = `
    <div class="rt-strip">
      <span class="rt-title">Realtime Trading Engine</span>
      <span class="rt-pill" id="rtConn">checking…</span>
      <span class="rt-pill" id="rtEnginePill">Engine: OFF</span>
      <span class="rt-pill" id="rtArmPill">DISARMED</span>
      <span class="rt-pill" id="rtFundsPill">Funds: --</span>
      <span style="flex:1"></span>
      <button class="btn-action" id="rtRefreshAccount" style="width:auto;padding:3px 10px;margin:0 4px 0 0;font-size:10px">Refresh Account</button>
      <button class="btn-action" id="rtSquareOff" style="width:auto;padding:3px 10px;margin:0;font-size:10px;background:#8a1f1f;border-color:#b03030;font-weight:700;color:#fff">Square Off All Trades</button>
    </div>

    <div class="monitor-toolbar" id="rtAstTplRunSection" style="border:1px solid #4a2d7e;border-radius:4px;margin:4px 0;padding:6px 8px;background:#0d0d1e">
      <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
        <b style="color:#b39ddb;font-size:10px">AST saved templates quick run:</b>
        <select id="rtAstTplRun" style="min-width:250px"><option value="">-- no saved templates --</option></select>
        <button class="btn-action" id="rtAstTplRunBtn" style="width:auto;padding:5px 14px;margin:0;background:#b39ddb;color:#0a0a18;font-weight:700;font-size:10px">Run saved template</button>
        <span style="font-size:8px;color:#666">Dropdown me se saved template chuno aur Run dabao - AI Smart Trading engine khud us template ki saved settings (SL/trail/TP, strikes, filters, symbols) apply karke uske saved mode (strategies / Indicator-filters / AI auto-pick) me trades start kar dega.</span>
      </div>
      <div style="margin-top:4px"><span id="rtAstTplRunStatus" style="font-size:9px;color:#888"></span></div>
    </div>

    <div class="account-section" style="border-top:1px solid #1e1e40">
      <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
        <h3 class="rt-title">Running Strategies &amp; Trades</h3>
        <button class="btn-action" style="width:auto;padding:3px 10px;margin:0;font-size:10px" data-act="runref">Refresh</button>
        <button class="btn-action warn" style="width:auto;padding:3px 10px;margin:0;font-size:10px" data-act="closeall">Square Off All Trades</button>
        <button class="btn-action warn" style="width:auto;padding:3px 10px;margin:0;font-size:10px" data-act="stoplall">Stop All Strategies</button>
      </div>
      <div id="rtMarginBar" style="display:none;align-items:center;gap:12px;flex-wrap:wrap;margin-top:4px;padding:4px 8px;background:#0d0d1e;border:1px solid #1e1e40;border-radius:3px;font-size:11px"></div>
      <div style="display:flex;gap:10px;flex-wrap:wrap;margin-top:4px">
        <div style="flex:1;min-width:280px">
          <div style="font-size:10px;color:#66ccff;font-weight:700;margin:4px 0 2px">Running Strategies / Indicator filter based trades</div>
          <div id="rtRunStrategies" style="max-height:220px;overflow-y:auto"></div>
        </div>
        <div style="flex:1;min-width:280px">
          <div style="font-size:10px;color:#66ccff;font-weight:700;margin:4px 0 2px">Running Trades (Dhan open positions)</div>
          <div id="rtRunTrades" style="max-height:220px;overflow-y:auto"></div>
        </div>
      </div>
    </div>

    <h3 id="rtAccountSection" style="font-size:11px;color:#888;text-transform:uppercase;margin:6px 0 4px">Running Trades <span style="text-transform:none;color:#666;font-weight:normal">(open positions)</span></h3>
    <div class="rt-scroll"><table class="account-table" id="rtPos"><thead><tr>
      <th>Option</th><th>Qty</th><th>Entry</th><th>LTP</th><th>P&amp;L (gross)</th><th>SL/Trail</th><th>Guard</th><th></th>
    </tr></thead><tbody></tbody></table></div>

    <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin:6px 0 4px">
      <h3 style="font-size:11px;color:#888;text-transform:uppercase;margin:0">Closed Trades <span id="rtClosedCount" style="text-transform:none;color:#666;font-weight:normal">(0)</span></h3>
      <button class="btn-action" id="rtClosedExpand" style="width:auto;padding:3px 10px;margin:0;font-size:10px" title="Closed Trades ki poori list ek saath dikhao (scroll box ki 230px limit hatao)">Show all</button>
    </div>
    <div class="rt-scroll" id="rtClosedWrap"><table class="account-table" id="rtClosed"><thead><tr>
      <th>Option</th><th>Qty</th><th>Entry → Exit</th><th>P&amp;L (net)</th><th>Charges</th><th>Reason</th><th>Entry → Exit time</th>
    </tr></thead><tbody></tbody></table></div>

    <div class="account-section" style="border-top:1px solid #1e1e40;margin-top:6px">
      <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap;padding:4px 0">
        <h3 class="rt-title">AI Smart Trading Engine</h3>
        <button class="btn-action" id="rtEngineToggle" style="width:auto;padding:5px 12px;margin:0">AI Smart Trading: OFF</button>
        <button class="btn-action" data-act="ticknow" style="width:auto;padding:5px 12px;margin:0;background:#66ccff;color:#0a0a18">Run / Tick Now</button>
        <span id="rtDisarmWarn" style="display:none;font-size:10px;font-weight:700;color:#ef5350;background:#2a0d0d;border:1px solid #ef5350;border-radius:4px;padding:3px 8px">Engine ON par DISARMED - koi order place nahi hoga. "AI Smart Trading: ON" dabao ya "Run / Tick Now" se arm karo.</span>
        <button class="btn-action" data-act="refresh" style="width:auto;padding:5px 12px;margin:0">Refresh Strategies</button>
        <button class="btn-action warn" data-act="stopall" style="width:auto;padding:5px 12px;margin:0">Stop All</button>
        <button class="btn-action warn" data-act="resetpnl" style="width:auto;padding:5px 12px;margin:0" title="Clear every closed trade, log and realized total. Starts the next trade from a clean ₹0 slate.">Reset P&amp;L</button>
        <button class="btn-action" data-act="selectall" style="width:auto;padding:5px 12px;margin:0">Select All</button>
        <span style="font-size:9px;color:#666">Tick saved strategies to trade them live with the settings below (no backtest)</span>
      </div>

      <div id="rtSummary" style="display:flex;gap:6px;margin-top:8px;flex-wrap:wrap"></div>

      <!-- ORDER PLACEMENT METHOD (old realtimeorders.js block) -->
      <div id="rtOrderMethods" class="account-section" style="border:1px solid #4a2d7e;border-radius:5px;margin:6px 0;padding:8px 10px;background:#0d0d1e">
        <div id="rtOrderMethodsHead" style="display:flex;align-items:center;gap:12px;flex-wrap:wrap">
          <h3 style="font-size:14px;color:#c4a9ff;text-transform:uppercase;margin:0">Order Placement Method</h3>
          <span style="font-size:11px;color:#888">Sirf ek method enable hota hai. Uski saari options niche Risk &amp; Quantity row me hain; engine khud order place karega (koi Place Order button nahi).</span>
          <span style="font-size:12px;color:#bbb;margin-left:auto">Dhan margin available: <b id="rtomMarginAvail" style="color:#ffd700">--</b></span>
        </div>
        <div id="rtomRow" style="display:flex;gap:10px;flex-wrap:wrap;margin-top:8px"></div>
        <div id="rtEngineExtra" style="display:flex;flex-wrap:wrap;align-items:center;gap:10px;margin-top:8px;border-top:1px dashed #4a4a80;padding-top:8px;font-size:12px">
          <span style="color:#c4a9ff;font-weight:700;font-size:12px;white-space:nowrap">Engine controls:</span>
          <span class="rtom-f" title="Live available balance (paper me virtual wallet, real me Dhan funds). Ye read-only hai.">Available balance <b id="rtEngAvailBal" style="color:#00d4aa">--</b></span>
          <label class="rtom-f" title="Available balance ka itna % margin budget ke roop me use hoga (locked across running trades). 0% = margin gate off, 100% = poora balance.">Margin to use <input type="number" data-set="marginPct" id="rtEngMarginPct" min="0" max="100" step="5" style="width:56px"> %</label>
          <label class="rtom-f"><input type="checkbox" data-set="slAuto"> SL auto (ATR-hunting-aware)</label>
          <label class="rtom-f"><input type="checkbox" data-set="tf1min"> 1 min</label>
          <label class="rtom-f"><input type="checkbox" data-set="tf5min"> 5 min</label>
          <label class="rtom-f"><input type="checkbox" data-set="mtf"> Multi-TF confirm</label>
          <label class="rtom-f"><input type="checkbox" data-set="useOwnSettings"> Use AST settings (own SL/trail/timeframe)</label>
          <label class="rtom-f"><input type="checkbox" data-set="aiSl"> AI Stop-Loss (save capital)</label>
          <label class="rtom-f"><input type="checkbox" data-set="aiTpPct"> AI TP % (decide run)</label>
          <label class="rtom-f"><input type="checkbox" data-set="manualTrailTp"> Manual Trail TP <input type="number" data-set="manualTrailTpPct" step="1" style="width:56px"> %</label>
        </div>
      </div>

      <!-- ENGINE RISK & QUANTITY: SL / Trail SL / Lot / Auto-Lot -->
      <div class="rt-engine-row" id="rtEngineRisk" style="border-color:#4a2d7e;border-width:1px">
        <b class="rt-cap" style="color:#c4a9ff">Risk &amp; Quantity:</b>
        <label class="rtom-f" style="color:#ef5350"><input type="checkbox" id="rtEngSl"> SL <input type="number" id="rtEngSlPct" step="0.5" min="0" style="width:56px" placeholder="%"> %</label>
        <label class="rtom-f" style="color:#ffb74d"><input type="checkbox" id="rtEngPointTrailSl"> Trail SL Pts <input type="number" id="rtEngPointTrailSlPts" step="0.05" min="0" style="width:56px" placeholder="pts"></label>
        <label class="rtom-f" style="color:#ffb74d"><input type="checkbox" id="rtEngTrailSl"> Trail SL <input type="number" id="rtEngTrailSlPct" step="0.5" min="0" style="width:56px" placeholder="%"> %</label>
        <label class="rtom-f" title="Lot size (per-lot qty). Khaali chhodne par engine har contract ka actual lot size Dhan scrip-master se auto fetch karta hai.">Lot size <input type="number" id="rtEngLot" step="1" min="0" style="width:64px" placeholder="auto"></label>
        <label class="rtom-f">Lots <input type="number" id="rtEngLots" step="1" min="1" style="width:56px"></label>
        <label class="rtom-f" style="color:#ffd700"><input type="checkbox" id="rtEngAutoLots"> Auto-Lot qty (OI + volume + margin)</label>
        <label class="rtom-f" style="color:#ffd700" title="Auto-Lot volume cap: traded volume ka itna % se zyada lots kabhi nahi">Vol % <input type="number" id="rtEngAutoLotVolPct" data-set="autoLotVolumePct" step="0.5" min="0" style="width:52px" placeholder="1"></label>
        <label class="rtom-f" style="color:#ffd700" title="Auto-Lot OI cap: open interest ka itna % se zyada lots kabhi nahi. Engine dono caps me se jo kam lots dega wahi buy karega.">OI % <input type="number" id="rtEngAutoLotOiPct" data-set="autoLotOiPct" step="0.5" min="0" style="width:52px" placeholder="1"></label>
        <span id="rtEngAutoLotsInfo" style="font-size:9px;color:#666"></span>
        <span style="font-size:9px;color:#666">SL / Trail SL % yahin set karo - AI Smart engine har entry par inhe lagayega, phir khud SL/trail hit par exit karega. Take Profit ke liye AI risk management me RR / AI TP% / Manual Trail TP use karo.</span>
        <span style="flex-basis:100%;height:0"></span>
        <span id="rtEngMethodWrap" style="display:flex;flex-wrap:wrap;align-items:center;gap:10px;flex-basis:100%;border-top:1px dashed #4a4a80;padding-top:7px">
          <span style="font-size:11px;color:#c4a9ff;font-weight:700;white-space:nowrap">Selected method:</span>
          <b id="rtEngMethodName" style="font-size:11px;color:#fff;white-space:nowrap">Normal Order</b>
          <span id="rtEngMethodOpts" style="display:flex;flex-wrap:wrap;align-items:center;gap:10px"></span>
        </span>
        <span style="font-size:9px;color:#666">Ye sab options selected method par auto-apply hote hain; order place karne ki zarurat nahi - condition meet hote hi engine khud place karega. Req: <b id="rtEngReq" style="color:#66ccff">--</b> - Freeze: <b id="rtEngFreeze" style="color:#b39ddb">--</b></span>
      </div>

      <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:8px;border:1px solid #1e1e40;border-radius:3px;padding:4px 8px">
        <label style="font-size:9px;color:#00d4aa;display:flex;align-items:center;gap:3px;cursor:pointer;font-weight:700" title="Live Data Pool: shows the shared realtime candle (open/high/low/close/volume), indicator values and the entry-condition PASS/FAIL for every resolved symbol. The strategies read the exact same pool, so what you see is what they trade.">
          <input type="checkbox" id="rtDataPool" data-set="data_pool" style="accent-color:#00d4aa"> Data Pool (live candle + indicator + filter readout)</label>
        <span id="rtDataPoolInfo" style="font-size:9px;color:#666">OFF - shared pool feeds strategies only</span>
        <button id="rtDataPoolRefresh" style="font-size:9px;color:#fff;background:#2d2d5e;border:1px solid #3d3d7e;border-radius:3px;padding:2px 10px;cursor:pointer;font-weight:700" title="Re-resolve the selected universe and re-render the pool readout now">Refresh</button>
      </div>
      <div id="rtDataPoolBody" style="display:none;margin-top:6px;max-height:240px;overflow-y:scroll;border:1px solid #1e1e40;border-radius:3px;scrollbar-width:thin;scrollbar-color:#4a4a80 #12122a;font-size:10px"></div>

      <div class="rt-engine-row" style="border-color:#2d2d50">
        <b class="rt-cap" style="color:#b39ddb">Template:</b>
        <label class="rtom-f">Name <input type="text" id="rtTplName" placeholder="Template name" style="width:130px"></label>
        <select id="rtTplMode" style="width:110px"><option value="bullish">Bullish</option><option value="bearish">Bearish</option><option value="sideways">Sideways</option></select>
        <button class="btn-action" id="rtTplSave" style="width:auto;padding:3px 10px;margin:0;background:#b39ddb;color:#0a0a18">Save Template</button>
        <span style="font-size:9px;color:#888">Open saved:</span>
        <select id="rtTplOpen" style="min-width:130px"><option value="">-- none --</option></select>
        <button class="btn-action warn" id="rtTplDelete" style="width:auto;padding:3px 10px;margin:0">Delete</button>
        <span id="rtTplInfo" style="font-size:9px;color:#888"></span>
      </div>

      <div class="rt-engine-row" style="border-color:#2d2d50">
        <b class="rt-cap" style="color:#b39ddb">Engine default template:</b>
        <label class="rtom-f" style="color:#00d4aa">Bullish default <select data-set="bullTemplate" id="rtTplBull" style="min-width:140px"></select></label>
        <label class="rtom-f" style="color:#ef5350">Bearish default <select data-set="bearTemplate" id="rtTplBear" style="min-width:140px"></select></label>
        <span style="font-size:9px;color:#666">Applied to a run when the active direction has no assigned AST template below.</span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">AI risk management:</b>
        <label class="rtom-f"><input type="checkbox" data-set="rrEnabled"> Set TP by Risk:Reward <input type="number" data-set="rrValue" step="0.1" style="width:56px"></label>
        <span id="rtRrStatus" style="font-size:9px;color:#00d4aa;font-weight:700"></span>
        <span style="font-size:9px;color:#666">Overall SL / Trail SL "Risk &amp; Quantity" row me hain (upar), AI Stop-Loss / AI TP% / Manual Trail TP "Engine controls" me. Take Profit ya to Risk:Reward ya AI TP% se lagta hai.</span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Trades per strategy:</b>
        <label class="rtom-f"><input type="checkbox" data-set="tradeLimit"> Max trades</label>
        <input type="number" data-set="tradeLimitCount" step="1" style="width:56px">
        <label class="rtom-f"><input type="checkbox" data-set="aiTrades"> AI auto trades</label>
        <span id="rtTradesStatus" style="font-size:9px;color:#666;font-weight:700"></span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Order rate:</b>
        <label class="rtom-f" title="Ek second me maximum itne hi order bhejega (Dhan API ~6/sec allow karta hai). Har entry is cap ke andar hi fire hogi.">Order per Second <input type="number" data-set="orderPerSec" min="1" max="30" step="1" style="width:64px"></label>
        <span id="rtOpsStatus" style="font-size:9px;color:#666;font-weight:700"></span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Engine scan:</b>
        <label class="rtom-f" title="ON: engine apni entry conditions ko throttled cadence par scan karega (niche set kiya gaya). OFF: normal ultrafast ~100ms tick. Exits (SL/Trail/TP) hamesha fast rehte hain.">
          <input type="checkbox" id="rtEngineScanCb" data-set="scanIntervalOn" style="accent-color:#00d4aa"> Engine Scan
        </label>
        <span id="rtScanIntervalBox" class="rt-scan-fade" title="Engine ko kitne interval par scan karna hai. Minutes + Seconds + Milliseconds milakar ek total time banta hai (minimum 50ms).">
          <label class="rtom-f">Minutes <input type="number" id="rtScanMin" min="0" max="1440" step="1" style="width:70px" placeholder="0"></label>
          <label class="rtom-f">Seconds <input type="number" id="rtScanSec" min="0" max="59" step="1" style="width:64px" placeholder="0"></label>
          <label class="rtom-f">Milliseconds <input type="number" id="rtScanMs" min="0" max="999" step="10" style="width:82px" placeholder="100"></label>
        </span>
        <span id="rtScanBarCloseChk" class="rt-scan-fade" title="ON: fixed interval ke bajaye har bar close par (nayi candle shuru hote hi) engine scan karega.">
          <label class="rtom-f"><input type="checkbox" id="rtScanBarCloseCb" data-set="scanBarClose" style="accent-color:#ffd700"> Countdown to Bar Close</label>
        </span>
        <span id="rtScanBarCloseBox" class="rt-scan-fade" title="Jis timeframe ka bar close hone par scan karna hai.">
          <select id="rtScanBarCloseTf" data-set="scanBarCloseTf" style="min-width:210px">
            <option value="1min">1 minute countdown to bar close</option>
            <option value="5min">5 minute countdown to bar close</option>
          </select>
        </span>
        <input type="number" id="rtScanTotalMs" data-set="scanIntervalMs" min="50" max="86400000" step="10" style="display:none">
        <span id="rtScanStatus" style="font-size:9px;color:#666;font-weight:700"></span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Trade times:</b>
        <label class="rtom-f"><input type="checkbox" data-set="startAfterEnabled"> Start trading after</label>
        <label class="rtom-f"><input type="time" data-set="startAfter" style="width:90px"></label>
        <label class="rtom-f"><input type="checkbox" data-set="noTradeAfterEnabled"> No trade after</label>
        <label class="rtom-f"><input type="time" data-set="noTradeAfter" style="width:90px"></label>
        <label class="rtom-f"><input type="checkbox" data-set="autoSquareOffEnabled"> Auto square off at</label>
        <label class="rtom-f"><input type="time" data-set="autoSquareOffTime" style="width:90px"></label>
        <span id="rtTimeStatus" style="font-size:9px;color:#666;font-weight:700"></span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Trading sessions:</b>
        <span style="font-size:8px;color:#666">Add multiple intraday windows (IST). Entries sirf enabled session ke andar fire honge; exits kabhi gated nahi.</span>
        <label class="rtom-f">Start <input type="text" id="rtSessionStart" value="09:30 AM" list="rtSessTimes" style="width:96px" autocomplete="off"></label>
        <label class="rtom-f">End <input type="text" id="rtSessionEnd" value="10:30 AM" list="rtSessTimes" style="width:96px" autocomplete="off"></label>
        <datalist id="rtSessTimes">
          <option value="09:15 AM"></option><option value="09:30 AM"></option><option value="10:00 AM"></option>
          <option value="10:30 AM"></option><option value="11:00 AM"></option><option value="12:00 PM"></option>
          <option value="01:00 PM"></option><option value="01:30 PM"></option><option value="02:00 PM"></option>
          <option value="02:30 PM"></option><option value="03:00 PM"></option><option value="03:14 PM"></option>
          <option value="03:15 PM"></option><option value="03:30 PM"></option>
        </datalist>
        <button type="button" class="btn-action" id="rtSessionAdd" style="width:auto;padding:4px 12px;margin:0 4px;font-size:10px">+ Add session</button>
        <span id="rtSessionStatus" style="font-size:9px;color:#666;font-weight:700"></span>
        <div id="rtTradeSessionsList" style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:6px;align-items:center;margin-top:3px"></div>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">NIFTY:</b>
        <span id="rtNiftyStatus" style="font-size:11px;color:#ffd700"></span>
        <span style="font-size:9px;color:#666">Straight-line confirm indicators drive the ensemble trend.</span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Strike:</b>
        <label class="rtom-f">Option Type
          <select data-set="optionSide" style="width:120px"><option value="both">Both CE &amp; PE</option><option value="CE">Call (CE)</option><option value="PE">Put (PE)</option></select></label>
        <label class="rtom-f">Execute Trade In
          <select data-set="strikeMode" style="width:190px">
            <option value="above">Above ATM</option>
            <option value="below">Below ATM</option>
            <option value="both_atm">Above and below ATM</option>
            <option value="above_atm">Above including ATM</option>
            <option value="below_atm">Below including ATM</option>
            <option value="both_atm_inc">Above and below including ATM</option>
            <option value="atm">Only ATM</option>
          </select></label>
        <label class="rtom-f">Number of Strikes <input type="number" data-set="strikeCount" min="0" step="1" style="width:56px"></label>
        <label class="rtom-f"><input type="checkbox" data-set="onlyPositive"> Only +green premium strikes</label>
        <label class="rtom-f" title="Only +green ke sath: agar Execute Trade In ki chuni hui side (Above/Below ATM) ki saari strikes minus ho, to ATM ke opposite side (Below/Above) me plus-sign wali strike dhoondh kar usi leg (CE/PE) me trade karta hai. Single-side dropdown par hi lagu."><input type="checkbox" data-set="fallbackOppositeSide" style="accent-color:#00d4aa"> Opposite side fallback (if all minus)</label>
        <label class="rtom-f" title="ON hone par Execute Trade In ki chuni hui side ke plus-sign (rising) premiums me se sabse jyada LTP change % wala strike pick hota hai (tie: jyada LTP change). +green aur Opposite side fallback ke sath mil kar chalta hai. OFF par +green ka sabse kareeb wala rising strike."><input type="checkbox" data-set="fastestRising" style="accent-color:#00d4aa"> Pick fastest positive rising LTP</label>
        <label class="rtom-f">Fastest-Rising Strikes <input type="number" data-set="fastestCount" min="1" step="1" style="width:56px"></label>
      </div>

      <div class="rt-engine-row" id="rtManualStrikeRow" style="align-items:flex-start;border-color:#4a3a0a">
        <label class="rtom-f" style="color:#ffd700;font-weight:bold" title="Testing only: ON hone par engine ATM / Execute-Trade-In / Number of Strikes / +green / fastest-rising ki jagah sirf neeche add kiye strikes par option leg resolve karega.">
          <input type="checkbox" data-set="manualStrikesEnabled" id="rtManualStrikeCb"> Manual Strike Select (Testing only)
        </label>
        <span id="rtManualStrikeHint" style="font-size:8px;color:#888">ON karo, phir instrument + strike pick karke Add dabao. Ye real market me order place + trail SL test karne ke liye hai.</span>
        <div id="rtManualStrikeBox" style="flex-basis:100%;display:none;flex-wrap:wrap;gap:8px;align-items:center;margin-top:4px">
          <label class="rtom-f">Instrument <select id="rtManualInstrument" style="min-width:280px"><option value="">-- loading --</option></select></label>
          <label class="rtom-f">Expiry <select id="rtManualExpiry" style="width:130px"></select></label>
          <label class="rtom-f">Strike <select id="rtManualStrikePick" style="min-width:230px"></select></label>
          <label class="rtom-f">Side <select id="rtManualSide" style="width:130px"><option value="both">Both CE &amp; PE</option><option value="CE">CE only</option><option value="PE">PE only</option></select></label>
          <button class="btn-action" id="rtManualStrikeAdd" style="width:auto;padding:3px 12px;margin:0">+ Add Strike</button>
          <span id="rtManualStrikeStatus" style="font-size:9px;color:#888"></span>
          <div id="rtManualStrikeList" style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:6px;align-items:center;font-size:9px;color:#ccc;margin-top:2px"></div>
        </div>
      </div>

      <div class="rt-engine-row">
        <label class="rtom-f" style="color:#ffd700;font-weight:bold"><input type="checkbox" data-set="premiumOnly"> Premium chart only (run + trade)</label>
        <span style="font-size:9px;color:#888">Strategies run AND trades execute on the option premium chart for all instruments.</span>
      </div>

      <div class="rt-engine-row" id="rtRunInRow">
        <b class="rt-cap">Strategy should be run in:</b>
        <label class="rtom-f">Indices <select data-set="runIndex" style="width:120px"><option value="spot">Spot chart</option><option value="premium">Option premium</option><option value="both">Both</option></select></label>
        <label class="rtom-f">F&amp;O stocks <select data-set="runFno" style="width:120px"><option value="spot">Spot chart</option><option value="premium">Option premium</option><option value="both">Both</option></select></label>
        <label class="rtom-f">Commodities <select data-set="runComm" style="width:130px"><option value="spot">Spot chart</option><option value="futures">Futures</option><option value="premium">Option premium</option><option value="both">Both</option></select></label>
        <label class="rtom-f"><input type="checkbox" data-set="runInDefault"> Make this default setting</label>
      </div>

      <div class="rt-engine-row" id="rtTradeInRow">
        <b class="rt-cap">Trade should be executed in:</b>
        <label class="rtom-f">Indices <select data-set="tradeInIndex" style="width:160px"><option value="spot">Spot chart</option><option value="premium">Selected strike option premium chart</option><option value="both">Both</option></select></label>
        <label class="rtom-f">F&amp;O stocks <select data-set="tradeInFno" style="width:180px" title="F&amp;O trades are always executed on the selected-strike option premium chart (engine-enforced)."><option value="premium">Selected strike option premium chart</option></select></label>
        <label class="rtom-f">Commodities <select data-set="tradeInComm" style="width:160px"><option value="spot">Futures contract</option><option value="premium">Option premium chart</option></select></label>
        <label class="rtom-f"><input type="checkbox" data-set="tradeInDefault"> Make this default setting</label>
        <span id="rtRoutingStatus" style="font-size:9px;color:#666;font-weight:700;flex-basis:100%"></span>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Top Gainers / Losers + Indices:</b>
        <button class="btn-action" data-toggle="moversOn" id="rtMoversToggle" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Top Movers: OFF</button>
        <label class="rtom-f">Top gainers <input type="number" data-set="moversGainers" min="0" step="1" style="width:56px"></label>
        <label class="rtom-f">Top losers <input type="number" data-set="moversLosers" min="0" step="1" style="width:56px"></label>
        <label class="rtom-f">Indices (set CE/PE leg)
          <select id="rtMoversIndicesSelect" style="min-width:150px"><option value="">-- pick index --</option></select>
        </label>
        <button class="btn-action" id="rtMoversIndicesAdd" style="width:auto;padding:3px 10px;margin:0">Add more</button>
        <span id="rtMoversIndicesList" style="font-size:9px;color:#ccc;display:flex;flex-wrap:wrap;gap:4px;align-items:center"></span>
        <span style="font-size:8px;color:#666;flex-basis:100%">Each index trades only its selected CE/PE leg. No leg = skipped (index trading needs an explicit trend call).</span>
        <input type="hidden" data-list="moversIndices">
      </div>
      <div id="rtMoversList" style="display:none;margin-top:4px;font-size:9px;color:#ccc;background:#12122a;border:1px solid #2d2d50;border-radius:4px;padding:6px 8px"></div>

      <div class="rt-engine-row">
        <b class="rt-cap" style="color:#00d4aa">NIFTY Trend Following:</b>
        <button class="btn-action" data-toggle="niftyTrendOn" id="rtNiftyTrendToggle" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Trend Follow: OFF</button>
        <label class="rtom-f">Confirm indicators
          <select id="rtNiftyTrendConfIndSelect" style="min-width:160px"><option value="">-- pick indicator --</option></select>
        </label>
        <button class="btn-action" id="rtNiftyTrendConfIndAdd" style="width:auto;padding:3px 10px;margin:0">Add</button>
        <span id="rtNiftyTrendConfIndList" style="font-size:9px;color:#ccc;display:flex;flex-wrap:wrap;gap:4px;align-items:center"></span>
        <input type="hidden" data-list="niftyTrendConfInds">
      </div>
      <div id="rtNiftyTrendList" style="display:none;margin-top:4px;font-size:9px;color:#ccc;background:#12122a;border:1px solid #2d2d50;border-radius:4px;padding:6px 8px"></div>

      <div class="rt-engine-row" id="rtMoversTplRow" style="border-color:#4a3a0a;align-items:flex-start">
        <b class="rt-cap" style="color:#b39ddb">Assign AST Template to direction:</b>
        <div style="flex-basis:100%;display:flex;align-items:center;gap:6px;flex-wrap:wrap">
          <label class="rtom-f" style="color:#00d4aa">Top gainers (bullish) &rarr; template
            <select id="rtTplMoverBull" style="min-width:190px"></select>
          </label>
          <button class="btn-action" id="rtAssignMoverBull" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Assign</button>
          <span id="rtMoverBullTplList" style="font-size:9px;color:#ccc;display:inline-flex;flex-wrap:wrap;gap:4px;align-items:center"></span>
        </div>
        <div style="flex-basis:100%;display:flex;align-items:center;gap:6px;flex-wrap:wrap">
          <label class="rtom-f" style="color:#ef5350">Top losers (bearish) &rarr; template
            <select id="rtTplMoverBear" style="min-width:190px"></select>
          </label>
          <button class="btn-action" id="rtAssignMoverBear" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Assign</button>
          <span id="rtMoverBearTplList" style="font-size:9px;color:#ccc;display:inline-flex;flex-wrap:wrap;gap:4px;align-items:center"></span>
        </div>
      </div>

      <div class="rt-engine-row">
        <b class="rt-cap">Commodities (MCX):</b>
        <button class="btn-action" data-toggle="commodityOn" id="rtCommodityToggle" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Commodities: OFF</button>
        <label class="rtom-f">Add <select id="rtCommodityAdd" style="min-width:150px"><option value="">-- pick commodity --</option></select></label>
        <button class="btn-action" id="rtCommodityAddBtn" style="width:auto;padding:3px 10px;margin:0">+ Add</button>
        <button class="btn-action warn" id="rtCommodityClearBtn" style="width:auto;padding:3px 8px;margin:0">Clear</button>
        <span id="rtCommodityStatus" style="font-size:9px;color:#888">Add MCX commodity futures to trade them directly.</span>
        <div id="rtCommodityChips" style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:4px;align-items:center;font-size:9px;color:#ccc"></div>
        <input type="hidden" data-list="commodityList">
      </div>

      <div class="rt-engine-row" id="rtScannerRemovedRow" style="display:none">
        <b class="rt-cap" style="color:#ef5350">Removed picks:</b>
        <span style="font-size:8px;color:#666">Stocks removed from Top Movers / Top Losers / NIFTY Trend - the engine will not pick or trade them until restored.</span>
        <div id="rtScannerRemovedList" style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:4px;align-items:center;font-size:9px;color:#ccc;margin-top:3px"></div>
        <input type="hidden" data-list="scannerExclude" id="rtScannerExcludeList">
      </div>

      <div class="rt-engine-row" id="rtStrikesSection">
        <b class="rt-cap">Picked Strikes:</b>
        <span id="rtStrikesNote" style="font-size:8px;color:#666">Option strikes the engine picked to execute (auto-shown while Top Movers / NIFTY Trend Follow is ON)</span>
      </div>
      <div id="rtStrikesList" style="display:none;margin-top:2px;font-size:9px;color:#ccc;background:#0e1626;border:1px solid #2d2d50;border-radius:4px;padding:6px 8px"></div>

      <div class="rt-engine-row" id="rtFilterRow" style="align-items:flex-start">
        <b class="rt-cap">Indicator filters:</b>
        <div style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:10px;align-items:center">
          <label class="rtom-f"><input type="checkbox" data-set="allInOne"> All together (strict AND)</label>
          <label class="rtom-f"><input type="checkbox" data-set="dirGuard"> Direction Guard (no trade on opposite)</label>
          <label class="rtom-f"><input type="checkbox" data-set="overallDir"> Overall Bullish/Bearish idea</label>
          <label class="rtom-f" style="color:#00d4aa" title="Filter-side routing: ON karne par har trade ka CE/PE leg us strategy ke apne indicator filter se decide hoga. Bullish filter detect hone par CE leg execute hogi, bearish detect hone par trade nahi hogi; bearish filter ke liye isi tarah PE leg. NIFTY lock / Top Movers auto side / Run-Strategy-In override / Overall direction koi bhi is leg ko nahi badal sakta. OFF = purana normal routing."><input type="checkbox" data-set="filterSideRoute" id="rtFilterSideRouteCb" style="accent-color:#00d4aa"> Filter side routing (Bullish&rarr;CE, Bearish&rarr;PE)</label>
          <label class="rtom-f">AI Brain
            <select data-set="brainMode" style="width:170px"><option value="off">OFF</option><option value="auto">Auto (score + conflict veto)</option></select></label>
          <label class="rtom-f">threshold <input type="number" data-set="brainThreshold" min="5" max="100" step="1" style="width:56px"> %</label>
          <label class="rtom-f" title="Opposite-side veto: Relative (default) = entry sirf tab veto hoti hai jab OPPOSITE side ke filters apni side se strictly zyada agree karein (kam se kam 1 filter aage). Barabar (tie) ya apni side aage ho to veto NAHI lagega, isliye symmetric Bull+Bear filter set dono side par deadlock nahi karega. OFF = koi opposite veto nahi, sirf apni side ka majority/strict/Brain rule.">Opposite veto
            <select data-set="oppositeVeto" style="width:180px"><option value="relative">Relative (stronger side wins)</option><option value="off">OFF</option></select></label>
        </div>
        <div id="rtBrainSummary" style="flex-basis:100%;font-size:9px;color:#b39ddb"></div>
        <div style="flex-basis:100%;display:flex;flex-wrap:wrap;gap:10px;align-items:flex-start">
          ${filterSectionHTML("Bull")}
          <div style="display:flex;flex-direction:column;justify-content:center;align-items:center;gap:4px;padding-top:4px">
            <button type="button" id="rtMirrorFilters" title="Mirror opposite: copy the currently-selected side's filters to the other side" style="background:#1a2332;color:#ffd700;border:1px solid #3d3d6b;border-radius:4px;font-size:11px;font-weight:700;padding:8px 6px;cursor:pointer">&#8646;<span style="display:block;font-size:8px;font-weight:400;color:#aaa;margin-top:2px">Mirror</span></button>
          </div>
          ${filterSectionHTML("Bear")}
        </div>
        <div style="display:none">
          <input type="number" data-set="scMinAgree"><input type="number" data-set="scConfirm"><input type="number" data-set="scStrength">
          <input type="text" data-set="scUpColor"><input type="text" data-set="scDownColor"><input type="text" data-set="scFlatColor"><input type="number" data-set="scLineWidth">
          <input type="number" data-set="supStrength"><input type="number" data-set="supAtrPeriod"><input type="number" data-set="supMinPct"><input type="number" data-set="supTolMult"><input type="number" data-set="supLook"><input type="number" data-set="supFwd"><input type="checkbox" data-set="supFullSpan"><input type="text" data-set="supUpColor"><input type="text" data-set="supDownColor"><input type="number" data-set="supLineWidth">
          <input type="number" data-set="resStrength"><input type="number" data-set="resAtrPeriod"><input type="number" data-set="resMinPct"><input type="number" data-set="resTolMult"><input type="number" data-set="resLook"><input type="number" data-set="resFwd"><input type="checkbox" data-set="resFullSpan"><input type="text" data-set="resUpColor"><input type="text" data-set="resDownColor"><input type="number" data-set="resLineWidth">
        </div>
      </div>

      <div class="rt-engine-row" id="rtMultiPosRow" style="border-color:#ffa726">
        <b class="rt-cap" style="color:#ffa726">Multi Position:</b>
        <label class="rtom-f" style="color:#00d4aa" title="Nayi position sirf fresh signal par: entry gate false se true hote hi ek position. Signal lagataar true rahe to sirf ek; reset hone par agli fresh trigger par nayi position. Concurrent positions unlimited."><input type="checkbox" data-set="multiFreshOn" id="rtMultiFreshCb"> Fresh signal par position</label>
        <label class="rtom-f" style="color:#ef5350" title="Condition true rahne tak har scan cycle (~100ms) par nayi position khulti rahegi. Unlimited concurrent positions; orders/sec cap fir bhi lagu rehta hai."><input type="checkbox" data-set="multiAlwaysOn" id="rtMultiAlwaysCb"> Condition true rahne tak baar-baar position</label>
        <span style="font-size:8px;color:#888;flex-basis:100%">Dono OFF = purana rule (ek strategy me ek hi open position). Jo mode active hoga, algo usi ke hisaab se trade karega.</span>
      </div>

      <div class="rt-engine-row" id="rtRunInRow2" style="border-top:1px dashed #1e1e40">
        <label class="rtom-f" style="color:#ffd700" title="Run every selected strategy on the option premium chart of only one side (CE or PE)."><input type="checkbox" data-set="runInEnabled" id="rtRunInEnabled"> Run Strategy In</label>
        <select data-set="runInSide" id="rtRunInSide" style="width:58px"><option value="CE">CE</option><option value="PE">PE</option></select>
        <label class="rtom-f" style="color:#66ccff" title="Auto pick CE/PE from NIFTY trend / top gainer-loser."><input type="checkbox" data-set="runInAuto" id="rtRunInAutoCb"> Auto Select Mode</label>
        <span id="rtRunInStatus" style="font-size:8px;color:#888"></span>
      </div>

      <div class="rt-engine-row" id="rtRunModeRow">
        <label class="rtom-f" style="color:#00d4aa" title="Normal Run Paper Trading mode. When ON the Indicator-filters button is faded/inactive.">
          <input type="checkbox" id="rtRunPaperModeCb" checked> Normal mode
        </label>
        <button class="btn-action" id="rtRunPaperBtn" style="width:auto;padding:6px 16px;margin:0;background:#00d4aa;color:#0a0a18;font-weight:700">Run Paper Trading</button>
        <label class="rtom-f" style="color:#b39ddb" title="Indicator-filters trading mode. When ON the normal Run Paper Trading button is faded/inactive.">
          <input type="checkbox" id="rtFilterModeCb"> Indicator-filters mode
        </label>
        <button class="btn-action" id="rtFilterPaperBtn" style="width:auto;padding:6px 16px;margin:0;background:#b39ddb;color:#0a0a18;font-weight:700">Place trades based on Indicator filters</button>
        <span style="font-size:9px;color:#888;flex-basis:100%">Run the ticked strategies - normal mode runs the ticked strategies, Indicator-filters mode trades the scanner universe (Top Movers / NIFTY trend / Commodities) when the Indicator filters agree by majority; tick "All together (strict AND)" to require ALL selected filters, or enable AI Brain (score + conflict veto) for a confluence threshold. Paper trades are simulated (no real Dhan orders).</span>
      </div>

      <div class="account-section" id="rtCondLogSection" style="overflow-y:auto;border-top:1px solid #ffd700;margin-top:8px;padding-top:6px">
        <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;padding:2px 0">
          <h3 style="font-size:11px;color:#ffd700;text-transform:uppercase;margin:0">Condition Log</h3>
          <span style="font-size:9px;color:#888">Engine skip/condition events - no option contracts, no tradeable instruments, filter gates and order errors. Newest first.</span>
          <span id="rtCondLogCount" style="font-size:8px;color:#888;margin-left:auto">0 line(s)</span>
          <label style="font-size:9px;color:#888;display:flex;align-items:center;gap:3px">Level
            <select id="rtCondLogLevel" style="width:auto"><option value="all">All</option><option value="warn">Warnings + errors</option><option value="error">Errors only</option></select>
          </label>
          <label style="font-size:9px;color:#888;display:flex;align-items:center;gap:3px"><input type="checkbox" id="rtCondLogPause"> Pause</label>
          <button class="btn-action" id="rtCondLogRefresh" style="width:auto;padding:1px 8px;margin:0;font-size:8px">Refresh</button>
          <button class="btn-action" id="rtCondLogClear" style="width:auto;padding:1px 8px;margin:0;font-size:8px">Clear</button>
        </div>
        <div id="rtCondLogBody" style="max-height:190px;overflow-y:auto;font-size:9px;line-height:1.5;background:#0a0a18;border:1px solid #1e1e40;border-radius:4px;padding:4px 6px;color:#aaa"></div>
      </div>

      <div id="rtEtPanel" style="max-height:170px;overflow-y:auto;font-size:9px;border-top:1px solid #1e1e40;margin-top:6px;padding-top:4px">
        <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap">
          <b style="color:#66ccff;font-size:10px">ENTRY TIMING DIAGNOSTICS</b>
          <span style="font-size:8px;color:#666">when the full Bullish/Bearish filter set first held vs when the entry actually placed - delay = late-entry lag. One row per alignment episode.</span>
          <span id="rtEtCount" style="font-size:8px;color:#888;margin-left:auto">0 event(s)</span>
          <button class="btn-action" id="rtEtClear" style="width:auto;padding:1px 8px;margin:0;font-size:8px">Clear</button>
        </div>
        <div id="rtEtBody" style="padding:2px 0;color:#888;line-height:1.5"></div>
      </div>

      <h3 style="font-size:11px;color:#888;text-transform:uppercase;margin:8px 0 4px">Holdings <span style="text-transform:none;color:#666;font-weight:normal">(Dhan demat, T+1 delivery)</span></h3>
      <div class="rt-scroll"><table class="account-table" id="rtHoldings"><thead><tr>
        <th>Symbol</th><th>Qty</th><th>Avg</th><th>LTP</th><th>P&amp;L</th>
      </tr></thead><tbody></tbody></table></div>
    </div>`;

  // Paper trading never places Dhan orders, so the Dhan Order Placement Method
  // cards (Normal / Super / Forever / Slice) are dead UI here. Hide the whole
  // method picker and keep only the universal engine controls + Risk & Quantity
  // row, which drive the simulated engine.
  if (PAPER) {
    const om = rt.querySelector("#rtOrderMethods");
    const head = rt.querySelector("#rtOrderMethodsHead");
    const cards = rt.querySelector("#rtomRow");
    const methodWrap = rt.querySelector("#rtEngMethodWrap");
    if (head) head.style.display = "none";
    if (cards) cards.style.display = "none";
    if (methodWrap) methodWrap.style.display = "none";
    if (om) {
      om.style.border = "none";
      om.style.background = "transparent";
      om.style.padding = "0";
      om.style.margin = "0";
    }
  }

  buildMethodCards();
  wire();
}

// ---------------------------------------------------------------------------
// Order Placement Method cards
// ---------------------------------------------------------------------------
function methodMeta(key) {
  return METHODS.find((m) => m.key === key) || METHODS[0];
}

// One option control for the active method's `order_cfg` (rendered inside the
// engine Risk & Quantity row, never duplicated on the method card).
function optField(x) {
  const f = `data-mfield="${x.k}"`;
  if (x.type === "select") {
    return `<label class="rtom-f"><span>${esc(x.label)}</span>
      <select class="rtEng-opt" ${f}>${x.opts.map((o) => `<option value="${o[0]}">${o[1]}</option>`).join("")}</select></label>`;
  }
  if (x.type === "check") {
    return `<label class="rtom-f" style="color:#e0e0e0"><input type="checkbox" class="rtEng-opt" ${f}> ${esc(x.label)}
      <span style="font-size:10px;color:#888">(super ko iceberg jaisa; off = slice band)</span></label>`;
  }
  return `<label class="rtom-f"><span>${esc(x.label)}</span>
    <input type="number" class="rtEng-opt" ${f} min="0" step="0.05" style="width:78px"></label>`;
}

// Method cards are NAME + enable checkbox only. Exactly one method is enabled
// (radio-like); every order option lives in the engine's single Risk & Quantity
// row and is written to the enabled method's `order_cfg`, which `open_entry` reads.
function methodChipHTML(m) {
  const isSel = m.key === activeMethod;
  return `<label class="rtom-chip${isSel ? " sel" : ""}" data-key="${m.key}" title="${esc(m.desc)} (${esc(m.sdk)})"
    style="display:flex;align-items:center;gap:7px;font-size:13px;font-weight:700;cursor:pointer;
    border-radius:5px;padding:6px 12px">
    <input type="checkbox" class="rtom-enable" data-key="${m.key}"${isSel ? " checked" : ""} style="accent-color:#b39ddb;width:16px;height:16px"> ${esc(m.name)}
    <span style="font-size:10px;color:#777;font-weight:400">${esc(m.sdk)}</span></label>`;
}

function buildMethodCards() {
  const row = document.getElementById("rtomRow");
  if (row) {
    row.innerHTML = METHODS.map(methodChipHTML).join("");
    row.querySelectorAll(".rtom-enable").forEach((cb) => {
      cb.addEventListener("change", () => {
        const k = cb.getAttribute("data-key");
        if (k) {
          activeMethod = k;
          API.method({ method: k });
        }
        buildMethodCards();
      });
    });
  }
  renderMethodOpts();
  applyChartPrice();
  syncEngineRisk();
  syncInterlocks();
}

// Render the active method's own options (order type, GTT leg, slice qty ...)
// into the one Risk & Quantity row.
function renderMethodOpts() {
  const box = document.getElementById("rtEngMethodOpts");
  if (!box) return;
  const m = methodMeta(activeMethod);
  const nameEl = document.getElementById("rtEngMethodName");
  if (nameEl) nameEl.textContent = m.name;
  box.innerHTML = m.extras.map(optField).join("");
  box.querySelectorAll(".rtEng-opt").forEach((e) => {
    e.addEventListener("input", () => onMethodOptInput(e));
    e.addEventListener("change", () => onMethodOptInput(e));
  });
  box.querySelectorAll(".rtEng-price").forEach((p) => p.addEventListener("input", () => onMethodOptInput(p)));
  syncMethodOpts();
}

// Auto-slice (Super) gates the slice-qty box, exactly like the old card.
function applySliceFade() {
  const box = document.getElementById("rtEngMethodOpts");
  if (!box) return;
  const chk = box.querySelector('[data-mfield="autoSlice"]');
  const qty = box.querySelector('[data-mfield="sliceQty"]');
  if (!qty) return;
  const on = !!(chk && chk.checked);
  qty.disabled = !on;
  const wrap = qty.closest("label");
  if (wrap) wrap.style.opacity = on ? "1" : ".35";
}

// order_cfg of the active method -> the Risk & Quantity method-option controls.
function syncMethodOpts() {
  const box = document.getElementById("rtEngMethodOpts");
  if (!box) return;
  const cfg = currentMethodCfg(activeMethod);
  const ae = document.activeElement;
  const p = box.querySelector(".rtEng-price");
  if (p && ae !== p && !num(p.value) && num(cfg.price) > 0) {
    p.value = cfg.price;
    priceAuto[activeMethod] = false;
  }
  box.querySelectorAll(".rtEng-opt").forEach((e) => {
    if (e === ae) return;
    const f = e.getAttribute("data-mfield");
    const v = cfg[f];
    if (v == null) return;
    if (e.type === "checkbox") e.checked = isTruthy(v);
    else e.value = v;
  });
  applySliceFade();
}

// Risk & Quantity method-option control -> active method's order_cfg.
function onMethodOptInput(e) {
  const key = activeMethod;
  const cfg = Object.assign({}, currentMethodCfg(key));
  if (e.classList.contains("rtEng-price")) {
    priceAuto[key] = false;
    cfg.price = num(e.value);
    updateMargin();
    API.method({ method: key, cfg }).then(refresh);
    return;
  }
  const f = e.getAttribute("data-mfield");
  if (!f) return;
  cfg[f] = e.type === "checkbox" ? !!e.checked : e.value;
  if (f === "autoSlice") applySliceFade();
  updateMargin();
  API.method({ method: key, cfg }).then(refresh);
}

// The active method's option controls live in the Risk & Quantity row, so a
// snapshot refresh just re-syncs them (+ chart price + engine risk).
function syncCards() {
  syncMethodOpts();
  applyChartPrice();
  syncEngineRisk();
}

// Engine-level Risk & Quantity row (SL / Trail SL / Lot size /
// Lots / Auto-Lot). Single source of truth is `settings` (+ top-level `autoLots`),
// exactly the fields `open_entry` reads, so what the operator sets here is what
// every engine entry gets.
function syncEngineRisk() {
  const u = (STATE && STATE.settings) || {};
  const ae = document.activeElement;
  const setCb = (id, v) => { const e = document.getElementById(id); if (e && ae !== e) e.checked = !!v; };
  const setNum = (id, v) => { const e = document.getElementById(id); if (e && ae !== e) e.value = num(v) > 0 ? v : ""; };
  setCb("rtEngSl", u.manualSl);
  setNum("rtEngSlPct", u.manualSlPct);
  setCb("rtEngTrailSl", u.manualTrailSl);
  setNum("rtEngTrailSlPct", u.manualTrailSlPct);
  setCb("rtEngPointTrailSl", u.manualPointTrailSl);
  setNum("rtEngPointTrailSlPts", u.manualPointTrailSlPoints);
  setNum("rtEngLot", u.lotSize);
  const auto = !!(STATE && STATE.autoLots);
  const lotsEl = document.getElementById("rtEngLots");
  if (lotsEl) {
    if (ae !== lotsEl) lotsEl.value = u.lots != null ? u.lots : 1;
    lotsEl.readOnly = auto;
    lotsEl.style.opacity = auto ? ".7" : "1";
    lotsEl.title = auto ? "Auto-Lot ON - engine min(volume %, OI %, margin) se lots decide karega" : "";
  }
  const al = document.getElementById("rtEngAutoLots");
  if (al) {
    if (ae !== al) al.checked = auto;
    al.parentElement && (al.parentElement.style.opacity = "1");
  }
  // Independent Auto-Lot liquidity caps (volume% and OI%). Only meaningful while
  // Auto-Lot is ON, so they fade + lock when it is off.
  const volPctEl = document.getElementById("rtEngAutoLotVolPct");
  if (volPctEl && ae !== volPctEl) volPctEl.value = u.autoLotVolumePct != null ? num(u.autoLotVolumePct) : 1;
  const oiPctEl = document.getElementById("rtEngAutoLotOiPct");
  if (oiPctEl && ae !== oiPctEl) oiPctEl.value = u.autoLotOiPct != null ? num(u.autoLotOiPct) : 1;
  [volPctEl, oiPctEl].forEach((e) => {
    if (!e) return;
    e.disabled = !auto;
    e.style.opacity = auto ? "1" : ".5";
  });
  const info = document.getElementById("rtEngAutoLotsInfo");
  if (info) {
    const vp = u.autoLotVolumePct != null ? num(u.autoLotVolumePct) : 1;
    const op = u.autoLotOiPct != null ? num(u.autoLotOiPct) : 1;
    info.textContent = auto
      ? `ON - min(volume ${vp}%, OI ${op}%, margin) se per-trade lots auto select`
      : "";
  }
}

function currentMethodCfg(key) {
  const cfg = (STATE && STATE.orderCfg) || {};
  return cfg[key] || {};
}

function updateMargin() {
  const avail = availableMargin();
  const ha = document.getElementById("rtomMarginAvail");
  if (ha) ha.textContent = avail == null ? "--" : fmtMoney(avail);
  const u = (STATE && STATE.settings) || {};
  const lotEl = document.getElementById("rtEngLot");
  let lot = num(u.lotSize);
  if (!(lot > 0) && lotEl) lot = num(lotEl.value);
  if (!(lot > 0)) lot = chartLot;
  const lotsEl = document.getElementById("rtEngLots");
  const lots = num(lotsEl && lotsEl.value) || 1;
  const sel = chartSelection();
  const pEl = document.querySelector("#rtEngMethodOpts .rtEng-price");
  const price = num(pEl && pEl.value) || (sel && sel.ltp) || 0;
  const qty = lot * lots;
  const req = document.getElementById("rtEngReq");
  if (req && !req.dataset.live) req.textContent = qty <= 0 || price <= 0 ? "--" : fmtMoney(qty * price);
  scheduleMarginRequired();
}

// The chart's active instrument + its live quote, published by sidebar.js.
function chartSelection() {
  return window.__chartSelection || null;
}

// Fill the active method's Est. price from the live chart LTP (unless the user
// typed a manual price), so order entry is priced off live data.
function applyChartPrice() {
  const sel = chartSelection();
  const p = document.querySelector("#rtEngMethodOpts .rtEng-price");
  if (p && priceAuto[activeMethod] !== false && document.activeElement !== p && sel && sel.ltp > 0) {
    p.value = Number(sel.ltp).toFixed(2);
  }
  updateMargin();
}

function scheduleMarginRequired() {
  if (marginReqTimer) clearTimeout(marginReqTimer);
  marginReqTimer = setTimeout(refreshMarginRequired, 350);
}

// Pull the real Dhan margin for the chart symbol + configured qty via the engine
// (`/api/rt/instrument`), replacing the notional estimate with broker truth.
async function refreshMarginRequired() {
  const sel = chartSelection();
  if (!sel) return;
  const u = (STATE && STATE.settings) || {};
  const lotEl = document.getElementById("rtEngLot");
  const lotsEl = document.getElementById("rtEngLots");
  const lot = num(lotEl && lotEl.value) || num(u.lotSize) || chartLot;
  const lots = num(lotsEl && lotsEl.value) || 1;
  const pEl = document.querySelector("#rtEngMethodOpts .rtEng-price");
  const price = num(pEl && pEl.value) || sel.ltp || 0;
  const params = new URLSearchParams({
    securityId: String(sel.id || 0),
    exchangeSegment: sel.exch || "",
    symbol: sel.name || "",
    side: "BUY",
    productType: "INTRADAY",
    lots: String(lots),
    lotSize: String(lot || 0),
    price: String(price || 0),
  });
  try {
    const r = await fetch("/api/rt/instrument?" + params.toString());
    const d = await r.json();
    if (!d || d.ok !== true) return;
    const req = document.getElementById("rtEngReq");
    if (req) {
      if (d.marginRequired > 0) {
        req.dataset.live = "1";
        req.textContent = fmtMoney(d.marginRequired);
      } else {
        delete req.dataset.live;
      }
    }
    const av = d.availableBalance != null ? d.availableBalance : availableMargin();
    const ha = document.getElementById("rtomMarginAvail");
    if (ha && av != null) ha.textContent = fmtMoney(av);
    if (lotEl && d.lotSize > 0) {
      chartLot = d.lotSize;
      lotEl.placeholder = String(d.lotSize);
    }
    if (pEl && d.ltp > 0 && priceAuto[activeMethod] !== false && document.activeElement !== pEl) {
      pEl.value = Number(d.ltp).toFixed(2);
    }
    const fr = document.getElementById("rtEngFreeze");
    if (fr && num(sel.id) > 0) {
      try {
        const f = await API.freeze(sel.id, sel.name);
        fr.textContent = f && num(f.freezeQty) > 0 ? String(num(f.freezeQty)) : "--";
      } catch (e) {
        /* freeze qty optional */
      }
    }
  } catch (e) {
    /* broker offline - keep the estimate */
  }
}

function availableMargin() {
  const f = (STATE && STATE.funds) || {};
  const bal = f.availabelBalance != null ? f.availabelBalance : f.availableBalance;
  return bal != null ? num(bal) : null;
}

// Reflect the live available balance into the read-only Engine-controls readout
// and the top funds pill. Called on every snapshot and account refresh.
function updateBalanceReadouts() {
  const bal = availableMargin();
  const txt = bal != null ? fmtMoney(bal) : "--";
  const fp = document.getElementById("rtFundsPill");
  if (fp) fp.textContent = "Funds: " + txt;
  const ab = document.getElementById("rtEngAvailBal");
  if (ab) ab.textContent = txt;
}

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------
function settingsFromDom() {
  const s = {};
  document.querySelectorAll("#tab-realtime [data-set]").forEach((inp) => {
    const k = inp.getAttribute("data-set");
    s[k] = inp.type === "checkbox" ? inp.checked : inp.type === "number" ? num(inp.value) : inp.value;
  });
  const intLists = ["moversIndices", "commodityList", "scannerExclude"];
  document.querySelectorAll("#tab-realtime [data-list]").forEach((inp) => {
    const k = inp.getAttribute("data-list");
    const raw = String(inp.value || "").split(",").map((x) => x.trim()).filter(Boolean);
    s[k] = intLists.indexOf(k) >= 0 ? raw.map((x) => num(x)) : raw;
  });
  const f = Object.assign({}, (STATE && STATE.settings && STATE.settings.filters) || {});
  document.querySelectorAll("#tab-realtime [data-filter]").forEach((cb) => {
    f[cb.getAttribute("data-filter")] = cb.checked;
  });
  s.filters = f;
  // Multi-session trade windows are edited by their own row below (not via
  // [data-set]), so carry them over from the last snapshot instead of letting a
  // generic settings save erase them.
  s.tradeSessions = (STATE && STATE.settings && STATE.settings.tradeSessions) || [];
  // Manual Strike Select list is edited by its own row (not via [data-set]), so
  // carry it over from the last snapshot instead of letting a generic save drop it.
  s.manualStrikes = (STATE && STATE.settings && STATE.settings.manualStrikes) || [];
  // Top Movers index legs are edited by each index chip's own CE/PE select (not
  // via [data-set]), so carry them over too.
  s.moversIndexLegs = (STATE && STATE.settings && STATE.settings.moversIndexLegs) || [];
  return s;
}

function pushSetting(partial) {
  const s = Object.assign({}, (STATE && STATE.settings) || {}, partial);
  API.settings(s).then(refresh);
}

// ---------------------------------------------------------------------------
// Engine Scan (testing): throttle the entry-condition scan either to a fixed
// interval (minutes + seconds + milliseconds) or to each bar close of a 1min /
// 5min timeframe. Exits are never throttled - the guardian task still runs at
// 50ms. The whole config area fades in/out with the toggles so the inactive
// controls are obviously not in play.
// ---------------------------------------------------------------------------
const SCAN_MIN_MS = 50;
const SCAN_MAX_MS = 86400000;

function scanTfNorm(v) {
  return String(v || "").toLowerCase().indexOf("5") === 0 ? "5min" : "1min";
}

// Combine the three unit inputs into one millisecond value, clamped to the same
// bounds the engine uses (so the UI and server never disagree).
function scanIntervalFromInputs() {
  const g = (id) => {
    const el = document.getElementById(id);
    return el ? Math.max(0, Math.floor(num(el.value) || 0)) : 0;
  };
  let total = g("rtScanMin") * 60000 + g("rtScanSec") * 1000 + g("rtScanMs");
  if (total < SCAN_MIN_MS) total = SCAN_MIN_MS;
  if (total > SCAN_MAX_MS) total = SCAN_MAX_MS;
  return total;
}

function humanScanMs(ms) {
  if (ms >= 60000 && ms % 60000 === 0) return ms / 60000 + (ms === 60000 ? " minute" : " minutes");
  if (ms >= 1000 && ms % 1000 === 0) return ms / 1000 + (ms === 1000 ? " second" : " seconds");
  return ms + " ms";
}

// Reflect the current checkbox/select state onto the DOM (fade + enable/disable
// + status text). Reads live DOM controls, so it can run before a server round
// trip for instant feedback.
function refreshEngineScanUi() {
  const master = document.getElementById("rtEngineScanCb");
  const barCb = document.getElementById("rtScanBarCloseCb");
  const on = !!(master && master.checked);
  const bc = on && !!(barCb && barCb.checked);
  const intervalActive = on && !bc;
  const barActive = on && bc;
  const box = (id) => document.getElementById(id);
  if (box("rtScanIntervalBox")) box("rtScanIntervalBox").classList.toggle("on", intervalActive);
  if (box("rtScanBarCloseChk")) box("rtScanBarCloseChk").classList.toggle("on", on);
  if (box("rtScanBarCloseBox")) box("rtScanBarCloseBox").classList.toggle("on", barActive);
  ["rtScanMin", "rtScanSec", "rtScanMs"].forEach((id) => {
    if (box(id)) box(id).disabled = !intervalActive;
  });
  if (box("rtScanBarCloseTf")) box("rtScanBarCloseTf").disabled = !barActive;
  if (barCb) barCb.disabled = !on;
  const status = box("rtScanStatus");
  if (status) {
    if (!on) {
      status.textContent = "Scan OFF (normal ~100ms tick)";
      status.style.color = "#666";
    } else if (bc) {
      const tf = scanTfNorm(box("rtScanBarCloseTf") && box("rtScanBarCloseTf").value);
      status.textContent = "Scan on each " + (tf === "5min" ? "5-minute" : "1-minute") + " bar close";
      status.style.color = "#00d4aa";
    } else {
      status.textContent = "Scanning every " + humanScanMs(scanIntervalFromInputs());
      status.style.color = "#00d4aa";
    }
  }
}

// Sync the Engine Scan controls from a settings snapshot. Never clobbers the
// field the user is currently typing in.
function applyEngineScanUI(s) {
  s = s || {};
  const raw = Math.floor(num(s.scanIntervalMs) || 0);
  const total = raw < SCAN_MIN_MS ? 100 : Math.min(raw, SCAN_MAX_MS);
  const active = document.activeElement;
  const setVal = (id, v) => {
    const el = document.getElementById(id);
    if (el && el !== active) el.value = v;
  };
  setVal("rtScanMin", Math.floor(total / 60000));
  setVal("rtScanSec", Math.floor((total % 60000) / 1000));
  setVal("rtScanMs", total % 1000);
  const hidden = document.getElementById("rtScanTotalMs");
  if (hidden && hidden !== active) hidden.value = total;
  const sel = document.getElementById("rtScanBarCloseTf");
  if (sel && sel !== active) sel.value = scanTfNorm(s.scanBarCloseTf);
  refreshEngineScanUi();
}

// --- Run mode (old AST run-mode section) --------------------------------
// Normal mode runs the ticked strategies; Indicator-filters mode trades the
// scanner universe (Top Movers / NIFTY trend / Commodities) with the ticked
// Bullish/Bearish indicator filters agreeing by majority (tick "All together
// (strict AND)" to require every one, or enable AI Brain for a score/veto gate).
// Exactly one mode is active: ticking a mode's checkbox activates it (the other
// fades and becomes inactive); unticking one switches to the other so a mode is
// always selected.
function runModeIsFilter() {
  return !!(STATE && STATE.settings && STATE.settings.filterMode);
}

function applyRunModeUI() {
  const filterMode = runModeIsFilter();
  const nCb = document.getElementById("rtRunPaperModeCb");
  if (nCb) nCb.checked = !filterMode;
  const fCb = document.getElementById("rtFilterModeCb");
  if (fCb) fCb.checked = filterMode;
  const runBtn = document.getElementById("rtRunPaperBtn");
  if (runBtn) {
    runBtn.style.opacity = filterMode ? "0.35" : "1";
    runBtn.disabled = filterMode;
  }
  const fBtn = document.getElementById("rtFilterPaperBtn");
  if (fBtn) {
    fBtn.style.opacity = filterMode ? "1" : "0.35";
    fBtn.disabled = !filterMode;
  }
}

async function saveRunMode(partial) {
  const s = Object.assign({}, (STATE && STATE.settings) || {}, partial);
  await API.settings(s);
  await refresh();
  applyRunModeUI();
}

async function startRunEngine() {
  if (!(await uiConfirm(PAPER
    ? "Paper engine ON karein?\n\nSab entries/exits SIMULATED honge (koi real Dhan order nahi).\nSignal live Dhan feed se aayenge."
    : "Realtime engine ON karein?\n\nLIVE Dhan orders (ARMED) enable ho jayenge.\nSirf OK dabao jab aap real trades chahte ho.", { danger: true }))) return false;
  await API.engine({ on: true });
  const r = await API.arm({ armed: true });
  if (!r.ok) {
    uiAlert(r.error || "arm failed");
    await API.engine({ on: false });
    return false;
  }
  if (STATE) {
    STATE.engineOn = true;
    STATE.armed = true;
    paintEnginePills(true, true);
  }
  return true;
}

/* "Run Paper Trading" (normal mode): turn off the dynamic AI auto-picker and
   keep manual selection on so only the ticked strategies are evaluated, then
   start the engine. */
async function runPaper() {
  await saveRunMode({ callManual: true, aiPick: false, filterMode: false });
  if (await startRunEngine()) refresh();
}

/* "Place trades based on Indicator filters": the scanner universe is traded
   when the ticked Bullish/Bearish filters agree by majority (or satisfy the
   AI Brain threshold when enabled); tick "All together (strict AND)" to demand
   every filter. */
async function runFilterPaper() {
  await saveRunMode({ callManual: true, aiPick: false, filterMode: true });
  if (await startRunEngine()) refresh();
}

/* Mutual-exclusion run-mode selector: ticking a mode's checkbox activates it,
   unticking one silently switches to the other so a mode is always selected. */
async function onRunModeToggle(mode, checked) {
  const filterMode = checked ? mode === "filter" : mode !== "filter";
  await saveRunMode({ filterMode });
}

function wire() {
  const $ = (id) => document.getElementById(id);

  // Closed Trades "Show all" / "Collapse": lifts/restores the 230px inner-scroll
  // cap. The full ledger is already in the DOM, so this is a pure view toggle.
  const closedBtn = $("rtClosedExpand");
  if (closedBtn) closedBtn.onclick = () => {
    closedExpanded = !closedExpanded;
    try { localStorage.setItem("rtClosedExpand", closedExpanded ? "1" : "0"); } catch (e) { /* non-fatal */ }
  applyClosedExpand();

  // Engine Scan: the interval inputs are not [data-set] (they combine into the
  // hidden scanIntervalMs), so wire them here; the checkboxes/select are normal
  // [data-set] controls saved by the generic binder further down.
  const scanMasterCb = $("rtEngineScanCb");
  const scanBarCb = $("rtScanBarCloseCb");
  [scanMasterCb, scanBarCb].forEach((cb) => {
    if (cb) cb.addEventListener("change", refreshEngineScanUi);
  });
  const scanTfSel = $("rtScanBarCloseTf");
  if (scanTfSel) scanTfSel.addEventListener("change", refreshEngineScanUi);
  let scanSaveTimer = null;
  const scheduleScanSave = () => {
    const hidden = $("rtScanTotalMs");
    if (hidden) hidden.value = scanIntervalFromInputs();
    if (scanSaveTimer) clearTimeout(scanSaveTimer);
    scanSaveTimer = setTimeout(() => {
      scanSaveTimer = null;
      API.settings(settingsFromDom()).then(refresh);
    }, 500);
  };
  ["rtScanMin", "rtScanSec", "rtScanMs"].forEach((id) => {
    const el = $(id);
    if (el) el.addEventListener("input", () => { refreshEngineScanUi(); scheduleScanSave(); });
  });
  refreshEngineScanUi();

  };
  applyClosedExpand();

  // Follow the chart's active instrument: keep the Order Placement cards priced
  // off the same live symbol the user is viewing.
  window.addEventListener("chartselection", () => {
    if (document.getElementById("rtomRow")) applyChartPrice();
  });

  $("rtEngineToggle").onclick = async () => {
    const on = !(STATE && STATE.engineOn);
    if (on) {
      if (!(await uiConfirm(PAPER
        ? "Paper engine ON karein?\n\nSab entries/exits SIMULATED honge (koi real Dhan order nahi).\nSignal live Dhan feed se aayenge."
        : "Realtime engine ON karein?\n\nLIVE Dhan orders (ARMED) enable ho jayenge.\nSirf OK dabao jab aap real trades chahte ho.", { danger: true }))) return;
      await API.engine({ on: true });
      const r = await API.arm({ armed: true });
      if (!r.ok) {
        uiAlert(r.error || "arm failed");
        await API.engine({ on: false });
      } else if (STATE) {
        // Optimistic: reflect ON immediately; the next poll re-confirms.
        STATE.engineOn = true;
        STATE.armed = true;
        paintEnginePills(true, true);
      }
    } else {
      await API.engine({ on: false });
      if (STATE) {
        STATE.engineOn = false;
        paintEnginePills(false, !!STATE.armed);
      }
    }
    refresh();
  };

  // Run-mode section: Normal mode / Run Paper Trading + Indicator-filters mode
  // / Place trades based on Indicator filters (mutually exclusive).
  const runBtn = $("rtRunPaperBtn");
  if (runBtn) runBtn.onclick = () => runPaper();
  const filterBtn = $("rtFilterPaperBtn");
  if (filterBtn) filterBtn.onclick = () => runFilterPaper();
  const runCb = $("rtRunPaperModeCb");
  if (runCb) runCb.onchange = () => onRunModeToggle("normal", runCb.checked);
  const filterCb = $("rtFilterModeCb");
  if (filterCb) filterCb.onchange = () => onRunModeToggle("filter", filterCb.checked);
  applyRunModeUI();

  // Engine Risk & Quantity row. SL / Trail SL default to a sane %
  // the moment they are ticked (a tick with 0% would disable the stop), and the
  // Auto-Lot toggle drives the same `autoLots` flag `open_entry` reads.
  const riskPct = (pctId, def) => num(($(pctId) || {}).value) || def;
  const slCb = $("rtEngSl");
  if (slCb) {
    slCb.onchange = () => {
      if (slCb.checked && !num(($("rtEngSlPct") || {}).value)) $("rtEngSlPct").value = 1.5;
      pushSetting({ manualSl: slCb.checked, manualSlPct: riskPct("rtEngSlPct", 1.5) });
    };
  }
  const slPct = $("rtEngSlPct");
  if (slPct) slPct.oninput = () => pushSetting({ manualSl: true, manualSlPct: num(slPct.value) });
  const trCb = $("rtEngTrailSl");
  if (trCb) {
    trCb.onchange = () => {
      if (trCb.checked && !num(($("rtEngTrailSlPct") || {}).value)) $("rtEngTrailSlPct").value = 1;
      pushSetting({ manualTrailSl: trCb.checked, manualTrailSlPct: riskPct("rtEngTrailSlPct", 1) });
    };
  }
  const trPct = $("rtEngTrailSlPct");
  if (trPct) trPct.oninput = () => pushSetting({ manualTrailSl: true, manualTrailSlPct: num(trPct.value) });
  const ptrCb = $("rtEngPointTrailSl");
  if (ptrCb) {
    ptrCb.onchange = () => {
      if (ptrCb.checked && !num(($("rtEngPointTrailSlPts") || {}).value)) $("rtEngPointTrailSlPts").value = 1;
      pushSetting({ manualPointTrailSl: ptrCb.checked, manualPointTrailSlPoints: riskPct("rtEngPointTrailSlPts", 1) });
    };
  }
  const ptrPts = $("rtEngPointTrailSlPts");
  if (ptrPts) ptrPts.oninput = () => pushSetting({ manualPointTrailSl: true, manualPointTrailSlPoints: num(ptrPts.value) });
  const lotEl = $("rtEngLot");
  if (lotEl) lotEl.onchange = () => pushSetting({ lotSize: num(lotEl.value) });
  const lotsEl = $("rtEngLots");
  if (lotsEl) lotsEl.onchange = () => { if (!(STATE && STATE.autoLots)) pushSetting({ lots: num(lotsEl.value) }); };
  const alCb = $("rtEngAutoLots");
  if (alCb) alCb.onchange = () => API.autoLots({ autoLots: !!alCb.checked }).then(refresh);

  // "Only +green premium strikes" and "Opposite side fallback (if all minus)" are
  // the required base the "Execute Trade In" dropdown runs on. If the operator
  // tries to switch either off, alert and keep it enabled instead of shipping a
  // config the dropdown cannot honour.
  const STRIKE_BASE_GUARD = {
    onlyPositive: "Only +green premium strikes",
    fallbackOppositeSide: "Opposite side fallback (if all minus)",
  };
  document.querySelectorAll("#tab-realtime [data-set]").forEach((inp) => {
    inp.onchange = () => {
      const k = inp.getAttribute("data-set");
      if (inp.type === "checkbox" && !inp.checked && STRIKE_BASE_GUARD[k]) {
        uiAlert(
          '"' + STRIKE_BASE_GUARD[k] + '" ON rakhna zaroori hai. "Execute Trade In" ke sare dropdown options isi +green base par kaam karte hain, isliye ise enable kar diya gaya hai.'
        );
        inp.checked = true;
      }
      syncInterlocks();
      API.settings(settingsFromDom()).then(refresh);
    };
  });

  // Manual Strike Select (testing): the checkbox is a normal [data-set] setting
  // (the generic handler above saves it; syncInterlocks hides/shows the box and
  // fades the automatic strike controls). These handlers drive the picker.
  const msCb = document.getElementById("rtManualStrikeCb");
  if (msCb) {
    msCb.addEventListener("change", () => {
      if (msCb.checked) manualLoadInstruments(false);
    });
  }
  const msInst = document.getElementById("rtManualInstrument");
  if (msInst) msInst.onchange = () => manualLoadChain();
  const msExp = document.getElementById("rtManualExpiry");
  if (msExp) msExp.onchange = () => manualLoadChain();
  const msAdd = document.getElementById("rtManualStrikeAdd");
  if (msAdd) msAdd.onclick = () => addManualStrike();

  document.querySelectorAll("#tab-realtime [data-act]").forEach((b) => {
    const act = b.getAttribute("data-act");
    b.onclick = async () => {
      if (act === "runref" || act === "refresh") {
        refresh();
      } else if (act === "ticknow") {
        // "Run / Tick Now" is the obvious "go" button: if the engine is not
        // armed, a bare force-tick would scan nothing and silently place no
        // orders. Arm it first (with the same confirm as the engine toggle),
        // then force the immediate evaluation pass.
        if (!(STATE && STATE.armed)) {
          if (!(await startRunEngine())) return;
        }
        await API.tick();
        refresh();
      } else if (act === "closeall") {
        await squareOffAllTrades();
      } else if (act === "stoplall") {
        if (!(await uiConfirm("Stop ALL running strategies? Open positions are NOT closed (use Square Off All Trades)."))) return;
        await API.engine({ on: false });
        await API.selectionSet({ action: "none" });
        refresh();
        loadAstSections();
      } else if (act === "stopall") {
        if (!(await uiConfirm("Stop ALL strategies? (open positions are NOT squared off)"))) return;
        for (const s of (STATE && STATE.strategies) || []) {
          if (s.enabled) await API.strategySave(Object.assign({}, s, { enabled: false }));
        }
        refresh();
      } else if (act === "resetpnl") {
        if (!(await uiConfirm("Clear closed trades + logs and reset realized P&L?", { danger: true }))) return;
        await API.reset({ what: "closed" });
        await API.reset({ what: "logs" });
        // Drop the client-side closed/day-stats caches so the emptied book is
        // actually reflected (the 1s poll only ships the newest slice and would
        // otherwise keep painting the cached rows).
        resetBookCaches();
        refresh();
      } else if (act === "selectall") {
        for (const s of (STATE && STATE.strategies) || []) {
          if (!s.enabled) await API.strategySave(Object.assign({}, s, { enabled: true }));
        }
        refresh();
      }
    };
  });

  const sqBtn = $("rtSquareOff");
  if (sqBtn) sqBtn.onclick = () => squareOffAllTrades();
  const refreshAccBtn = $("rtRefreshAccount");
  if (refreshAccBtn) refreshAccBtn.onclick = () => refreshAccount();

  const condClear = $("rtCondLogClear");
  if (condClear)
    condClear.onclick = async () => {
      if (!(await uiConfirm("Clear the Condition Log?"))) return;
      await API.reset({ what: "logs" });
      refresh();
    };
  const condRefresh = $("rtCondLogRefresh");
  if (condRefresh) condRefresh.onclick = () => refreshLogs();
  const condLvl = $("rtCondLogLevel");
  if (condLvl) condLvl.onchange = () => renderConditionLog();
  const condPause = $("rtCondLogPause");
  if (condPause) condPause.onchange = () => renderConditionLog();

  // Live Data Pool
  $("rtDataPool").onchange = () => {
    const on = $("rtDataPool").checked;
    applyDataPoolUi(on);
    API.settings(settingsFromDom()).then(refresh);
  };
  $("rtDataPoolRefresh").onclick = () => loadPool(true);

  // Templates (server-backed AST engine settings, assignable to directions)
  refreshTemplateList();
  $("rtTplSave").onclick = async () => {
    const name = ($("rtTplName").value || "").trim();
    if (!name) {
      uiAlert("Template name required");
      return;
    }
    const side = ($("rtTplMode").value || "bullish");
    const r = await API.template({ action: "save", name, side });
    if (!r || !r.ok) {
      $("rtTplInfo").textContent = (r && r.error) || "save failed";
      return;
    }
    $("rtTplName").value = "";
    $("rtTplInfo").textContent = "saved";
    await refreshTemplateList();
  };
  $("rtTplOpen").onchange = async () => {
    const name = $("rtTplOpen").value;
    if (!name) return;
    const r = await API.template({ action: "open", name });
    $("rtTplInfo").textContent = r && r.ok ? "opened " + name : (r && r.error) || "open failed";
    refresh();
  };
  $("rtTplDelete").onclick = async () => {
    const name = $("rtTplOpen").value;
    if (!name) return;
    const r = await API.template({ action: "delete", name });
    $("rtTplInfo").textContent = r && r.ok ? "deleted" : (r && r.error) || "delete failed";
    await refreshTemplateList();
  };

  // AST saved templates quick run (top of the tab, old-app parity).
  const qrBtn = $("rtAstTplRunBtn");
  if (qrBtn) qrBtn.onclick = () => quickRunTemplate();

  // AST master toggles (Top Movers / NIFTY Trend / Commodities).
  document.querySelectorAll("#tab-realtime [data-toggle]").forEach((b) => {
    b.onclick = () => {
      const k = b.getAttribute("data-toggle");
      const cur = !!((STATE && STATE.settings && STATE.settings[k]));
      const next = !cur;
      const patch = { [k]: next };
      // Top Movers and NIFTY Trend Following are connected and must run
      // together: switching either one ON also switches the other ON. Turning
      // one OFF stays individual (the other keeps its own state), and the old
      // fade-out that forced one off while the other was on is gone.
      if (next && (k === "moversOn" || k === "niftyTrendOn")) {
        patch.moversOn = true;
        patch.niftyTrendOn = true;
      }
      pushSetting(patch);
    };
  });

  // Commodity picker: add / clear MCX futures in the commodityList.
  const cAdd = $("rtCommodityAddBtn");
  if (cAdd)
    cAdd.onclick = () => {
      const sel = $("rtCommodityAdd");
      const id = sel && sel.value;
      if (!id) return;
      const inp = document.querySelector('#tab-realtime [data-list="commodityList"]');
      const ids = String((inp && inp.value) || "").split(",").map((x) => x.trim()).filter(Boolean);
      if (ids.indexOf(String(id)) < 0) ids.push(String(id));
      if (inp) inp.value = ids.join(", ");
      API.settings(settingsFromDom()).then(() => {
        renderChips("commodityList", "rtCommodityChips", "commodity");
        refresh();
      });
    };
  const cClear = $("rtCommodityClearBtn");
  if (cClear)
    cClear.onclick = () => {
      const inp = document.querySelector('#tab-realtime [data-list="commodityList"]');
      if (inp) inp.value = "";
      API.settings(settingsFromDom()).then(() => {
        renderChips("commodityList", "rtCommodityChips", "commodity");
        refresh();
      });
    };

  // Index / indicator dropdown pickers.
  wireAstPickers();

  // Multi-session trade windows (Trade times -> Trading sessions).
  wireTradeSessions();

  // Individual Bullish / Bearish indicator-filter checkboxes.
  // `filtersBusy` holds the row steady across the save round-trip so the 1s
  // snapshot poll cannot revert a tick the user just made.
  const saveFilters = () => {
    filtersBusy = true;
    return API.settings(settingsFromDom()).finally(() => {
      filtersBusy = false;
    });
  };
  document.querySelectorAll("#tab-realtime [data-filter]").forEach((cb) => {
    cb.onchange = () => {
      syncInterlocks();
      saveFilters().then(refresh, refresh);
    };
  });

  // Free-text id/indicator lists (Top Movers indices, NIFTY index list /
  // confirm indicators, commodity ids): persist on blur/change.
  document.querySelectorAll("#tab-realtime [data-list]").forEach((inp) => {
    inp.onchange = () => {
      syncInterlocks();
      API.settings(settingsFromDom()).then(refresh);
    };
  });

  // Bullish / Bearish "(select all)" masters.
  document.querySelectorAll("#tab-realtime [data-master]").forEach((cb) => {
    cb.onchange = () => {
      const side = cb.getAttribute("data-master");
      const list = side === "Bull" ? FILTER_BULL : FILTER_BEAR;
      const on = cb.checked;
      const sec = document.getElementById("rtFilterSection" + side);
      if (sec) sec.querySelectorAll("[data-filter]").forEach((f) => (f.checked = on));
      syncInterlocks();
      saveFilters().then(refresh, refresh);
    };
  });

  // Mirror the selected side's filters to the other side.
  const mirror = $("rtMirrorFilters");
  if (mirror)
    mirror.onclick = () => {
      const count = (side) => Array.from(document.querySelectorAll("#rtFilterSection" + side + " [data-filter]")).filter((c) => c.checked).length;
      const bullN = count("Bull");
      const bearN = count("Bear");
      // Nothing ticked on either side: there is nothing to copy.
      if (bullN === 0 && bearN === 0) return;
      // Copy from the side that actually holds filters; when both sides have
      // some, the fuller one wins (old behaviour) so a lone tick always mirrors.
      const from = bullN >= bearN ? "Bull" : "Bear";
      const to = from === "Bull" ? "Bear" : "Bull";
      const map = from === "Bull" ? FILTER_MIRROR_TO_BEAR : FILTER_MIRROR_TO_BULL;
      document.querySelectorAll("#rtFilterSection" + from + " [data-filter]").forEach((src) => {
        const srcKey = src.getAttribute("data-filter");
        const key = map[srcKey] || srcKey;
        const dst = document.querySelector('#rtFilterSection' + to + ' [data-filter="' + key + '"]');
        if (dst) dst.checked = src.checked;
      });
      // syncInterlocks() re-derives each section master and dim state.
      syncInterlocks();
      saveFilters().then(refresh, refresh);
    };

  // Entry Timing Diagnostics.
  const etClear = $("rtEtClear");
  if (etClear)
    etClear.onclick = async () => {
      await API.entryTimingSet({ action: "clear" });
      loadAstSections();
    };
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------
// Paint the header engine/arm pills + toggle label. Used by the snapshot render
// and by the engine toggle for an instant optimistic update, so the button always
// reacts even before the next (now lightweight) poll confirms it.
function paintEnginePills(on, armed) {
  const pEngine = document.getElementById("rtEnginePill");
  const pArm = document.getElementById("rtArmPill");
  if (pEngine) {
    pEngine.textContent = on ? "Engine: ON" : "Engine: OFF";
    pEngine.className = "rt-pill " + (on ? "on" : "off");
  }
  if (pArm) {
    pArm.textContent = armed ? "ARMED" : "DISARMED";
    pArm.className = "rt-pill " + (armed ? "armed" : "off");
  }
  const tg = document.getElementById("rtEngineToggle");
  if (tg) tg.textContent = "AI Smart Trading: " + (on ? "ON" : "OFF");
  const warn = document.getElementById("rtDisarmWarn");
  if (warn) warn.style.display = on && !armed ? "inline-block" : "none";
}

function renderSnapshot(s) {
  if (!s) return;
  STATE = s;
  if (!methodInit && s.method) {
    activeMethod = s.method;
    methodInit = true;
    buildMethodCards();
  }

  const on = !!s.engineOn;
  const armed = !!s.armed;
  paintEnginePills(on, armed);
  updateBalanceReadouts();

  // apply settings to the engine-control checkboxes/inputs
  applySettingsToDom(s.settings);

  // Smart P&L summary: the engine panes show TODAY ONLY - the full history lives
  // in the Trade Stats tabs. `st` is the all-time closed book used only as a
  // fallback until the day-scoped (`range=today`) stats land; once DAY_STATS is
  // available it drives realized / win-rate / trade counts / charges. Live gross
  // P&L stays today's realized plus the currently open (unrealized) book.
  const st = s.stats || {};
  const day = DAY_STATS;
  const sum = document.getElementById("rtSummary");
  if (sum) {
    const total = day && day.n != null ? num(day.n) : num(st.total);
    const wins = day && day.wins != null ? num(day.wins) : num(st.wins);
    const losses = day && day.losses != null ? num(day.losses) : Math.max(0, total - wins);
    const realized = day && day.net != null ? num(day.net) : num(st.realized);
    const unrealized = num(st.unrealized);
    const live = realized + unrealized;
    const wr = total ? (wins / total) * 100 : 0;
    const charges = day && day.charges != null ? num(day.charges) : num(st.charges);
    const chOn = !!st.chargesOn;
    const card = (label, val, color) =>
      `<div style="flex:1;min-width:130px;border:1px solid #1e1e40;border-radius:4px;background:#12122a;padding:5px 9px">` +
      `<div style="font-size:9px;color:#888;white-space:nowrap">${label}</div>` +
      `<div style="font-size:14px;font-weight:700;color:${color};white-space:nowrap">${val}</div></div>`;
    sum.innerHTML = [
      card("Smart Live P&L (Today)", (live >= 0 ? "+" : "-") + fmtMoney(Math.abs(live)), live >= 0 ? "#00d4aa" : "#ef5350"),
      card("Smart Realized P&L (Today)", (realized >= 0 ? "+" : "-") + fmtMoney(Math.abs(realized)), realized >= 0 ? "#00d4aa" : "#ef5350"),
      card("Today Win Rate", wr.toFixed(2) + "%", wr >= 50 ? "#00d4aa" : "#ff9800"),
      card("Today Trades (W/L)", `${total} (${wins}W / ${losses}L)`, "#d0d0d0"),
      card("Today Charges", (chOn ? "-" : "") + fmtMoney(charges), "#ff9800"),
      `<div style="flex:0 0 auto;display:flex;align-items:center;padding:2px">` +
        `<button class="btn-action warn" data-smartreset="1" style="width:auto;padding:7px 14px;margin:0;font-size:10px" ` +
        `title="Sab Smart P&L zero karo: closed trades + logs${PAPER ? " + open paper positions" : ""} clear ho jayenge. Undo nahi hoga.">Reset</button></div>`,
    ].join("");
    const rb = sum.querySelector("[data-smartreset]");
    if (rb) {
      rb.onclick = async () => {
        const msg = "Reset all Smart P&L?\n\nYe closed trades aur logs" + (PAPER ? " aur open paper positions" : "") + " clear kar dega. Undo nahi hoga.";
        if (!(await uiConfirm(msg, { danger: true }))) return;
        await API.reset({ what: "smart" });
        resetBookCaches();
        refresh();
      };
    }
  }

  syncCards();
  renderMarginBar(s.margin);
  renderRunning(s);
  renderClosed(s);
  // Pull the full ledger once whenever the closed count changes (first load or a
  // fresh close); the 1s poll itself only ever ships the newest slice.
  const closedTotal = s.closedCount != null ? num(s.closedCount) : null;
  if (closedTotal == null && CLOSED_CACHE == null) loadClosed();
  // The on-demand ledger is authoritative and never behind the snapshot, so only
  // re-pull when it is actually missing rows (cache shorter than the count). The
  // old `!==` test re-fetched the whole multi-MB ledger every second whenever the
  // two briefly disagreed, which alone could freeze the pane.
  else if (closedTotal != null && (!CLOSED_CACHE || CLOSED_CACHE.length < closedTotal)) loadClosed();
  // Keep the TODAY-only summary fresh: reload day stats whenever the closed
  // count moves (or on the slow throttle inside loadDayStats).
  loadDayStats(closedTotal);
  renderHoldings(s);
  loadPool();
  loadScanners();
  loadAstSections();
  renderConditionLog();
  renderQuickRun();
  renderStrikes();
}

// Live Top Movers + NIFTY trend readouts (auto CE/PE side source).
async function loadScanners() {
  // Three throttled readouts per call; they change on the server's own cadence,
  // not every second, so polling them at 1s only added churn. Refresh a bit
  // slower - the master-toggle state itself still updates on the 1s snapshot.
  const now = Date.now();
  if (now - scannerPollAt < 2500) return;
  scannerPollAt = now;
  const cfg = (STATE && STATE.settings) || {};
  const ml = document.getElementById("rtMoversList");
  const tl = document.getElementById("rtNiftyTrendList");
  const cl = document.getElementById("rtCommodityStatus");
  const symName = (id) => {
    const s = CATALOG.symbols.find((x) => num(x.id) === num(id));
    return s ? s.name : id;
  };
  // The three scanners are fetched unconditionally (the server rate-limits each
  // to its own cadence), so turning a master toggle on shows data immediately
  // instead of waiting for the next poll, and the commodity picker is always
  // populated regardless of the master toggle.
  try {
    const m = await API.movers();
    if (ml && cfg.moversOn && !cfg.manualStrikesEnabled) {
      ml.style.display = "block";
      const chip = (r) =>
        `<span style="display:inline-flex;align-items:center;gap:3px;background:#16163a;border:1px solid #2d2d50;border-radius:8px;padding:1px 4px;margin:1px">` +
        `<span>${esc(symName(r.securityId))} (${num(r.changePct).toFixed(2)}%)</span>` +
        `<button data-sc-remove="${num(r.securityId)}" data-sc-name="${esc(symName(r.securityId))}" title="Remove - engine will not pick/trade this" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u00d7</button></span>`;
      const fmt = (rows) => (rows || []).map(chip).join(" ") || "--";
      ml.innerHTML =
        `<b style="color:#00d4aa">Gainers</b> ${fmt(m && m.gainers)}<br>` +
        `<b style="color:#ef5350">Losers</b> ${fmt(m && m.losers)}<br>` +
        `<span style="color:#888">Bias: ${m && m.bias > 0 ? "gainers lead -> CE" : m && m.bias < 0 ? "losers lead -> PE" : "balanced"}` +
        ` (up ${(m && m.up) || 0} / down ${(m && m.down) || 0})</span>`;
      ml.querySelectorAll("[data-sc-remove]").forEach((b) => {
        b.onclick = (e) => {
          e.preventDefault();
          removeScannerPick(num(b.getAttribute("data-sc-remove")), b.getAttribute("data-sc-name"));
        };
      });
    } else if (ml) {
      ml.style.display = "none";
    }

    const t = await API.trend();
    if (tl && cfg.niftyTrendOn && !cfg.manualStrikesEnabled) {
      tl.style.display = "block";
      const dirTxt =
        t && t.dir
          ? `<b class="${t.dir > 0 ? "rt-pos" : "rt-neg"}">${t.dir > 0 ? "BULLISH (CE)" : "BEARISH (PE)"}</b>`
          : "neutral / computing... (needs Dhan connection + 35 candles)";
      const picks = (t && t.picks) || [];
      const pfmt = picks
        .map(
          (p) =>
            `<span style="display:inline-flex;align-items:center;gap:3px;background:#16163a;border:1px solid #2d2d50;border-radius:8px;padding:1px 4px;margin:1px">` +
            `<span>${esc(symName(p.securityId))} (${num(p.changePct).toFixed(2)}%)</span>` +
            `<button data-sc-remove="${num(p.securityId)}" data-sc-name="${esc(symName(p.securityId))}" title="Remove - engine will not pick/trade this" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u00d7</button></span>`
        )
        .join(" ");
      tl.innerHTML =
        `Direction: ${dirTxt}<br>` +
        `<span style="color:#888">Picks (${picks.length}):</span> ${pfmt || "(none qualify yet)"}`;
      tl.querySelectorAll("[data-sc-remove]").forEach((b) => {
        b.onclick = (e) => {
          e.preventDefault();
          removeScannerPick(num(b.getAttribute("data-sc-remove")), b.getAttribute("data-sc-name"));
        };
      });
    } else if (tl) {
      tl.style.display = "none";
    }

    const c = await API.commodities();
    populateCommodityAdd(c);
    if (cl) {
      if (cfg.commodityOn) {
        const selected = (cfg.commodityList || []).map(num).filter((x) => x > 0);
        const byId = {};
        ((c && c.commodities) || []).forEach((x) => (byId[num(x.security_id)] = x.name));
        const names = selected.length ? selected.map((id) => byId[id] || id) : [];
        cl.textContent = names.length
          ? "ON - " + names.join(", ")
          : "ON - trading the default MCX futures set (add specific ids to narrow)";
      } else {
        cl.textContent = "Add MCX commodity futures to trade them directly.";
      }
    }
  } catch (e) {
    /* scanner readout is best-effort */
    console.warn("loadScanners failed", e);
  }
}

function populateCommodityAdd(c) {
  const rows = (c && c.commodities) || [];
  rows.forEach((x) => {
    COMM_NAMES[num(x.security_id)] = x.name || x.symbol || String(x.security_id);
  });
  const sel = document.getElementById("rtCommodityAdd");
  if (sel && !commodityPopulated && rows.length) {
    sel.innerHTML =
      `<option value="">-- pick commodity --</option>` +
      rows.map((x) => `<option value="${esc(x.security_id)}">${esc(x.name)} · ${esc(x.symbol)}</option>`).join("");
    commodityPopulated = true;
  }
  renderChips("commodityList", "rtCommodityChips", "commodity");
}

// ---- AST dropdown pickers (Indices / Confirm indicators / Commodities) -------
function indexName(id) {
  const s = CATALOG.symbols.find((x) => num(x.id) === num(id));
  if (s) return s.name;
  if (COMM_NAMES[num(id)]) return COMM_NAMES[num(id)];
  return String(id);
}
function indicatorName(id) {
  const i = (CATALOG.indicators || []).find((x) => x.id === id);
  return i ? i.name || i.id : id;
}
function chipLabel(kind, id) {
  if (kind === "indicator") return indicatorName(id);
  return indexName(id);
}
function chipHTML(key, id, kind) {
  return `<span style="display:inline-flex;align-items:center;gap:4px;background:#16163a;border:1px solid #2d2d50;border-radius:8px;padding:1px 4px;margin:1px">${esc(chipLabel(kind, id))}<button data-chip-key="${esc(key)}" data-chip-id="${esc(String(id))}" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u00d7</button></span>`;
}
function listValues(key) {
  const inp = document.querySelector(`#tab-realtime [data-list="${key}"]`);
  return inp ? String(inp.value || "").split(",").map((x) => x.trim()).filter(Boolean) : [];
}
function renderChips(key, containerId, kind) {
  // Index chips carry their own CE/PE leg selector, so they render specially.
  if (kind === "index") { renderIndexChips(); return; }
  const box = document.getElementById(containerId);
  if (!box) return;
  const vals = listValues(key);
  box.innerHTML = vals.length
    ? vals.map((v) => chipHTML(key, v, kind)).join("")
    : `<span style="color:#666">None added.</span>`;
  box.querySelectorAll("[data-chip-id]").forEach((b) => {
    b.onclick = (e) => {
      e.preventDefault();
      const id = b.getAttribute("data-chip-id");
      const inp = document.querySelector(`#tab-realtime [data-list="${key}"]`);
      if (!inp) return;
      inp.value = vals.filter((x) => x !== id).join(", ");
      API.settings(settingsFromDom()).then(() => renderChips(key, containerId, kind));
    };
  });
}
// ---- Top Movers "Indices" chips + per-index CE/PE leg selector --------------
// An added index trades ONLY the leg the operator picks here (CE = bullish view,
// PE = bearish view). With no leg chosen the index is skipped by the engine -
// index trading needs an explicit trend call, never an automatic side.
function indexLegValue(id) {
  const arr = (STATE && STATE.settings && STATE.settings.moversIndexLegs) || [];
  const hit = arr.find((x) => num(x && x.securityId) === num(id));
  const s = hit ? String(hit.side || "").toUpperCase() : "";
  return s === "CE" || s === "PE" ? s : "";
}
function setIndexLeg(id, side) {
  const cur = (((STATE && STATE.settings && STATE.settings.moversIndexLegs) || []).slice())
    .map((x) => ({ securityId: num(x && x.securityId), side: String((x && x.side) || "") }));
  const i = cur.findIndex((x) => x.securityId === num(id));
  if (side === "CE" || side === "PE") {
    if (i >= 0) cur[i] = { securityId: num(id), side: side };
    else cur.push({ securityId: num(id), side: side });
  } else if (i >= 0) {
    cur.splice(i, 1);
  }
  STATE.settings = Object.assign({}, (STATE && STATE.settings) || {}, { moversIndexLegs: cur });
  API.settings(settingsFromDom()).then(refresh);
}
function renderIndexChips() {
  const box = document.getElementById("rtMoversIndicesList");
  if (!box) return;
  const vals = listValues("moversIndices");
  if (!vals.length) {
    box.innerHTML = `<span style="color:#666">None added.</span>`;
    return;
  }
  box.innerHTML = vals.map((v) => {
    const leg = indexLegValue(v);
    const opts = [["", "leg: none (skip)"], ["CE", "CE"], ["PE", "PE"]]
      .map(([o, label]) => `<option value="${o}"${o === leg ? " selected" : ""}>${label}</option>`)
      .join("");
    const border = leg ? "#2d6b50" : "#6b3a3a";
    return `<span style="display:inline-flex;align-items:center;gap:4px;background:#16163a;border:1px solid ${border};border-radius:8px;padding:1px 4px;margin:1px">${esc(indexName(v))}`
      + `<select data-idx-leg="${esc(String(v))}" title="Leg this index trades. CE = bullish view, PE = bearish view. No leg = the index is skipped (index trading needs an explicit trend call)." style="background:#0e1626;color:#00d4aa;border:1px solid #2d2d50;border-radius:4px;font-size:9px;font-weight:700;padding:1px 2px">${opts}</select>`
      + `<button data-chip-key="moversIndices" data-chip-id="${esc(String(v))}" title="Remove this index" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u00d7</button></span>`;
  }).join("");
  box.querySelectorAll("[data-idx-leg]").forEach((sel) => {
    sel.onchange = (e) => {
      e.stopPropagation();
      setIndexLeg(sel.getAttribute("data-idx-leg"), sel.value);
    };
  });
  box.querySelectorAll("[data-chip-id]").forEach((b) => {
    b.onclick = (e) => {
      e.preventDefault();
      const id = b.getAttribute("data-chip-id");
      const inp = document.querySelector(`#tab-realtime [data-list="moversIndices"]`);
      if (!inp) return;
      inp.value = listValues("moversIndices").filter((x) => x !== id).join(", ");
      // Drop the removed index's assigned leg too, then persist both edits.
      const cur = (((STATE && STATE.settings && STATE.settings.moversIndexLegs) || []).slice())
        .filter((x) => num(x && x.securityId) !== num(id));
      STATE.settings = Object.assign({}, (STATE && STATE.settings) || {}, { moversIndexLegs: cur });
      API.settings(settingsFromDom()).then(() => renderIndexChips());
    };
  });
}
function renderAllChips() {
  renderChips("moversIndices", "rtMoversIndicesList", "index");
  renderChips("niftyTrendConfInds", "rtNiftyTrendConfIndList", "indicator");
  renderChips("commodityList", "rtCommodityChips", "commodity");
  renderManualStrikes();
}

// ---------------------------------------------------------------------------
// Manual Strike Select (testing only): pick an underlying + strike by hand and
// the engine resolves its option leg to that contract instead of the automatic
// ATM selection. Instrument + strike lists come from the engine-agnostic
// /api/instruments + /api/manual_strikes/chain endpoints; the picked list is a
// normal setting, so the real and paper tabs keep independent lists.
// ---------------------------------------------------------------------------
let MANUAL_INSTRUMENTS = null;
let MANUAL_CHAIN = null;
let MANUAL_LOADING = false;

function manualOptKey(sec) {
  return String(sec.security_id) + "|" + String(sec.exchange_segment || "");
}

function manualFindInstrument(key) {
  if (!key || !MANUAL_INSTRUMENTS) return null;
  const d = MANUAL_INSTRUMENTS.data || {};
  const all = [].concat(d.indices || [], d.fno || [], d.commodities || []);
  return all.find((it) => manualOptKey(it) === key) || null;
}

function manualFillInstruments() {
  const sel = document.getElementById("rtManualInstrument");
  if (!sel) return;
  const keep = sel.value;
  const d = (MANUAL_INSTRUMENTS && MANUAL_INSTRUMENTS.data) || {};
  const mk = (arr) => (arr || []).map((it) => `<option value="${esc(manualOptKey(it))}">${esc(it.name)}</option>`).join("");
  let html = '<option value="">-- pick instrument --</option>';
  if ((d.indices || []).length) html += '<optgroup label="Indices">' + mk(d.indices) + "</optgroup>";
  if ((d.fno || []).length) html += '<optgroup label="F&amp;O Stocks">' + mk(d.fno) + "</optgroup>";
  if ((d.commodities || []).length) html += '<optgroup label="Commodities (MCX)">' + mk(d.commodities) + "</optgroup>";
  sel.innerHTML = html;
  if (keep) sel.value = keep;
}

async function manualLoadInstruments(force) {
  const sel = document.getElementById("rtManualInstrument");
  if (!sel) return;
  if (MANUAL_INSTRUMENTS && !force) {
    manualFillInstruments();
    return;
  }
  if (MANUAL_LOADING) return;
  MANUAL_LOADING = true;
  sel.innerHTML = '<option value="">-- loading --</option>';
  try {
    MANUAL_INSTRUMENTS = await get("/api/instruments");
  } catch (e) {
    MANUAL_LOADING = false;
    sel.innerHTML = '<option value="">-- load failed --</option>';
    return;
  }
  MANUAL_LOADING = false;
  manualFillInstruments();
}

function manualFillStrikes() {
  const sel = document.getElementById("rtManualStrikePick");
  if (!sel) return;
  const rows = (MANUAL_CHAIN && MANUAL_CHAIN.data) || [];
  if (!rows.length) {
    sel.innerHTML = '<option value="">-- no strikes --</option>';
    return;
  }
  sel.innerHTML = rows
    .map((r) => {
      const k = num(r.Strike);
      const ce = num(r["CE LTP"]);
      const pe = num(r["PE LTP"]);
      const label = k + "  CE " + (ce > 0 ? ce.toFixed(2) : "-") + "  PE " + (pe > 0 ? pe.toFixed(2) : "-");
      return `<option value="${k}">${esc(label)}</option>`;
    })
    .join("");
}

async function manualLoadChain() {
  const instKey = (document.getElementById("rtManualInstrument") || {}).value || "";
  const strikeSel = document.getElementById("rtManualStrikePick");
  const expSel = document.getElementById("rtManualExpiry");
  const status = document.getElementById("rtManualStrikeStatus");
  const inst = manualFindInstrument(instKey);
  MANUAL_CHAIN = null;
  if (!inst) {
    if (strikeSel) strikeSel.innerHTML = "";
    if (expSel) expSel.innerHTML = "";
    if (status) status.textContent = "";
    return;
  }
  if (strikeSel) strikeSel.innerHTML = '<option value="">-- loading strikes --</option>';
  if (status) status.textContent = "loading strikes...";
  const wantExpiry = expSel ? expSel.value : "";
  // Best-effort spot so the strike window centres on the live price. Without a
  // session this stays 0 and the backend centres on the middle of the ladder.
  let spot = 0;
  try {
    const q = await post("/api/quotes", {
      securities: [{ security_id: inst.security_id, exchange_segment: inst.exchange_segment }],
    });
    const key = inst.exchange_segment === "IDX_I" ? "IDX_I:" + inst.security_id : String(inst.security_id);
    spot = num(q && q.data && q.data[key] ? q.data[key].ltp : 0);
  } catch (e) {
    spot = 0;
  }
  let j;
  try {
    j = await post("/api/manual_strikes/chain", {
      security_id: inst.security_id,
      exchange_segment: inst.exchange_segment,
      symbol_name: inst.name,
      expiry: wantExpiry,
      spot: spot,
    });
  } catch (e) {
    if (status) status.textContent = "chain load failed";
    return;
  }
  if (!j || j.status === "loading") {
    if (status) status.textContent = "scrip master loading - retrying...";
    setTimeout(manualLoadChain, 1500);
    return;
  }
  MANUAL_CHAIN = j;
  if (expSel) {
    const exps = MANUAL_CHAIN.expiries || [];
    expSel.innerHTML = exps.map((e) => `<option value="${esc(e)}">${esc(e)}</option>`).join("");
    if (MANUAL_CHAIN.expiry) expSel.value = MANUAL_CHAIN.expiry;
  }
  manualFillStrikes();
  if (status) status.textContent = (MANUAL_CHAIN.count || 0) + " strikes (spot " + num(MANUAL_CHAIN.spot).toFixed(2) + ")";
}

function manualStrikeList() {
  return (STATE && STATE.settings && STATE.settings.manualStrikes) || [];
}

function renderManualStrikes() {
  const host = document.getElementById("rtManualStrikeList");
  if (!host) return;
  const rows = manualStrikeList();
  if (!rows.length) {
    host.innerHTML = '<span style="color:#666">No manual strikes added yet.</span>';
    return;
  }
  host.innerHTML = rows
    .map((m, i) => {
      const col = String(m.optionType).toUpperCase() === "CE" ? "#00d4aa" : "#ef5350";
      return (
        `<span style="display:inline-flex;align-items:center;gap:4px;background:#12122a;border:1px solid #2d2d50;border-radius:4px;padding:2px 6px">` +
        `<b style="color:${col}">${esc(m.symbolName)} ${esc(m.expiry)} ${num(m.strike)} ${esc(m.optionType)}</b>` +
        `<button data-ms-remove="${i}" title="Remove" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:12px;padding:0 2px;line-height:1">\u00d7</button>` +
        `</span>`
      );
    })
    .join("");
  host.querySelectorAll("[data-ms-remove]").forEach((b) => {
    b.onclick = (e) => {
      e.preventDefault();
      const arr = manualStrikeList().slice();
      arr.splice(num(b.getAttribute("data-ms-remove")), 1);
      API.settings({ manualStrikes: arr }).then(refresh);
    };
  });
}

function addManualStrike() {
  const status = document.getElementById("rtManualStrikeStatus");
  const inst = manualFindInstrument((document.getElementById("rtManualInstrument") || {}).value || "");
  if (!inst || !MANUAL_CHAIN) {
    if (status) status.textContent = "pick an instrument first";
    return;
  }
  const strike = num((document.getElementById("rtManualStrikePick") || {}).value);
  const side = (document.getElementById("rtManualSide") || {}).value || "both";
  const row = (MANUAL_CHAIN.data || []).find((r) => Math.abs(num(r.Strike) - strike) < 0.001);
  if (!row) {
    if (status) status.textContent = "pick a strike first";
    return;
  }
  const list = manualStrikeList().slice();
  const sides = side === "both" ? ["CE", "PE"] : [side];
  let added = 0;
  for (const s of sides) {
    if (num(row[s + " SID"]) <= 0) continue;
    const dup = list.some(
      (m) =>
        m.symbolName === inst.name &&
        m.expiry === MANUAL_CHAIN.expiry &&
        Math.abs(num(m.strike) - strike) < 0.001 &&
        String(m.optionType).toUpperCase() === s
    );
    if (dup) continue;
    list.push({
      symbolName: inst.name,
      securityId: inst.security_id,
      exchangeSegment: inst.exchange_segment,
      expiry: MANUAL_CHAIN.expiry,
      strike: strike,
      optionType: s,
      lot: num(MANUAL_CHAIN.lot_size),
    });
    added++;
  }
  if (!added) {
    if (status) status.textContent = "already added";
    return;
  }
  if (list.length > 20) {
    if (status) status.textContent = "max 20 strikes";
    return;
  }
  if (status) status.textContent = "added " + added;
  API.settings({ manualStrikes: list }).then(refresh);
}

// "Picked Strikes": the CE/PE option contracts the engine resolved for the live
// Top Movers / NIFTY trend (and MCX) picks. Fed by the snapshot `strikes` array.
function renderStrikes() {
  const host = document.getElementById("rtStrikesList");
  if (!host) return;
  const cfg = (STATE && STATE.settings) || {};
  const rows = (STATE && STATE.strikes) || [];
  const manualOn = !!cfg.manualStrikesEnabled;
  const note = document.getElementById("rtStrikesNote");
  if (note) {
    note.textContent = manualOn
      ? "Manual Strike Select ON - only these manual contracts run/trade; Top Movers & NIFTY Trend are inactive"
      : "Option strikes the engine picked to execute (auto-shown while Top Movers / NIFTY Trend Follow is ON)";
  }
  if (!manualOn && !cfg.niftyTrendOn && !cfg.moversOn && !cfg.commodityOn) {
    host.style.display = "none";
    return;
  }
  host.style.display = "block";
  if (!rows.length) {
    host.innerHTML = manualOn
      ? `<span style="color:#666">Manual Strike Select ON - add strikes in the Manual Strike list above; only these will run/trade.</span>`
      : `<span style="color:#666">No option strikes resolved yet - waiting for live Dhan quotes on the Top Movers / NIFTY Trend picks.</span>`;
    renderScannerRemoved();
    return;
  }
  host.innerHTML = rows
    .map((r) => {
      const bull = String(r.side || "").toUpperCase() === "CE";
      const col = bull ? "#00d4aa" : "#ef5350";
      const uid = num(r.underlyingSecurityId);
      const isManual = manualOn || String(r.source || "") === "Manual";
      const rm = !isManual && uid > 0
        ? `<button data-sc-remove="${uid}" data-sc-name="${esc(r.underlying)}" title="Remove - engine will not pick/trade this" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:12px;padding:0 3px;line-height:1">\u00d7</button>`
        : "";
      const spot = num(r.spot) > 0
        ? `<span style="color:#888">spot ${num(r.spot).toFixed(2)} &middot; ${num(r.changePct).toFixed(2)}%</span>`
        : "";
      return (
        `<div style="display:flex;gap:8px;align-items:center;padding:1px 0">` +
        `<span style="color:${col};font-weight:700;min-width:70px">${esc(r.underlying)} ${esc(r.side)}</span>` +
        `<span style="color:#fff">${esc(r.tradingSymbol)}</span>` +
        spot +
        `<span style="color:#555;margin-left:auto">${esc(r.source || "")}</span>` +
        rm +
        `</div>`
      );
    })
    .join("");
  host.querySelectorAll("[data-sc-remove]").forEach((b) => {
    b.onclick = (e) => {
      e.preventDefault();
      removeScannerPick(num(b.getAttribute("data-sc-remove")), b.getAttribute("data-sc-name"));
    };
  });
  renderScannerRemoved();
}

// ---- Scanner "remove" picks (operator blacklist) ---------------------------
// The old app's removeMoverIndex / removeNiftyTrendIndex, but server-enforced:
// the excluded underlying is dropped from scan_universe, so it can never be
// ranked, resolved to an option leg or traded until restored. Persisted in
// settings.scannerExclude so it survives restarts and both tabs stay in sync.
function scannerExcludedIds() {
  const cfg = (STATE && STATE.settings) || {};
  return (cfg.scannerExclude || []).map(num).filter((x) => x > 0);
}
function renderScannerRemoved() {
  const row = document.getElementById("rtScannerRemovedRow");
  const box = document.getElementById("rtScannerRemovedList");
  const inp = document.getElementById("rtScannerExcludeList");
  if (!row || !box) return;
  const ids = scannerExcludedIds();
  if (inp) inp.value = ids.join(", ");
  if (!ids.length) {
    row.style.display = "none";
    box.innerHTML = "";
    return;
  }
  row.style.display = "block";
  box.innerHTML = ids
    .map(
      (id) =>
        `<span style="display:inline-flex;align-items:center;gap:4px;background:#2a1414;border:1px solid #5a2222;border-radius:8px;padding:1px 4px;margin:1px">${esc(indexName(id))}` +
        `<button data-sc-restore="${id}" title="Restore - engine will pick it again" style="background:none;border:none;color:#00d4aa;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u21ba</button></span>`
    )
    .join("");
  box.querySelectorAll("[data-sc-restore]").forEach((b) => {
    b.onclick = (e) => {
      e.preventDefault();
      restoreScannerPick(num(b.getAttribute("data-sc-restore")));
    };
  });
}
function setScannerExclude(ids) {
  const uniq = [];
  (ids || []).map(num).forEach((x) => {
    if (x > 0 && uniq.indexOf(x) < 0) uniq.push(x);
  });
  if (STATE.settings) STATE.settings.scannerExclude = uniq;
  const inp = document.getElementById("rtScannerExcludeList");
  if (inp) inp.value = uniq.join(", ");
  renderScannerRemoved();
  return API.settings({ scannerExclude: uniq });
}
function removeScannerPick(id, name) {
  id = num(id);
  if (!(id > 0)) return;
  const ids = scannerExcludedIds();
  if (ids.indexOf(id) >= 0) return;
  ids.push(id);
  setScannerExclude(ids);
  if (typeof linkToast === "function") linkToast("ok", `${name || indexName(id)} scanner picks se hata diya`);
}
function restoreScannerPick(id) {
  setScannerExclude(scannerExcludedIds().filter((x) => x !== num(id)));
}
function populateAstSelects() {
  if (astSelectsPopulated) return;
  const fill = (selId, rows, labelFn) => {
    const sel = document.getElementById(selId);
    if (!sel) return;
    const cur = sel.value;
    sel.innerHTML = `<option value="">-- pick --</option>` + rows.map((r) => `<option value="${esc(r.v)}">${esc(labelFn(r))}</option>`).join("");
    if (cur) sel.value = cur;
  };
  const indices = CATALOG.symbols
    .filter((s) => String(s.inst || "").toUpperCase() === "INDEX" || String(s.exch || "").includes("IDX_I"))
    .map((s) => ({ v: s.id, s }));
  const inds = (CATALOG.indicators || []).map((i) => ({ v: i.id, i }));
  if (!indices.length && !inds.length) return;
  fill("rtMoversIndicesSelect", indices, (r) => `${r.s.name} (${r.v})`);
  const straightInds = inds.filter((r) => STRAIGHT_LINE_IDS.includes(String(r.v)));
  fill("rtNiftyTrendConfIndSelect", straightInds, (r) => `${r.i.name || r.i.id}${r.i.cat ? " · " + r.i.cat : ""}`);
  astSelectsPopulated = true;
  renderAllChips();
}
function wirePicker(selectId, addId, key, listId, kind) {
  const sel = document.getElementById(selectId);
  const add = document.getElementById(addId);
  if (!add) return;
  add.onclick = () => {
    const v = sel && sel.value;
    if (!v) return;
    const inp = document.querySelector(`#tab-realtime [data-list="${key}"]`);
    if (!inp) return;
    const vals = listValues(key);
    if (vals.indexOf(v) < 0) vals.push(v);
    inp.value = vals.join(", ");
    API.settings(settingsFromDom()).then(() => renderChips(key, listId, kind));
  };
}
function wireAstPickers() {
  wirePicker("rtMoversIndicesSelect", "rtMoversIndicesAdd", "moversIndices", "rtMoversIndicesList", "index");
  wirePicker("rtNiftyTrendConfIndSelect", "rtNiftyTrendConfIndAdd", "niftyTrendConfInds", "rtNiftyTrendConfIndList", "indicator");
  wireAssign("rtTplMoverBull", "rtAssignMoverBull", "moverBullTemplate", "bullish");
  wireAssign("rtTplMoverBear", "rtAssignMoverBear", "moverBearTemplate", "bearish");
  renderAllChips();
}

// ---------------------------------------------------------------------------
// Interlock / fade logic (ported from the old app's aismart.js sync*UI helpers)
//
// Selecting one option fades out (disables + dims) the mutually-exclusive one:
//   - AI Stop-Loss  <->  Manual Overall SL / Trail SL
//   - AI Trail TP   <->  Manual Trail TP
//   - AI TP %       <->  Manual Trail TP
//   - Risk:Reward   ->   all TP controls
//   - Pick modes, strike count, premium lock, NIFTY-Trend/Top-Movers...
// A faded control is .disabled + pointerEvents:none, exactly like the old app.
// ---------------------------------------------------------------------------
function fadeControl(el, on) {
  if (!el) return;
  el.disabled = !!on;
  el.style.opacity = on ? "0.35" : "1";
  el.style.pointerEvents = on ? "none" : "";
  el.style.cursor = on ? "not-allowed" : "";
  const lbl = el.closest ? el.closest("label") : null;
  // Dim the label for the visual cue only when it is dedicated to this control.
  // When the label also holds a sibling control (e.g. an enable checkbox plus a
  // value input like "Set TP by Risk:Reward [x] <value>" or the Trade-times
  // rows), dimming the label would grey out - and pointer-lock - the sibling
  // toggle, so that whole section looked faded and could not be switched back
  // on. The control itself is already disabled + dimmed above.
  if (lbl && lbl.querySelectorAll("input, select, button").length <= 1) {
    lbl.style.opacity = on ? "0.35" : "1";
  } else if (lbl) {
    lbl.style.opacity = "1";
  }
}
function dimControl(el, on) {
  if (!el) return;
  el.disabled = !!on;
  el.style.opacity = on ? "0.5" : "1";
}

function renderRiskStatus() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const rrEl = q("rrEnabled");
  const rrOn = !!(rrEl && rrEl.checked);
  const rrValEl = q("rrValue");
  const rr = Number(rrValEl && rrValEl.value) > 0 ? Number(rrValEl.value) : 2;
  const rrBox = document.getElementById("rtRrStatus");
  if (rrBox) {
    const slChk = document.getElementById("rtEngSl");
    const slPctEl = document.getElementById("rtEngSlPct");
    const slPct = slChk && slChk.checked ? Number(slPctEl && slPctEl.value) || 0 : 0;
    if (!rrOn) {
      rrBox.style.color = "#666";
      rrBox.textContent = "RR off - TP from AI TP% / Manual Trail TP";
    } else if (slPct > 0) {
      rrBox.style.color = "#00d4aa";
      rrBox.textContent = "RR " + rr + "x SL " + slPct + "% => TP target " + (rr * slPct).toFixed(2) + "% (overrides TP + AI TP%)";
    } else {
      rrBox.style.color = "#ffb74d";
      rrBox.textContent = "RR " + rr + "x SL distance (AI/ATR stop) - target set live per trade";
    }
  }
}

function renderTradesStatus() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const box = document.getElementById("rtTradesStatus");
  if (!box) return;
  const tlEl = q("tradeLimit");
  const aiEl = q("aiTrades");
  const cntEl = q("tradeLimitCount");
  const tlOn = !!(tlEl && tlEl.checked);
  const aiOn = !!(aiEl && aiEl.checked);
  const count = Number(cntEl && cntEl.value) || 0;
  if (aiOn) {
    box.style.color = "#7c9cff";
    box.textContent = "AI auto trades ON - per-strategy cap ignored (unlimited entries)";
  } else if (tlOn && count > 0) {
    box.style.color = "#00d4aa";
    box.textContent = "Max " + count + " trades per strategy (counted across open + closed entries)";
  } else if (tlOn) {
    box.style.color = "#ffb74d";
    box.textContent = "Max trades ON but count is 0 - no cap applied, set a positive number";
  } else {
    box.style.color = "#666";
    box.textContent = "No per-strategy trade limit";
  }
}

function renderOpsStatus() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const box = document.getElementById("rtOpsStatus");
  if (!box) return;
  const el = q("orderPerSec");
  const ops = Math.max(1, Math.min(30, Math.round(Number(el && el.value) || 6)));
  box.style.color = "#00d4aa";
  box.textContent = " | Max " + ops + " orders/sec - har entry is cap ke andar hi bhejega";
}

function renderTimeStatus() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const box = document.getElementById("rtTimeStatus");
  if (!box) return;
  const on = (k) => !!(q(k) && q(k).checked);
  const val = (k) => { const e = q(k); return e && e.value ? e.value : ""; };
  const parts = [];
  if (on("startAfterEnabled")) parts.push("entries open at " + val("startAfter"));
  if (on("noTradeAfterEnabled")) parts.push("no new entries after " + val("noTradeAfter"));
  if (on("autoSquareOffEnabled")) parts.push("auto square-off all at " + val("autoSquareOffTime"));
  const ses = tradeSessions().filter((x) => x.enabled && tsParseTime(x.start) != null && tsParseTime(x.end) != null && tsParseTime(x.start) < tsParseTime(x.end));
  if (ses.length) parts.push("sessions " + ses.map((x) => tsFmt12(tsParseTime(x.start)) + "-" + tsFmt12(tsParseTime(x.end))).join(", "));
  box.style.color = parts.length ? "#ffb74d" : "#666";
  box.textContent = parts.length ? " | " + parts.join("  ·  ") : " | No trade-time gate - entries run all session";
}

// ---- Multi-session trade windows (Trade times -> Trading sessions) ----------
// The operator can add several intraday windows (e.g. 09:30-10:30 and
// 14:00-15:14). The server gate (`sessions_gate_ok`) is authoritative: entries
// only fire while the IST minute is inside an enabled, well-formed window.
// This row is a pure editor that persists the list in settings.tradeSessions.
function tsISTMinutes() {
  const now = new Date();
  const ist = new Date(now.getTime() + (330 + now.getTimezoneOffset()) * 60000);
  return ist.getHours() * 60 + ist.getMinutes();
}
// Accepts both 24h ("09:30", "14:00") and 12h with AM/PM ("9:30 AM", "02:00 PM",
// "2 PM"). Returns minutes past midnight, or null. Storage always stays 24h
// "HH:MM" (the server parses that); AM/PM is purely the display layer.
function tsParseTime(s) {
  const t = String(s || "").trim().toUpperCase().replace(/\s+/g, " ");
  const m = /^(\d{1,2}):?(\d{2})?\s*(AM|PM)?$/.exec(t);
  if (!m) return null;
  let h = +m[1];
  const mi = m[2] == null ? 0 : +m[2];
  const ap = m[3];
  if (mi > 59) return null;
  if (ap) {
    if (h < 1 || h > 12) return null;
    if (h === 12) h = 0;
    if (ap === "PM") h += 12;
  } else if (h > 23) {
    return null;
  }
  return h * 60 + mi;
}
// Minutes -> "09:30 AM" / "02:00 PM" / "12:00 PM".
function tsFmt12(mins) {
  if (mins == null || isNaN(mins)) return "";
  mins = ((Math.round(mins) % 1440) + 1440) % 1440;
  const h24 = (mins / 60) | 0;
  const mi = mins % 60;
  const ap = h24 >= 12 ? "PM" : "AM";
  let h = h24 % 12;
  if (h === 0) h = 12;
  return String(h).padStart(2, "0") + ":" + String(mi).padStart(2, "0") + " " + ap;
}
// Minutes -> stored 24h "HH:MM".
function tsFmtHHMM(mins) {
  mins = ((Math.round(mins) % 1440) + 1440) % 1440;
  return String((mins / 60) | 0).padStart(2, "0") + ":" + String(mins % 60).padStart(2, "0");
}
function tradeSessions() {
  const s = (STATE && STATE.settings) || {};
  if (!Array.isArray(s.tradeSessions)) return [];
  return s.tradeSessions.map((x) => ({
    start: (x && x.start) || "",
    end: (x && x.end) || "",
    enabled: !x || x.enabled !== false,
  }));
}
function persistTradeSessions(list) {
  if (STATE.settings) STATE.settings.tradeSessions = list;
  renderTradeSessions(true);
  renderTimeStatus();
  return API.settings({ tradeSessions: list });
}
function renderTradeSessions(force) {
  const host = document.getElementById("rtTradeSessionsList");
  if (!host) return;
  // Never yank the DOM out from under an in-progress edit (the 1s snapshot
  // refresh calls this); a focused time input / checkbox would lose focus.
  // Our own edits pass force=true so add/remove still repaint immediately.
  if (!force && host.contains(document.activeElement)) return;
  const list = tradeSessions();
  const nowM = tsISTMinutes();
  const validOf = (s) => {
    const a = tsParseTime(s.start), b = tsParseTime(s.end);
    return s.enabled && a != null && b != null && a < b;
  };
  host.innerHTML = list.length
    ? list
        .map((s, i) => {
          const a = tsParseTime(s.start), b = tsParseTime(s.end);
          const bad = a == null || b == null || a >= b;
          const live = validOf(s) && nowM >= a && nowM <= b;
          return (
            `<span style="display:inline-flex;align-items:center;gap:4px;background:#16163a;border:1px solid ${live ? "#00d4aa" : bad ? "#5a2222" : "#2d2d50"};border-radius:6px;padding:2px 6px">` +
            `<input type="text" data-ts-start="${i}" value="${esc(tsFmt12(a))}" list="rtSessTimes" autocomplete="off" style="width:86px;font-size:10px;text-align:center">` +
            `<span style="color:#888;font-size:9px">to</span>` +
            `<input type="text" data-ts-end="${i}" value="${esc(tsFmt12(b))}" list="rtSessTimes" autocomplete="off" style="width:86px;font-size:10px;text-align:center">` +
            `<label style="font-size:9px;color:#aaa"><input type="checkbox" data-ts-on="${i}" ${s.enabled ? "checked" : ""}> on</label>` +
            `<button type="button" data-ts-del="${i}" title="Remove session" style="background:none;border:none;color:#ef5350;cursor:pointer;font-size:12px;padding:0 2px;line-height:1">\u00d7</button>` +
            (live ? `<span style="color:#00d4aa;font-size:8px;font-weight:700">LIVE</span>` : bad && s.enabled ? `<span style="color:#ef5350;font-size:8px">invalid</span>` : "") +
            `</span>`
          );
        })
        .join("")
    : `<span style="color:#666">No session added - entries run any time (subject to Start/No-trade gate).</span>`;
  const box = document.getElementById("rtSessionStatus");
  if (box) {
    const valid = list.filter(validOf);
    box.style.color = valid.length ? "#ffb74d" : "#666";
    box.textContent = valid.length
      ? ` | entries only inside ${valid.map((s) => tsFmt12(tsParseTime(s.start)) + " to " + tsFmt12(tsParseTime(s.end))).join(", ")}`
      : "";
  }
  const applyEdit = (i, field, raw) => {
    const l = tradeSessions();
    if (!l[i]) return;
    const m = tsParseTime(raw);
    if (m == null) {
      uiAlert("Valid time dein (e.g. 09:30 AM ya 14:00).");
      renderTradeSessions(true);
      return;
    }
    l[i][field] = tsFmtHHMM(m);
    persistTradeSessions(l);
  };
  host.querySelectorAll("[data-ts-start]").forEach((el) => {
    el.onchange = () => applyEdit(+el.getAttribute("data-ts-start"), "start", el.value);
    el.onkeydown = (e) => { if (e.key === "Enter") el.blur(); };
  });
  host.querySelectorAll("[data-ts-end]").forEach((el) => {
    el.onchange = () => applyEdit(+el.getAttribute("data-ts-end"), "end", el.value);
    el.onkeydown = (e) => { if (e.key === "Enter") el.blur(); };
  });
  host.querySelectorAll("[data-ts-on]").forEach((el) => {
    el.onchange = () => {
      const l = tradeSessions();
      const i = +el.getAttribute("data-ts-on");
      if (!l[i]) return;
      l[i].enabled = el.checked;
      persistTradeSessions(l);
    };
  });
  host.querySelectorAll("[data-ts-del]").forEach((el) => {
    el.onclick = (e) => {
      e.preventDefault();
      const l = tradeSessions();
      l.splice(+el.getAttribute("data-ts-del"), 1);
      persistTradeSessions(l);
    };
  });
  syncTradeSessionInterlock();
}
// When at least one enabled, well-formed session exists it becomes the single
// authority for entry timing, so the old single "Start trading after / No trade
// after" controls are disabled (greyed, non-interactive) to avoid a confusing
// double gate. Removing every session re-enables them exactly as before.
function syncTradeSessionInterlock() {
  const active = tradeSessions().some((s) => {
    const a = tsParseTime(s.start), b = tsParseTime(s.end);
    return s.enabled && a != null && b != null && a < b;
  });
  const row = document.querySelector('#tab-realtime [data-set="startAfterEnabled"]');
  const holder = row && row.closest(".rt-engine-row");
  ["startAfterEnabled", "startAfter", "noTradeAfterEnabled", "noTradeAfter"].forEach((k) => {
    const el = document.querySelector('#tab-realtime [data-set="' + k + '"]');
    if (!el) return;
    el.disabled = active;
    el.style.pointerEvents = active ? "none" : "";
    if (active) el.style.opacity = "0.3";
  });
  // Not active: hand opacity back to the engine's own interlock so an unticked
  // Start/No-trade box fades exactly as before.
  if (!active && typeof syncInterlocks === "function") syncInterlocks();
  if (holder) holder.title = active
    ? "Trading sessions active hain - ye single Start/No-trade gate disable hai. Sab sessions hatane par wapas on ho jayega."
    : "";
}
function addTradeSession() {
  const a = document.getElementById("rtSessionStart");
  const b = document.getElementById("rtSessionEnd");
  const rawA = (a && a.value) || "";
  const rawB = (b && b.value) || "";
  const sa = tsParseTime(rawA), sb = tsParseTime(rawB);
  if (sa == null || sb == null) {
    uiAlert("Session start aur end time valid dein (e.g. 09:30 AM / 02:00 PM).");
    return;
  }
  if (sa >= sb) {
    uiAlert("Invalid session: start < end hona chahiye (e.g. 09:30 AM to 10:30 AM).");
    return;
  }
  const start = tsFmtHHMM(sa), end = tsFmtHHMM(sb);
  const l = tradeSessions();
  if (l.some((s) => tsParseTime(s.start) === sa && tsParseTime(s.end) === sb)) {
    uiAlert("Ye session already added hai.");
    return;
  }
  l.push({ start, end, enabled: true });
  l.sort((x, y) => (tsParseTime(x.start) || 0) - (tsParseTime(y.start) || 0));
  persistTradeSessions(l);
  if (a) a.value = tsFmt12(sb);
  if (b) b.value = tsFmt12(sb + 30);
}
function wireTradeSessions() {
  const btn = document.getElementById("rtSessionAdd");
  if (btn) btn.onclick = addTradeSession;
  const a = document.getElementById("rtSessionStart");
  const b = document.getElementById("rtSessionEnd");
  const bump = () => {
    const t = tsParseTime(a && a.value);
    if (t != null && b) b.value = tsFmt12(t + 60);
  };
  if (a) a.onchange = bump;
  if (b) b.onkeydown = (e) => { if (e.key === "Enter") addTradeSession(); };
  renderTradeSessions();
}

function renderNiftyStatus() {
  const box = document.getElementById("rtNiftyStatus");
  if (!box) return;
  const nt = (STATE && STATE.niftyTrend) || {};
  const s = (STATE && STATE.settings) || {};
  const on = !!(s.niftyTrendOn || nt.on);
  const tf = nt.tf || s.niftyTf || "5min";
  const tfLabel = tf === "both" ? "1min+5min" : tf.replace("min", " min");
  if (!on) {
    box.style.color = "#666";
    box.textContent = "Trend follow OFF";
    return;
  }
  const dir = nt.dir === 1 ? "BULLISH" : nt.dir === -1 ? "BEARISH" : "neutral";
  box.style.color = nt.dir === 1 ? "#00d4aa" : nt.dir === -1 ? "#ef5350" : "#ffd700";
  box.textContent = "Ensemble " + tfLabel + " -> " + dir;
}

function renderRoutingStatus() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const box = document.getElementById("rtRoutingStatus");
  if (!box) return;
  const val = (k) => { const e = q(k); return e && e.value ? e.value : ""; };
  const checked = (k) => !!(q(k) && q(k).checked);
  const runLabel = (v) => (v === "both" ? "Both" : v === "premium" ? "Premium" : "Spot");
  if (checked("premiumOnly")) {
    box.style.color = "#ffd700";
    box.textContent = "Premium chart only ON - every instrument runs AND trades on the option premium chart (run-in / trade-in choices locked)";
    return;
  }
  const run = "Run: indices " + runLabel(val("runIndex")) + ", F&O " + runLabel(val("runFno")) + ", commodities " + runLabel(val("runComm"));
  const tradeIdx = val("tradeInIndex") === "premium" ? "Premium" : val("tradeInIndex") === "both" ? "Both" : "Spot";
  const tradeComm = val("tradeInComm") === "premium" ? "Option premium" : "Futures contract";
  const trade = "Trade: indices " + tradeIdx + ", F&O Premium, commodities " + tradeComm;
  const saved = [];
  if (checked("runInDefault")) saved.push("run-in");
  if (checked("tradeInDefault")) saved.push("trade-in");
  box.style.color = saved.length ? "#00d4aa" : "#888";
  box.textContent = run + "  ·  " + trade + (saved.length ? "  (" + saved.join(" + ") + " saved as default for all future tasks)" : "");
}

// Run Strategy In (Selected Strategies side override). Mirrors the old AST
// section: the manual CE/PE dropdown is live only while Run-in is ON and Auto
// Select Mode is OFF; when Auto Select Mode is ON it is faded + disabled (the
// engine picks the side from the NIFTY trend / top gainer-loser). The Auto
// Select Mode box stays clickable even when faded so the user can switch back.
function renderRunInStatus() {
  const en = document.getElementById("rtRunInEnabled");
  const side = document.getElementById("rtRunInSide");
  const auto = document.getElementById("rtRunInAutoCb");
  const status = document.getElementById("rtRunInStatus");
  const enabled = !!(en && en.checked);
  const autoOn = !!(auto && auto.checked);
  const manual = (side && side.value) || "CE";
  // Live side the engine resolved this pass (server `activeRunInSide`) plus the
  // auto source (`autoSide`), so Auto Select Mode shows the CE/PE it actually
  // picked instead of only the static manual fallback.
  const s = (STATE && STATE.settings) || {};
  const live = (STATE && STATE.activeRunInSide) || "";
  const autoLive = (STATE && STATE.autoSide) || "";
  const nt = (STATE && STATE.niftyTrend) || {};
  const mv = (STATE && STATE.movers) || {};
  // Both straight-line sets assigned: a bullish line trades the Top Gainer (CE)
  // leg and a bearish line the Top Loser (PE) leg at the same time, so the
  // status must show both legs together instead of a single resolved side.
  const niftyBoth = !!(s.niftyTrendOn && nt.bothActive);
  let src = "";
  if (autoLive) {
    if (s.niftyTrendOn && nt.dir) src = "NIFTY trend";
    else if (s.moversOn && mv.bias) src = "Top Movers";
  }
  if (side) {
    const locked = autoOn;
    side.disabled = locked;
    side.style.opacity = locked ? "0.45" : "1";
    side.style.pointerEvents = locked ? "none" : "";
    const wrap = side.closest("label");
    if (wrap) wrap.style.opacity = locked ? "0.45" : "1";
  }
  if (auto) {
    // Dim (but keep clickable) whenever the manual side is the effective mode
    // or run-in is off, exactly like the old app.
    auto.style.opacity = enabled && autoOn ? "1" : "0.45";
  }
  if (status) {
    if (!enabled) {
      status.style.color = "#666";
      status.textContent = "off";
    } else if (autoOn && niftyBoth) {
      status.style.color = "#00d4aa";
      status.textContent =
        "auto: BOTH legs CE + PE - NIFTY trend (bull line -> Top Gainers / bear line -> Top Losers, each on its own side)";
    } else if (autoOn) {
      status.style.color = autoLive ? "#00d4aa" : "#66ccff";
      status.textContent = autoLive
        ? "auto: " + autoLive + (src ? " (" + src + ")" : " (live)") + " - manual " + manual + " if no signal"
        : "auto: " + manual + " (manual side if no signal)";
    } else {
      status.style.color = "#ffd700";
      status.textContent = "manual: " + manual;
    }
  }
}

function syncInterlocks() {
  const q = (k) => document.querySelector('#tab-realtime [data-set="' + k + '"]');
  const checked = (k) => { const e = q(k); return !!(e && e.checked); };
  const val = (k) => { const e = q(k); return e ? e.value : ""; };
  const cq = (sel) => document.querySelector("#tab-realtime " + sel);
  const setv = (id, on) => {
    const row = document.getElementById(id);
    if (!row) return;
    row.style.opacity = on ? "0.35" : "1";
    row.style.pointerEvents = on ? "none" : "auto";
    row.querySelectorAll("select, input").forEach((x) => (x.disabled = on));
  };

  // --- AI Stop-Loss <-> Manual Overall SL / Trail SL ---
  const slChk = cq("#rtEngSl");
  const trailChk = cq("#rtEngTrailSl");
  const pointTrailChk = cq("#rtEngPointTrailSl");
  const manualSlOn = !!(slChk && slChk.checked);
  const manualTrailSlOn = !!(trailChk && trailChk.checked);
  const manualPointTrailSlOn = !!(pointTrailChk && pointTrailChk.checked);
  const aiSlOn = checked("aiSl") && !manualSlOn && !manualTrailSlOn && !manualPointTrailSlOn;
  fadeControl(q("aiSl"), manualSlOn || manualTrailSlOn || manualPointTrailSlOn);
  fadeControl(slChk, aiSlOn);
  fadeControl(trailChk, aiSlOn);
  fadeControl(pointTrailChk, aiSlOn);
  fadeControl(cq("#rtEngSlPct"), aiSlOn || !manualSlOn);
  fadeControl(cq("#rtEngTrailSlPct"), aiSlOn || !manualTrailSlOn);
  fadeControl(cq("#rtEngPointTrailSlPts"), aiSlOn || !manualPointTrailSlOn);

  // --- AI TP% <-> Manual Trail TP (two take-profit mechanisms) ---
  const manualTrailTpOn = checked("manualTrailTp");
  const aiTpPctOn = checked("aiTpPct");
  fadeControl(q("manualTrailTp"), aiTpPctOn);
  fadeControl(q("manualTrailTpPct"), aiTpPctOn || !manualTrailTpOn);
  fadeControl(q("aiTpPct"), manualTrailTpOn);

  // --- Risk:Reward overrides every TP control ---
  const rrOn = checked("rrEnabled");
  fadeControl(q("rrValue"), !rrOn);
  if (rrOn) {
    fadeControl(q("aiTpPct"), true);
    fadeControl(q("manualTrailTp"), true);
    fadeControl(q("manualTrailTpPct"), true);
  }
  renderRiskStatus();

  // --- Order rate (orders per second cap) ---
  renderOpsStatus();

  // --- Trade times: the time input only lives while its checkbox is on ---
  fadeControl(q("startAfter"), !checked("startAfterEnabled"));
  fadeControl(q("noTradeAfter"), !checked("noTradeAfterEnabled"));
  fadeControl(q("autoSquareOffTime"), !checked("autoSquareOffEnabled"));
  renderTimeStatus();
  renderRunInStatus();
  renderNiftyStatus();
  renderRoutingStatus();

  // --- Trades per strategy ---
  fadeControl(q("tradeLimitCount"), !(checked("tradeLimit") && !checked("aiTrades")));
  renderTradesStatus();

  // --- Strike pool: ATM / fastest-rising fade the normal count; fastest-rising
  //     is a REFINEMENT of "+green only" (biggest riser among the rising
  //     premiums), so it no longer fades/disable it - both work together.
  //     Manual Strike Select (testing) fades the whole automatic pool. ---
  const manualOn = checked("manualStrikesEnabled");
  const atm = val("strikeMode") === "atm";
  const fastOn = checked("fastestRising");
  const cnt = q("strikeCount");
  if (cnt) {
    const dis = atm || fastOn || manualOn;
    cnt.disabled = dis;
    cnt.style.opacity = dis ? "0.35" : "1";
    cnt.style.pointerEvents = dis ? "none" : "";
    const w = cnt.closest("label");
    if (w) { w.style.opacity = dis ? "0.5" : "1"; w.style.pointerEvents = dis ? "none" : ""; }
  }
  const fc = q("fastestCount");
  if (fc) {
    const on = fastOn && !manualOn;
    fc.disabled = !on;
    fc.style.opacity = on ? "1" : "0.35";
    fc.style.pointerEvents = on ? "" : "none";
    const w = fc.closest("label");
    if (w) { w.style.opacity = on ? "1" : "0.5"; w.style.pointerEvents = on ? "" : "none"; }
  }
  const pos = q("onlyPositive");
  if (pos) {
    const dis = manualOn;
    pos.disabled = dis;
    pos.style.opacity = dis ? "0.5" : "1";
    pos.style.pointerEvents = dis ? "none" : "";
    const w = pos.closest("label");
    if (w) { w.style.opacity = dis ? "0.6" : "1"; w.style.pointerEvents = dis ? "none" : ""; }
  }
  // --- Opposite-side fallback: needs +green on AND a single-sided "Execute
  //     Trade In" mode (both-sided / ATM-only have no opposite side). Works the
  //     same with fastest-rising on (both share the direction + fallback). ---
  const fb = q("fallbackOppositeSide");
  if (fb) {
    const mdl = val("strikeMode");
    const singleSide = mdl === "above" || mdl === "above_atm" || mdl === "below" || mdl === "below_atm";
    const dis = manualOn || !checked("onlyPositive") || !singleSide;
    fb.disabled = dis;
    fb.style.opacity = dis ? "0.5" : "1";
    fb.style.pointerEvents = dis ? "none" : "";
    const w = fb.closest("label");
    if (w) { w.style.opacity = dis ? "0.6" : "1"; w.style.pointerEvents = dis ? "none" : ""; }
  }
  fadeControl(q("optionSide"), manualOn);
  fadeControl(q("strikeMode"), manualOn);
  fadeControl(q("fastestRising"), manualOn);
  const msBox = document.getElementById("rtManualStrikeBox");
  if (msBox) msBox.style.display = manualOn ? "flex" : "none";
  const msHint = document.getElementById("rtManualStrikeHint");
  if (msHint) msHint.style.opacity = manualOn ? "1" : "0.6";
  if (manualOn && !MANUAL_INSTRUMENTS) manualLoadInstruments(false);
  // Manual Strike Select freezes the automatic scanner masters (Top Movers /
  // NIFTY Trend / Commodities). Their toggle label + disabled state is set in
  // syncAstToggles; here we dim the whole row so an inactive universe reads as
  // inactive. Their persisted settings are untouched and resume when the mode
  // is switched off.
  ["rtMoversToggle", "rtNiftyTrendToggle", "rtCommodityToggle"].forEach((id) => {
    const b = document.getElementById(id);
    const row = b && b.closest ? b.closest(".rt-engine-row") : null;
    if (row) row.style.opacity = manualOn ? "0.45" : "1";
  });

  // --- Premium-only lock: the run-in / trade-in selectors are engine-pinned ---
  const prem = checked("premiumOnly");
  setv("rtRunInRow", prem);
  setv("rtTradeInRow", prem);

  // --- NIFTY trend-following: the confirmation-indicator picker is live only
  //     while the trend mode is active. ---
  const s = (STATE && STATE.settings) || {};
  const trendEnabled = !!s.niftyTrendOn;
  const ntActive = trendEnabled;
  const dimRow = (id, on) => {
    const row = document.getElementById(id);
    if (row) {
      row.disabled = !!on;
      row.style.opacity = on ? "0.4" : "1";
      row.style.pointerEvents = on ? "none" : "";
    }
  };
  dimRow("rtNiftyTrendConfIndSelect", !ntActive);
  dimRow("rtNiftyTrendConfIndAdd", !ntActive);

  // --- Top Movers: independent of NIFTY trend-following. Both masters can be
  //     ON together and run side by side (they are connected), so there is no
  //     fade-out between them any more - only the controls for a scanner that
  //     is itself OFF are dimmed. ---
  const mvActive = !!s.moversOn;
  ["moversGainers", "moversLosers"].forEach((k) => dimControl(q(k), !mvActive));
  dimRow("rtMoversIndicesSelect", !mvActive);
  dimRow("rtMoversIndicesAdd", !mvActive);
  const mvChips = document.getElementById("rtMoversIndicesList");
  if (mvChips) mvChips.style.opacity = mvActive ? "1" : "0.5";

  // --- Indicator filter sections: a side with nothing selected is dimmed, and
  //     the section master auto-arms when any sub-filter is ticked. ---
  ["Bull", "Bear"].forEach((side) => {
    const sec = document.getElementById("rtFilterSection" + side);
    if (!sec) return;
    const master = sec.querySelector("[data-master]");
    const subs = Array.from(sec.querySelectorAll("[data-filter]"));
    let on = !!(master && master.checked) || subs.some((b) => b.checked);
    if (master) master.checked = on;
    sec.style.opacity = on ? "1" : "0.45";
  });

  // --- Manual filter list goes inactive while a saved AST template owns the
  //     active Top Movers direction (the template supplies the filters). ---
  const hasTpl = (k) => String(s[k] || "").trim() !== "";
  const listLocked = !!s.moversOn && (hasTpl("moverBullTemplate") || hasTpl("moverBearTemplate"));
  const frow = document.getElementById("rtFilterRow");
  if (frow) {
    frow.style.opacity = listLocked ? "0.6" : "";
    frow.querySelectorAll("input[type=checkbox], select, button").forEach((c) => (c.disabled = listLocked));
    frow.title = listLocked
      ? "Manual indicator filters inactive: a saved AST template is assigned to the active Top Movers / NIFTY trend direction and supplies the filters."
      : "";
  }
}

function applySettingsToDom(s) {
  if (!s) return;
  const ae = document.activeElement;
  const busy = (k) => ae && ae.hasAttribute && ((ae.getAttribute("data-set") === k) || (ae.getAttribute("data-list") === k));
  document.querySelectorAll("#tab-realtime [data-set]").forEach((inp) => {
    const k = inp.getAttribute("data-set");
    if (!(k in s)) return;
    if (busy(k)) return;
    if (inp.type === "checkbox") inp.checked = !!s[k];
    else inp.value = s[k] == null ? "" : s[k];
  });
  const intLists = ["moversIndices", "commodityList", "scannerExclude"];
  document.querySelectorAll("#tab-realtime [data-list]").forEach((inp) => {
    const k = inp.getAttribute("data-list");
    if (!(k in s)) return;
    if (busy(k)) return;
    const arr = s[k];
    inp.value = Array.isArray(arr) ? arr.join(intLists.indexOf(k) >= 0 ? ", " : ", ") : (arr || "");
  });
  const filters = s.filters || {};
  // While a filter toggle is still saving, keep the user's ticks untouched: a
  // snapshot fetched before the click would otherwise silently uncheck them.
  if (!filtersBusy)
    document.querySelectorAll("#tab-realtime [data-filter]").forEach((cb) => {
      const k = cb.getAttribute("data-filter");
      if (k in filters) cb.checked = !!filters[k];
    });
  syncAstToggles(s);
  applyDataPoolUi(!!s.data_pool);
  applyEngineScanUI(s);
  // Strike-selection base must stay ON (see strikeBaseNormalized). A legacy
  // snapshot with either flag off is corrected once, with a notice, so the
  // "Execute Trade In" dropdown always runs on the +green base.
  if (!strikeBaseNormalized && (s.onlyPositive === false || s.fallbackOppositeSide === false)) {
    strikeBaseNormalized = true;
    ["onlyPositive", "fallbackOppositeSide"].forEach((k) => {
      const e = document.querySelector('#tab-realtime [data-set="' + k + '"]');
      if (e) e.checked = true;
    });
    uiAlert(
      '"Only +green premium strikes" aur "Opposite side fallback (if all minus)" ON rakhna zaroori hai - "Execute Trade In" ke sare dropdown options isi base par kaam karte hain. Dono enable kar diye gaye hain.'
    );
    pushSetting({ onlyPositive: true, fallbackOppositeSide: true });
  }
  syncInterlocks();
  applyRunModeUI();
  renderAllChips();
  renderTemplateAssignment();
  renderScannerRemoved();
  renderTradeSessions();
}

// Reflect engine toggles (Top Movers / NIFTY Trend / Commodities)
// on their button labels, matching the old AST master buttons.
function syncAstToggles(s) {
  const manualOn = !!s.manualStrikesEnabled;
  const set = (id, on, label, blocked) => {
    const b = document.getElementById(id);
    if (!b) return;
    b.textContent = label;
    b.disabled = !!blocked;
    b.style.pointerEvents = blocked ? "none" : "";
    b.style.cursor = blocked ? "not-allowed" : "";
    b.classList.toggle("on", !blocked && on);
    b.classList.toggle("off", !blocked && !on);
    b.classList.toggle("blocked", !!blocked);
    b.style.background = "";
    b.style.color = "";
    b.style.opacity = blocked ? "0.5" : "1";
  };
  // Manual Strike Select makes every automatic scanner universe inactive, so the
  // three master toggles are frozen (their persisted state is kept and resumes
  // the moment the mode is switched off).
  const suffix = manualOn ? " (inactive)" : "";
  const trendOn = !!s.niftyTrendOn;
  const commOn = !!s.commodityOn;
  set(
    "rtMoversToggle",
    !!s.moversOn,
    "Top Movers: " + (s.moversOn ? "ON" : "OFF") + suffix,
    manualOn
  );
  set("rtNiftyTrendToggle", trendOn, "Trend Follow: " + (trendOn ? "ON" : "OFF") + suffix, manualOn);
  set("rtCommodityToggle", commOn, "Commodities: " + (commOn ? "ON" : "OFF") + suffix, manualOn);
}

function filterIsBull(k) {
  if (String(k).startsWith("Bull")) return true;
  if (String(k).startsWith("Bear")) return false;
  return String(k).toUpperCase().includes("UP");
}

// AI Smart margin bar (above Running Trades): budget, locked by open trades and
// the amount still available. The engine blocks + warns the next trade whose
// required margin exceeds the available amount.
function renderMarginBar(m) {
  const bar = document.getElementById("rtMarginBar");
  if (!bar) return;
  const budget = num(m && m.budget);
  const locked = num(m && m.locked);
  const count = m && m.count != null ? num(m.count) : 0;
  const avail = m && m.available != null ? num(m.available) : Math.max(0, budget - locked);
  const pct = m && m.pct != null ? num(m.pct) : null;
  const configured = !!(m && m.configured) || budget > 0 || count > 0;
  if (!configured) {
    bar.style.display = "none";
    bar.innerHTML = "";
    return;
  }
  bar.style.display = "flex";
  const availCol = avail > 0 ? "#00d4aa" : "#ef5350";
  bar.innerHTML =
    '<b style="color:#00d4aa;white-space:nowrap">AI Smart trade margin:</b>' +
    '<b style="color:#ffd700;font-size:11px;white-space:nowrap">' + fmtMoney(budget) + "</b>" +
    (pct != null ? '<span style="color:#666;white-space:nowrap">(' + pct + "% of available balance)</span>" : "") +
    '<span style="color:#888;white-space:nowrap">Locked by ' + count + " running trade" + (count === 1 ? "" : "s") + ': <b style="color:#ffd700">' + fmtMoney(locked) + "</b></span>" +
    '<span style="color:#888;white-space:nowrap">Available now: <b style="color:' + availCol + '">' + fmtMoney(avail) + "</b></span>" +
    '<span style="color:#666;font-size:8px">next AI Smart/manual trade is blocked + warned when its required margin &gt; available</span>';
}

// Old-app "Open Chart": focus the main chart on this trade's contract and let
// the parent window's overlay poller draw the live P&L / SL / trail-SL lines on
// the candles. The Paper Trade pane runs inside an iframe, so it forwards the
// request to the parent instead of trying to chart inside the frame.
function openChartForTrade(info) {
  const payload = {
    type: "openTradeChart",
    securityId: Number(info.securityId || info.sid || 0),
    exchangeSegment: info.exchangeSegment || info.exch || "",
    instrument: info.instrument || info.inst || "",
    tradingSymbol: info.tradingSymbol || info.label || info.name || "",
    paper: !!window.__PAPER__,
  };
  if (!payload.securityId) return;
  if (window.parent && window.parent !== window) {
    try { window.parent.postMessage(payload, "*"); } catch (_) {}
  }
  if (typeof window.openTradeChart === "function") {
    try { window.openTradeChart(payload); } catch (_) {}
  }
}

// Chart routing for one running strategy: the chart the engine evaluates the
// entry on (`runMode`) and the chart the order executes on (`tradeMode`), with
// the resolved premium leg for each. `spot` uses the strategy's own instrument;
// `premium` / `both` use the engine-resolved option contract, so the Running
// Strategies view can show - and open - the exact chart each strategy runs on.
function strategyChartInfo(x, leg, mode) {
  const label =
    mode === "premium" ? "Premium" : mode === "both" ? "Spot+Premium" : mode === "futures" ? "Futures" : "Spot";
  // The server publishes the exact run leg for the mode (underlying spot when
  // run mode is spot, option premium when premium/both). Prefer it whenever it
  // is present so an option-instrument strategy does not show its own premium
  // chart while labelled "Spot".
  const useLeg = !!leg;
  const info = useLeg
    ? { securityId: leg.securityId, segment: leg.segment, instrument: leg.instrument, tradingSymbol: leg.tradingSymbol }
    : {
        securityId: x.securityId,
        segment: x.exchangeSegment,
        instrument: x.instrument,
        tradingSymbol: x.tradingSymbol || x.name || String(x.securityId || ""),
      };
  return { label, info, resolved: !!info.securityId };
}

function chartBadgeHTML(prefix, chart) {
  if (!chart || !chart.resolved) {
    return '<span style="color:#666">' + prefix + ": resolving\u2026</span>";
  }
  const i = chart.info;
  const btn =
    '<button class="btn-action" data-openchart="' + esc(String(i.securityId || "")) +
    '" data-exch="' + esc(i.segment || "") +
    '" data-inst="' + esc(i.instrument || "") +
    '" data-sym="' + esc(i.tradingSymbol || "") +
    '" style="width:auto;padding:0 5px;margin:0 0 0 4px;font-size:9px" title="Is chart ko kholo">Chart</button>';
  return (
    '<span style="color:#888">' + prefix + ":</span> " +
    '<b style="color:#8ab4ff">' + esc(chart.label) + "</b> " +
    '<span style="color:#ccc">' + esc(i.tradingSymbol || i.securityId) + "</span>" + btn
  );
}

function renderRunning(s) {
  const positions = s.positions || [];
  const broker = s.brokerPositions || [];
  const settings = s.settings || {};
  const sel = s.selected || {};
  // The engine's run set (matches scan_signals()): saved strategies that are
  // enabled, OR the manually ticked run set while "Call manually selected
  // strategies" is on, OR the live AI-trader picks while it is on.
  const callManual = settings.callManual !== false;
  const aiPicks = new Set(s.aiPicks || []);
  const running = (s.strategies || []).filter((x) => x.enabled || (callManual && sel[x.id]) || aiPicks.has(x.id));
  const modeLabel = "Real (live)";
  const armedFilters = Object.keys(settings.filters || {}).filter((k) => settings.filters[k]);
  const filtersBull = armedFilters.filter(filterIsBull).length;
  const filtersBear = armedFilters.length - filtersBull;
  const bullNames = armedFilters.filter(filterIsBull);
  const bearNames = armedFilters.filter((k) => !filterIsBull(k));
  const sideWord = (bull) => (bull ? "bullish" : "bearish");
  const sideCol = (bull) => (bull ? "#00d4aa" : "#ef5350");
  // Per-running-strategy detail line: Overall SL / Trail SL / Lot size / Lot qty.
  // Live values come from the open position; while waiting, the configured plan
  // (from settings) is shown so the operator always sees what will be applied.
  const fmtLot = (v) => (num(v) > 0 ? String(num(v)) : "auto");
  const trailLabel = (pct, pts) => {
    const a = [];
    if (num(pct) > 0) a.push(num(pct).toFixed(2) + "%");
    if (num(pts) > 0) a.push(num(pts).toFixed(2) + " pts");
    return a.length ? a.join(" + ") : "-";
  };
  const plannedDetail = () => {
    const ov = settings.manualSl ? num(settings.manualSlPct).toFixed(2) + "%" : "-";
    const tr = trailLabel(
      settings.manualTrailSl ? settings.manualTrailSlPct : 0,
      settings.manualPointTrailSl ? settings.manualPointTrailSlPoints : 0
    );
    const lots = num(settings.lots) > 0 ? num(settings.lots) : 1;
    return `Overall SL ${ov} · Trail SL ${tr} · Lot size ${fmtLot(settings.lotSize)} · Lots ${lots}`;
  };
  const posDetail = (p) => {
    const ov = num(p.sl) > 0 ? num(p.sl).toFixed(2) : settings.manualSl ? num(settings.manualSlPct).toFixed(2) + "%" : "-";
    const lots = num(p.lots) > 0 ? num(p.lots) : 0;
    return `Overall SL ${ov} · Trail SL ${trailLabel(p.trail, p.pointTrail)} · Lot size ${fmtLot(p.lotSize)} · Qty ${num(p.qty)}${lots ? ` (${lots} lot${lots === 1 ? "" : "s"})` : ""}`;
  };
  // Indicator-filters run mode trades the scanner universe (Top Movers / NIFTY
  // trend / Commodities) with synthetic strategies that never live in
  // `s.strategies`, so surface the struck contracts here: picked option strike +
  // the ticked indicator filters that gate that side.
  const strikes = s.strikes || [];
  const manualMode = !!settings.manualStrikesEnabled;
  const scannerOn = !!(
    settings.filterMode ||
    settings.moversOn ||
    settings.niftyTrendOn ||
    settings.commodityOn ||
    manualMode
  );

  const runStrat = document.getElementById("rtRunStrategies");
  if (runStrat) {
    const shown = running.length || !scannerOn ? running.length : strikes.length;
    const head =
      `<div style="display:flex;justify-content:space-between;gap:6px;font-size:9px;color:#888;padding:0 0 3px">` +
      `<span>${s.engineOn ? '<span style="color:#00d4aa">Engine RUNNING</span>' : '<span style="color:#ffd700">Engine OFF</span>'} · ${shown} strateg${shown === 1 ? "y" : "ies"} · ${esc(modeLabel)}</span>` +
      `<span>${s.armed ? '<span style="color:#ef5350">ARMED</span>' : '<span style="color:#666">disarmed</span>'}${armedFilters.length ? ` · filters ${filtersBull}CE/${filtersBear}PE` : ""}</span>` +
      `</div>`;
    const filtName = (bull) => (bull ? bullNames : bearNames);
    if (running.length) {
      runStrat.innerHTML =
        head +
        running
          .map((x) => {
            const bull = stratBull(x);
            const pos = positions.filter((p) => (p.strategyId && p.strategyId === x.id) || (p.strategyName && x.name && p.strategyName === x.name));
            const pnl = pos.reduce((a, p) => a + num(p.pnl), 0);
            const filt = filtName(bull);
            const names = filt.slice(0, 4).join(", ") + (filt.length > 4 ? ` +${filt.length - 4}` : "");
            const tail = pos.length
              ? `<span style="color:${num(pnl) >= 0 ? "#00d4aa" : "#ef5350"}">${pos.length} trade · ${num(pnl).toFixed(2)}</span>`
              : filt.length
              ? `<span style="color:#ffd700" title="${esc(filt.join(", "))}">waiting · ${filt.length} filter${names ? " (" + esc(names) + ")" : ""}</span>`
              : `<span style="color:#666">waiting</span>`;
            const runChart = strategyChartInfo(x, x.runLeg, x.runMode);
            const tradeChart = strategyChartInfo(x, x.tradeLeg, x.tradeMode);
            return `<div style="font-size:9px;border:1px solid #1e1e40;border-radius:3px;padding:2px 6px;margin:2px 0">
              <div style="display:flex;justify-content:space-between;gap:6px">
                <span><b style="color:#d0d0d0">${esc(x.name || x.id)}</b> <span style="color:${sideCol(bull)}">${sideWord(bull)}</span></span>
                <span style="color:#666">${esc(x.engineTf || x.timeframe || "")} · ${tail}</span>
              </div>
              <div style="display:flex;gap:12px;flex-wrap:wrap;padding:2px 0 0 2px;font-size:8px">
                ${chartBadgeHTML("Run chart", runChart)}
                ${chartBadgeHTML("Trade chart", tradeChart)}
              </div>
              <div style="font-size:8px;color:#8aa0c0;padding:2px 0 0 2px">${esc(pos.length ? posDetail(pos[0]) : plannedDetail())}</div></div>`;
          })
          .join("");
    } else if (scannerOn && strikes.length) {
      runStrat.innerHTML =
        head +
        strikes
          .map((r) => {
            const bull = String(r.side || "").toUpperCase() === "CE";
            const isManualRow = manualMode || String(r.source || "") === "Manual";
            const pos = positions.filter((p) => String(p.securityId) === String(r.securityId));
            const pnl = pos.reduce((a, p) => a + num(p.pnl), 0);
            const filt = filtName(bull);
            const names = filt.slice(0, 4).join(", ") + (filt.length > 4 ? ` +${filt.length - 4}` : "");
            const tail = pos.length
              ? `<span style="color:${num(pnl) >= 0 ? "#00d4aa" : "#ef5350"}">${pos.length} trade · ${num(pnl).toFixed(2)}</span>`
              : filt.length
              ? `<span style="color:#ffd700" title="${esc(filt.join(", "))}">waiting · ${filt.length} filter${names ? " (" + esc(names) + ")" : ""}</span>`
              : isManualRow
              ? `<span style="color:#888">manual strike · waiting</span>`
              : `<span style="color:#666">waiting</span>`;
            // The scanner pick carries both charts: the underlying it is
            // ANALYSED on (Run) and the option premium it TRADES on. Showing
            // both stops "Run-In: Spot" from looking like it runs on premium.
            const runMode = String(r.runMode || "");
            const runSpot = runMode === "spot" || runMode === "both";
            const underBtn =
              runSpot && r.underlyingSecurityId
                ? '<button class="btn-action" data-openchart="' + esc(String(r.underlyingSecurityId)) +
                  '" data-exch="' + esc(r.underlyingSegment || "") + '" data-inst="' + esc(r.underlyingInstrument || "") +
                  '" data-sym="' + esc(r.underlying || "") + '" style="width:auto;padding:0 5px;margin:0 0 0 4px;font-size:9px" title="Underlying spot chart (jis par strategy analyse hoti hai)">Chart</button>'
                : "";
            const runChip = runSpot
              ? '<span style="color:#888">Run:</span> <b style="color:#8ab4ff">Spot</b> <span style="color:#ccc">' + esc(r.underlying || "") + "</span>" + underBtn
              : '<span style="color:#888">Run:</span> <b style="color:#8ab4ff">' + esc(runMode === "premium" ? "Premium" : runMode || "-") + "</b>";
            const tradeBtn =
              '<button class="btn-action" data-openchart="' + esc(String(r.securityId || "")) +
              '" data-exch="' + esc(r.segment || r.exchangeSegment || "") + '" data-inst="' + esc(r.instrument || "") +
              '" data-sym="' + esc(r.tradingSymbol || "") + '" style="width:auto;padding:0 5px;margin:0 0 0 4px;font-size:9px" title="Trade (premium) chart">Chart</button>';
            const tradeChip =
              '<span style="color:#888">Trade:</span> <b style="color:#8ab4ff">Premium</b> <span style="color:#ccc">' + esc(r.tradingSymbol || "") + "</span>" + tradeBtn;
            return `<div style="font-size:9px;border:1px solid #1e1e40;border-radius:3px;padding:2px 6px;margin:2px 0">
              <div style="display:flex;justify-content:space-between;gap:6px">
                <span><b style="color:#d0d0d0">${esc(r.underlying || "")}</b> <span style="color:${sideCol(bull)}">${sideWord(bull)}</span> <span style="color:#666">${esc(r.source || "")}</span></span>
                <span>${tail}</span>
              </div>
              <div style="display:flex;gap:12px;flex-wrap:wrap;padding:2px 0 0 2px;font-size:8px">${runChip}${tradeChip}</div>
              <div style="font-size:8px;color:#8aa0c0;padding:2px 0 0 2px">${esc(pos.length ? posDetail(pos[0]) : plannedDetail())}</div></div>`;
          })
          .join("");
    } else {
      runStrat.innerHTML =
        head +
        `<div style="font-size:10px;color:#666;padding:2px 2px">Koi strategy running nahi. "Run Paper Trading" dabao, phir engine ON karo.</div>`;
    }
  }

  const runTrades = document.getElementById("rtRunTrades");
  if (runTrades) {
    // De-dupe: an engine position already mirrors its Dhan position, so only
    // show broker rows the engine is not tracking (e.g. manual Dhan trades).
    const engineBrokerSec = new Set(positions.filter((p) => p.broker).map((p) => String(p.securityId)));
    const dhanOnly = broker.filter((p) => !engineBrokerSec.has(String(p.securityId)));
    let total = 0;
    const pnlCell = (v) => {
      const cls = num(v) >= 0 ? "#00d4aa" : "#ef5350";
      return `<td style="color:${cls}">${num(v) >= 0 ? "+" : "-"}${fmtMoney(Math.abs(num(v)))}</td>`;
    };
    const rows = positions
      .map((p) => {
        const pnl = num(p.pnl);
        total += pnl;
        return `<tr>
          <td><b>${esc(p.tradingSymbol || p.securityId)}</b><br><span style="font-size:8px;color:#666">REAL · ${esc(p.method || "")}${p.broker ? " · broker" : ""}</span></td>
          <td>${num(p.qty)}</td>
          <td>${num(p.entry).toFixed(2)}</td>
          <td>${num(p.ltp).toFixed(2)}</td>
          ${pnlCell(pnl)}
          <td style="white-space:nowrap">
            <button class="btn-action" data-openchart="${esc(String(p.securityId || ""))}" data-exch="${esc(p.exchangeSegment || "")}" data-inst="${esc(p.instrument || "")}" data-sym="${esc(p.tradingSymbol || "")}" style="width:auto;padding:2px 8px;margin:0 4px 0 0;font-size:10px" title="Is trade ka chart kholo (live P&amp;L / SL / trail lines)">Chart</button>
            <button class="btn-action warn" data-runclose="${esc(p.id)}" style="width:auto;padding:2px 8px;margin:0;font-size:10px">Close</button>
          </td>
        </tr>`;
      })
      .concat(
        dhanOnly.map((p) => {
          const pnl = num(p.pnl);
          total += pnl;
          return `<tr>
            <td><b>${esc(p.tradingSymbol || p.securityId)}</b><br><span style="font-size:8px;color:#666">DHAN · ${esc(p.exchangeSegment || "")} · ${esc(p.positionType || "")}${p.productType ? " · " + esc(p.productType) : ""}</span></td>
            <td>${num(p.netQty)}</td>
            <td>${num(p.buyAvg).toFixed(2)}</td>
            <td>${num(p.ltp).toFixed(2)}</td>
            ${pnlCell(pnl)}
            <td></td>
          </tr>`;
        })
      )
      .join("");
    if (!rows) {
      runTrades.innerHTML = `<div style="font-size:10px;color:#666;padding:6px">Dhan me koi open position nahi hai.</div>`;
    } else {
      const totCol = total >= 0 ? "#00d4aa" : "#ef5350";
      runTrades.innerHTML =
        `<table class="account-table"><thead><tr><th>Option</th><th>Qty</th><th>Entry</th><th>LTP</th><th>P&amp;L</th><th></th></tr></thead><tbody>${rows}</tbody></table>` +
        `<div style="font-size:9px;color:#888;padding:3px 2px">Total gross P&amp;L: <b style="color:${totCol}">${total >= 0 ? "+" : "-"}${fmtMoney(Math.abs(total))}</b> · updated ${istTime(Date.now())}</div>`;
      runTrades.querySelectorAll("[data-runclose]").forEach((b) => {
        b.onclick = async () => {
          if (!(await uiConfirm("Close this position at market?", { danger: true }))) return;
          await API.close({ id: b.getAttribute("data-runclose") });
          refresh();
        };
      });
    }
  }

  const body = document.querySelector("#rtPos tbody");
  if (body) {
    body.innerHTML = positions
      .map((p) => {
        const pnlCls = num(p.pnl) >= 0 ? "rt-pos" : "rt-neg";
        const sl = p.sl ? `SL ${num(p.sl).toFixed(2)}` : "-";
        const tr = p.trail ? `Trail ${num(p.trail).toFixed(2)}%` : "";
        const g = [];
        if (num(p.sl) > 0) g.push("SL");
        if (num(p.tp) > 0) g.push("TP");
        if (num(p.trail) > 0) g.push("TRAIL");
        if (num(p.trailTp) > 0) g.push("TRAIL-TP");
        const guardTxt = g.length ? `<span style="color:#ffd700">${g.join("+")}</span>` : "--";
        return `<tr>
          <td>${esc(p.tradingSymbol || p.securityId)}</td>
          <td>${num(p.qty)}</td>
          <td>${num(p.entry).toFixed(2)}</td>
          <td>${num(p.ltp).toFixed(2)}</td>
          <td class="${pnlCls}">${num(p.pnl).toFixed(2)}</td>
          <td>${esc(sl)} ${tr ? " / " + esc(tr) : ""}</td>
          <td>${guardTxt}</td>
          <td style="white-space:nowrap">
            <button class="btn-action" data-openchart="${esc(String(p.securityId || ""))}" data-exch="${esc(p.exchangeSegment || "")}" data-inst="${esc(p.instrument || "")}" data-sym="${esc(p.tradingSymbol || "")}" style="width:auto;padding:2px 8px;margin:0 4px 0 0;font-size:10px" title="Is trade ka chart kholo (live P&amp;L / SL / trail lines)">Chart</button>
            <button class="btn-action warn" data-close="${esc(p.id)}" style="width:auto;padding:2px 8px;margin:0;font-size:10px">Close</button>
          </td>
        </tr>`;
      })
      .join("");
    body.querySelectorAll("[data-close]").forEach((b) => {
      b.onclick = async () => {
        if (!(await uiConfirm("Close this position at market?", { danger: true }))) return;
        await API.close({ id: b.getAttribute("data-close") });
        refresh();
      };
    });
  }

  wireOpenChartButtons();
}

// Attach the per-trade / per-strike "Open Chart" handlers. Called at the end of
// renderRunning once every running-trade / strike table has been rebuilt.
function wireOpenChartButtons() {
  document.querySelectorAll("[data-openchart]").forEach((b) => {
    b.onclick = () => openChartForTrade({
      securityId: b.getAttribute("data-openchart"),
      exchangeSegment: b.getAttribute("data-exch"),
      instrument: b.getAttribute("data-inst"),
      tradingSymbol: b.getAttribute("data-sym"),
    });
  });
}

function reasonLabel(reason) {
  const r = String(reason || "").toUpperCase();
  if (r === "STOP_LOSS") return "Overall SL";
  if (r === "TRAIL_SL") return "Trail SL";
  if (r === "TRAIL_TP") return "Trail TP";
  if (r === "TARGET") return "Target";
  return reason;
}

function renderClosed(s) {
  const body = document.querySelector("#rtClosed tbody");
  if (!body) return;
  const fmtT = (t) => (t ? istTime(num(t)) : "-");
  const on = chargesOn();
  // Prefer the full on-demand ledger; fall back to the snapshot's newest slice
  // before the first `/closed` fetch resolves.
  const list = CLOSED_CACHE && CLOSED_CACHE.length ? CLOSED_CACHE : ((s && s.closed) || []);
  const cnt = document.getElementById("rtClosedCount");
  if (cnt) cnt.textContent = "(" + list.length + ")";
  // Repaint only when the ledger (or the charges toggle) actually changed. A
  // multi-thousand-row innerHTML rebuild on every 1s poll is the main reason the
  // paper pane felt frozen; the rows themselves are immutable once booked.
  const sig =
    (CLOSED_CACHE && CLOSED_CACHE.length ? "c" : "s") +
    ":" + list.length +
    ":" + (list.length ? num(list[0].closedAt) : 0) +
    ":" + (list.length ? num(list[list.length - 1].closedAt) : 0) +
    ":" + (on ? "1" : "0");
  if (sig === closedRenderSig) return;
  closedRenderSig = sig;
  body.innerHTML = list
    .map((c) => {
      const charges = on ? (c.charges != null ? num(c.charges) : estimateCharges(c.entry, c.exit, c.qty, c.side, c.instrument, c.tradingSymbol)) : 0;
      const net = on && c.netPnl != null ? num(c.netPnl) : num(c.pnl);
      const cls = net >= 0 ? "rt-pos" : "rt-neg";
      return `<tr>
        <td>${esc(c.tradingSymbol || c.securityId)}</td>
        <td>${num(c.qty)}</td>
        <td>${num(c.entry).toFixed(2)} → ${num(c.exit).toFixed(2)}</td>
        <td class="${cls}">${net.toFixed(2)}</td>
        <td>${on ? charges.toFixed(2) : "--"}</td>
        <td>${esc(reasonLabel(c.reason))}</td>
        <td>${esc(fmtT(c.openedAt))} → ${esc(fmtT(c.closedAt))}</td>
      </tr>`;
    })
    .join("");
}

// The closed ledger and day-stats are cached client-side (the 1s poll only ships
// the newest slice). After a P&L reset the server book is empty, so drop the
// caches and force a repaint - otherwise the old rows keep showing.
function resetBookCaches() {
  CLOSED_CACHE = null;
  closedLoadedAt = 0;
  closedRenderSig = "";
  DAY_STATS = null;
  dayStatsAt = 0;
  dayStatsSig = "";
}

// "Deduct Dhan charges" toggle state. Real trades settle charges broker-side
// (net == gross), so only the paper engine honours the toggle; default on so a
// saved setting is only ever disabled explicitly.
function chargesOn() {
  if (!PAPER) return false;
  const s = STATE && STATE.settings;
  return !s || s.brokerCharges !== false;
}

// Dhan-style broker charge simulation, ported 1:1 from the Python paper-trade
// engine (`static/papertrade.js`). Rates are Dhan's published retail tariffs:
//   Delivery: brokerage Rs 0, STT 0.1% (buy+sell), NSE txn 0.0030699%,
//             SEBI 0.0001%, stamp 0.015% (buy), IPFT 0.0000001%, GST 18%
//   Intraday: brokerage Rs 20 or 0.03% (lower of the two), STT 0.025% (sell),
//             NSE txn 0.0030699%, SEBI 0.0001%, stamp 0.003% (buy),
//             IPFT 0.0000001%, GST 18%
//   Options:  brokerage Rs 20 / executed order, STT 0.0625% of premium (sell),
//             NSE txn 0.03503% of premium, SEBI 0.0001%, stamp 0.003% (buy),
//             IPFT 0.0000001%, GST 18%
//   Futures:  brokerage Rs 20 / executed order, STT 0.02% of turnover (sell),
//             NSE txn 0.00173% of turnover, SEBI 0.0001%, stamp 0.003% (buy),
//             IPFT 0.0000001%, GST 18%
// Rounding follows the Dhan contract-note rule: STT + stamp duty to the nearest
// rupee, every other component to 2 dp. Mirrors the Rust engine's
// `compute_charges_for_trade()` so the Closed Trades table, paper wallet and
// Paper Stats all settle to the same figure.
const CHARGES_CONFIG = {
  delivery: { brokerageFlat: 0, brokeragePct: 0, txnPct: 0.0030699, sttBuyPct: 0.1, sttSellPct: 0.1, sebiPct: 0.0001, stampBuyPct: 0.015, stampSellPct: 0, gstPct: 18, ipftPct: 0.0000001 },
  intraday: { brokerageFlat: 20, brokeragePct: 0.03, txnPct: 0.0030699, sttBuyPct: 0, sttSellPct: 0.025, sebiPct: 0.0001, stampBuyPct: 0.003, stampSellPct: 0, gstPct: 18, ipftPct: 0.0000001 },
  options: { brokerageFlat: 20, brokeragePct: 0, txnPct: 0.03503, sttBuyPct: 0, sttSellPct: 0.0625, sebiPct: 0.0001, stampBuyPct: 0.003, stampSellPct: 0, gstPct: 18, ipftPct: 0.0000001 },
  futures: { brokerageFlat: 20, brokeragePct: 0, txnPct: 0.00173, sttBuyPct: 0, sttSellPct: 0.02, sebiPct: 0.0001, stampBuyPct: 0.003, stampSellPct: 0, gstPct: 18, ipftPct: 0.0000001 },
};

// Charge segment for a position / symbol: OPT* -> options, FUT* -> futures,
// otherwise delivery (equity / index spot). Mirrors Python `segmentFor()`.
function chargesSegment(instrument, tradingSymbol) {
  const inst = String(instrument == null ? "" : instrument).toUpperCase();
  if (inst === "OPTIDX" || inst === "OPTSTK" || inst === "OPTFUT" || inst === "OPT") return "options";
  if (inst === "FUTIDX" || inst === "FUTSTK" || inst === "FUTCOM" || inst === "FUT") return "futures";
  const nm = String(tradingSymbol == null ? "" : tradingSymbol).toUpperCase();
  if (/(?:^|[^A-Z0-9])(?:CE|PE)(?:$|[^A-Z0-9])/.test(nm)) return "options";
  return "delivery";
}

// Per-side charge line for one order (turnover = qty x price). STT + stamp
// round to the nearest rupee; all other charges to 2 decimals (Dhan rule).
function sideCharges(segment, isBuy, turnover) {
  const c = CHARGES_CONFIG[segment] || CHARGES_CONFIG.delivery;
  const t = Math.abs(num(turnover));
  const r2 = (n) => Math.round(n * 100) / 100;
  let brokerage = c.brokerageFlat;
  if (c.brokeragePct > 0) {
    const pct = (t * c.brokeragePct) / 100;
    brokerage = c.brokerageFlat > 0 ? Math.min(c.brokerageFlat, pct) : pct;
  }
  const txn = (t * c.txnPct) / 100;
  const stt = Math.round(((isBuy ? c.sttBuyPct : c.sttSellPct) * t) / 100);
  const sebi = (t * c.sebiPct) / 100;
  const stamp = Math.round(((isBuy ? c.stampBuyPct : c.stampSellPct) * t) / 100);
  const ipft = (t * c.ipftPct) / 100;
  const gst = ((brokerage + txn + sebi + ipft) * c.gstPct) / 100;
  return { brokerage: r2(brokerage), txn: r2(txn), stt: stt, sebi: r2(sebi), stamp: stamp, ipft: ipft, gst: r2(gst), total: r2(brokerage + txn + stt + sebi + stamp + ipft + gst) };
}

// Full round-trip charge estimate for a position closed at `exit`. Returns the
// round-trip total; entry leg uses `side`, exit leg the opposite side so STT
// lands on the sell leg and stamp on the buy leg.
function estimateCharges(entry, exit, qty, side, instrument, tradingSymbol) {
  const q = Math.abs(num(qty));
  const e = Math.abs(num(entry));
  const x = Math.abs(num(exit));
  if (!q || !e || !x) return 0;
  const segment = chargesSegment(instrument, tradingSymbol);
  const isLong = String(side == null ? "BUY" : side).toUpperCase() !== "SELL";
  const entryC = sideCharges(segment, isLong, e * q);
  const exitC = sideCharges(segment, !isLong, x * q);
  return Math.round((entryC.total + exitC.total) * 100) / 100;
}

function renderHoldings(s) {
  const body = document.querySelector("#rtHoldings tbody");
  if (!body) return;
  const rows = s.holdings || [];
  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="5" style="color:#666;padding:6px">Dhan demat me koi holding nahi hai.</td></tr>`;
    return;
  }
  body.innerHTML = rows
    .map((h) => {
      const ltp = num(h.ltp);
      const avg = num(h.avgCostPrice);
      const qty = num(h.totalQty);
      const hasLtp = ltp > 0;
      const pnl = hasLtp ? (ltp - avg) * qty : 0;
      const cls = pnl >= 0 ? "rt-pos" : "rt-neg";
      return `<tr>
        <td><b>${esc(h.tradingSymbol || h.securityId)}</b><br><span style="font-size:8px;color:#666">${esc(h.exchange || "")}</span></td>
        <td>${qty}</td>
        <td>${avg.toFixed(2)}</td>
        <td>${hasLtp ? ltp.toFixed(2) : "--"}</td>
        <td class="${hasLtp ? cls : ""}">${hasLtp ? (pnl >= 0 ? "+" : "-") + fmtMoney(Math.abs(pnl)) : "--"}</td>
      </tr>`;
    })
    .join("");
}

// ---------------------------------------------------------------------------
// Old-app AST entry timing diagnostics + Run Strategy In controls.
// ---------------------------------------------------------------------------
function stratBull(s) {
  return !/bear/i.test(String((s && s.category) || ""));
}

async function loadAstSections() {
  if (astBusy) return;
  astBusy = true;
  try {
    const et = await API.entryTiming();
    AST.entryTiming = (et && et.rows) || [];
    renderRunInControls();
    renderEntryTiming();
  } catch (e) {
    // non-fatal: the core engine UI keeps working
  } finally {
    astBusy = false;
  }
}

function renderRunInControls() {
  const s = (STATE && STATE.settings) || {};
  const en = document.getElementById("rtRunInEnabled");
  if (en) en.checked = !!s.runInEnabled;
  const sd = document.getElementById("rtRunInSide");
  if (sd && s.runInSide) sd.value = s.runInSide;
  const au = document.getElementById("rtRunInAutoCb");
  if (au) au.checked = !!s.runInAuto;
  renderRunInStatus();
}


function renderEntryTiming() {
  const body = document.getElementById("rtEtBody");
  const cnt = document.getElementById("rtEtCount");
  if (cnt) cnt.textContent = AST.entryTiming.length + " event(s)";
  if (!body) return;
  const rows = AST.entryTiming.slice(-60).reverse();
  body.innerHTML = rows.length
    ? rows
        .map((r) => {
          const held = r.metAt ? fmtLogTime(r.metAt) : "-";
          const placed = r.entryAt ? fmtLogTime(r.entryAt) : "-";
          const delayS = num(r.delayMs) / 1000;
          const win = num(r.ordersWindow);
          return `<div style="display:flex;gap:8px;padding:1px 2px;border-bottom:1px solid #16163a">
            <b style="color:#b39ddb">${esc(r.strategyName || r.strategyId || "")}</b>
            <span style="color:#888">held ${esc(held)} · placed ${esc(placed)}</span>
            <span style="color:#666">orders ${win}</span>
            <span style="margin-left:auto" class="${delayS > 0 ? "rt-neg" : "rt-pos"}">delay ${delayS.toFixed(1)}s</span>
          </div>`;
        })
        .join("")
    : `<div style="color:#666">No alignment episodes recorded yet.</div>`;
}


// ---------------------------------------------------------------------------
// Condition Log: engine skip/condition events (no option contracts, no
// tradeable instruments, order errors). Reads `logs` from the snapshot, which
// the server already returns newest-first.
// ---------------------------------------------------------------------------
function fmtLogTime(ms) {
  return istTime(num(ms));
}
async function refreshAccount() {
  const btn = el("rtRefreshAccount");
  if (btn) { btn.disabled = true; btn.textContent = "Refreshing..."; }
  try {
    const a = await API.account();
    if (STATE) {
      if (a.funds) STATE.funds = a.funds;
      if (Array.isArray(a.positions)) STATE.brokerPositions = a.positions;
      if (Array.isArray(a.holdings)) STATE.holdings = a.holdings;
      renderRunning(Object.assign({}, STATE));
      renderHoldings({ holdings: STATE.holdings || [] });
      updateBalanceReadouts();
    }
  } catch (e) {
    /* broker offline - snapshot poll will recover */
  }
  if (btn) { btn.disabled = false; btn.textContent = "Refresh Account"; }
}

async function refreshLogs() {
  try {
    const r = await API.logs();
    if (STATE && Array.isArray(r.logs)) STATE.logs = r.logs;
    renderConditionLog();
  } catch (e) {
    /* keep the last snapshot log */
  }
}

function renderConditionLog() {
  const body = document.getElementById("rtCondLogBody");
  if (!body) return;
  const lvlEl = document.getElementById("rtCondLogLevel");
  const pauseEl = document.getElementById("rtCondLogPause");
  if (pauseEl && pauseEl.checked) return;
  const lvl = (lvlEl && lvlEl.value) || "all";
  let logs = (STATE && STATE.logs) || [];
  if (!Array.isArray(logs)) logs = [];
  if (lvl === "warn") logs = logs.filter((l) => l.level === "warn" || l.level === "error");
  else if (lvl === "error") logs = logs.filter((l) => l.level === "error");
  const cnt = document.getElementById("rtCondLogCount");
  if (cnt) cnt.textContent = logs.length + " line(s)";
  // Skip the DOM rebuild while the visible lines are unchanged (same count and
  // same newest line): the poll runs every second but logs only move on events.
  const sig = logs.length + ":" + (logs.length ? num(logs[0].t) : 0) + ":" + lvl;
  if (sig === logRenderSig) return;
  logRenderSig = sig;
  const color = (l) => (l === "error" ? "#ef5350" : l === "warn" ? "#ffb300" : "#66ccff");
  body.innerHTML = logs.length
    ? logs
        .map(
          (l) => `<div style="display:flex;gap:6px;padding:1px 0;border-bottom:1px solid #16163a">
            <span style="color:#555;flex:0 0 52px">${esc(fmtLogTime(l.t))}</span>
            <b style="color:${color(l.level)};text-transform:uppercase;flex:0 0 40px">${esc(l.level || "info")}</b>
            <span style="color:#ccc;white-space:normal">${esc(l.msg || "")}</span>
          </div>`
        )
        .join("")
    : `<div style="color:#666">No condition events yet.</div>`;
}

// ---------------------------------------------------------------------------
// Live Data Pool
// ---------------------------------------------------------------------------
function applyDataPoolUi(on) {
  const body = document.getElementById("rtDataPoolBody");
  const info = document.getElementById("rtDataPoolInfo");
  const cb = document.getElementById("rtDataPool");
  if (cb && cb.checked !== !!on) cb.checked = !!on;
  if (body) body.style.display = on ? "block" : "none";
  if (info) info.textContent = on ? "ON - live candle + indicator + filter readout" : "OFF - shared pool feeds strategies only";
  if (on) loadPool();
}

async function loadPool(force) {
  const body = document.getElementById("rtDataPoolBody");
  const on = document.getElementById("rtDataPool");
  if (!body || !on || !on.checked) return;
  try {
    const p = await API.pool(force);
    renderPool(p);
  } catch (e) {
    body.textContent = "pool unavailable";
  }
}

function renderPool(p) {
  const body = document.getElementById("rtDataPoolBody");
  if (!body) return;
  const rows = (p && p.rows) || [];
  const scanner = (p && p.scanner) || [];
  const info = document.getElementById("rtDataPoolInfo");
  if (info) info.textContent = "ON - tick-native live candle + indicator + filter readout";
  const scannerHtml = scanner.length
    ? `<div style="border-top:1px solid #2d2d50;margin-top:3px;padding:3px 6px;color:#66ccff;font-weight:700;font-size:9px">Scanner instruments (${scanner.length})</div>` +
      scanner
        .map((r) => {
          const c = r.candle || null;
          const candle = c
            ? `O ${num(c.open).toFixed(2)} H ${num(c.high).toFixed(2)} L ${num(c.low).toFixed(2)} C <b style="color:#fff">${num(c.close).toFixed(2)}</b>`
            : "no candle";
          return `<div style="border-bottom:1px solid #14142e;padding:3px 6px;color:#cfcfe6">
            <b>${esc(r.tradingSymbol || r.securityId)}</b> <span style="color:${String(r.side).toUpperCase() === "PE" ? "#ef5350" : "#00d4aa"}">${esc(r.side || "")}</span>
            <span style="color:#888"> · ${esc(r.source || "")} · ${esc(r.underlying || "")} spot ${num(r.spot).toFixed(2)} (${num(r.changePct).toFixed(2)}%)</span>
            <div style="color:#9a9ac0">${candle}</div></div>`;
        })
        .join("")
    : "";
  if (!rows.length && !scanner.length) {
    body.innerHTML = `<div style="padding:6px;color:#666">No strategies or scanner instruments to read. Add a strategy below or enable a scanner.</div>`;
    return;
  }
  const stratHtml = rows
    .map((r) => {
      const rd = r.readout;
      const head = `<div style="padding:3px 6px;color:${r.enabled ? "#cfcfe6" : "#666"};border-bottom:1px solid #1c1c38">
        <b>${esc(r.name)}</b> · ${esc(r.tradingSymbol || r.securityId)} · ${esc(r.timeframe)} · ${esc(r.side)} ${r.enabled ? "" : "(disabled)"}</div>`;
      if (!rd) return `<div style="border-bottom:1px solid #14142e">${head}<div style="padding:4px 6px;color:#666">no conditions</div></div>`;
      if (rd.error) return `<div style="border-bottom:1px solid #14142e">${head}<div style="padding:4px 6px;color:#ff8888">${esc(rd.error)}</div></div>`;
      const c = rd.candle || {};
      const candle = `<div style="padding:3px 6px;color:#9a9ac0">O ${num(c.open).toFixed(2)} H ${num(c.high).toFixed(2)} L ${num(c.low).toFixed(2)} C <b style="color:#fff">${num(c.close).toFixed(2)}</b> V ${num(c.volume).toFixed(0)}</div>`;
      const conds = (rd.conditions || [])
        .map(
          (x) => `<div style="padding:1px 6px;display:flex;gap:8px;color:${x.pass ? "#7CFFB2" : "#ff8888"}">
            <span style="min-width:120px">${esc(x.indicator)}${x.source ? "[" + esc(x.source) + "]" : ""}</span>
            <span>${esc(x.op)} ${esc(x.target)}</span>
            <span>val=${x.value == null ? "--" : num(x.value).toFixed(2)}</span>
            <b>${x.pass ? "PASS" : "FAIL"}</b></div>`
        )
        .join("");
      const verdict = `<div style="padding:2px 6px;font-weight:700;color:${rd.pass ? "#7CFFB2" : "#ff8888"}">${rd.pass ? "ALL PASS - entry ready" : "WAIT"}</div>`;
      return `<div style="border-bottom:1px solid #14142e">${head}${candle}${conds}${verdict}</div>`;
    })
    .join("");
  body.innerHTML = stratHtml + scannerHtml;
}

// ---------------------------------------------------------------------------
// Templates (server-backed AST engine templates, assignable to directions)
// ---------------------------------------------------------------------------
let TPLS = {};

async function fetchTemplates() {
  try {
    const r = await API.templates();
    TPLS = (r && r.templates) || {};
  } catch (e) {
    TPLS = {};
  }
  return TPLS;
}
function templateNames(side) {
  return Object.keys(TPLS)
    .filter((n) => !side || !TPLS[n].side || TPLS[n].side === side)
    .sort();
}
async function refreshTemplateList() {
  await fetchTemplates();
  const fill = (id, side) => {
    const sel = document.getElementById(id);
    if (!sel) return;
    const cur = sel.value;
    sel.innerHTML = `<option value="">-- none --</option>` +
      templateNames(side).map((n) => `<option value="${esc(n)}">${esc(n)}</option>`).join("");
    if (cur) sel.value = cur;
  };
  fill("rtTplOpen");
  fill("rtTplBull", "bullish");
  fill("rtTplBear", "bearish");
  renderTemplateAssignment();
  renderQuickRun();
  if (STATE && STATE.settings) applySettingsToDom(STATE.settings);
}

// ---------------------------------------------------------------------------
// "Assign AST Template to direction" (old-app parity): saved engine-settings
// templates bound to a direction. The assignment drives the next run start of
// that side; no assignment = manual engine settings run as usual.
// ---------------------------------------------------------------------------
function tplOptions(side, selected) {
  const names = templateNames(side);
  if (!names.length) return `<option value="">-- no saved ${side} template --</option>`;
  return (
    `<option value="">-- select ${side} template --</option>` +
    names.map((n) => `<option value="${esc(n)}"${n === selected ? " selected" : ""}>${esc(n)}</option>`).join("")
  );
}
function assignedTplChip(containerId, key, side, name) {
  const box = document.getElementById(containerId);
  if (!box) return;
  if (!name) {
    box.innerHTML = `<span style="color:#666">none assigned - manual engine settings run as usual</span>`;
    return;
  }
  const t = TPLS[name] || {};
  box.innerHTML =
    `<span style="display:inline-flex;align-items:center;gap:4px;background:#1a1a35;border:1px solid #2d2d50;color:#ffd700;border-radius:3px;padding:2px 6px;font-size:9px;margin:1px">` +
    `${esc(name)} (${esc(t.side || side)})` +
    ` <button data-tpl-clear="${esc(key)}" title="Remove from ${esc(side)} assignments" style="background:none;border:none;color:#ef5350;font-weight:700;cursor:pointer;font-size:11px;padding:0 2px;line-height:1">\u00d7</button></span>`;
  box.querySelectorAll("[data-tpl-clear]").forEach((b) => {
    b.onclick = () => pushSetting({ [b.getAttribute("data-tpl-clear")]: "" });
  });
}
function setDirSelect(id, side, val) {
  const el = document.getElementById(id);
  if (!el) return;
  const sig = String(val || "") + "|" + templateNames(side).join(",");
  if (el.dataset.tplSig === sig) return;
  el.dataset.tplSig = sig;
  el.innerHTML = tplOptions(side, val || "");
}
function renderTemplateAssignment() {
  const s = (STATE && STATE.settings) || {};
  setDirSelect("rtTplMoverBull", "bullish", s.moverBullTemplate);
  setDirSelect("rtTplMoverBear", "bearish", s.moverBearTemplate);
  assignedTplChip("rtMoverBullTplList", "moverBullTemplate", "bullish", s.moverBullTemplate);
  assignedTplChip("rtMoverBearTplList", "moverBearTemplate", "bearish", s.moverBearTemplate);
}
function wireAssign(selId, btnId, key, side) {
  const btn = document.getElementById(btnId);
  if (!btn) return;
  btn.onclick = () => {
    const sel = document.getElementById(selId);
    const v = sel ? sel.value : "";
    if (!v) {
      uiAlert("Pick a saved " + side + " template to assign first");
      return;
    }
    pushSetting({ [key]: v });
  };
}

// ---------------------------------------------------------------------------
// "AST saved templates quick run" (old-app parity): pick a saved AST engine
// template and start the engine on it in one click. The server applies the
// template's saved settings, restores its ticked strategy set and turns the
// engine on (arm required to place live orders).
// ---------------------------------------------------------------------------
function renderQuickRun() {
  const sel = document.getElementById("rtAstTplRun");
  if (!sel) return;
  const cur = sel.value;
  const names = Object.keys(TPLS).sort();
  const rows = names.map((n) => {
    const t = TPLS[n] || {};
    const nc = Array.isArray(t.selected) ? t.selected.length : 0;
    return { id: n, label: `${n} (${t.side || ""})${nc ? " \u00b7 " + nc + " strat" : ""}` };
  });
  const sig = rows.map((r) => r.id + "~" + r.label).join("|");
  const hasVal = (v) => Array.from(sel.options).some((o) => o.value === v);
  if (sel.dataset.tplIds === sig) {
    if (cur && hasVal(cur)) sel.value = cur;
    return;
  }
  sel.dataset.tplIds = sig;
  sel.innerHTML = rows.length
    ? `<option value="">-- select template --</option>` + rows.map((r) => `<option value="${esc(r.id)}">${esc(r.label)}</option>`).join("")
    : `<option value="">-- no saved templates --</option>`;
  if (cur && hasVal(cur)) sel.value = cur;
}

async function quickRunTemplate() {
  const sel = document.getElementById("rtAstTplRun");
  const status = document.getElementById("rtAstTplRunStatus");
  const name = sel ? sel.value : "";
  if (!name) {
    if (status) status.innerHTML = `<span style="color:#ff9800">Please select a saved template from the dropdown first.</span>`;
    return;
  }
  try {
    const r = await API.template({ action: "run", name });
    if (r && r.ok) {
      // Top Movers with BOTH legs assigned: the gainer (bullish) + loser (bearish)
      // assigned templates drive the live run per direction, not the dropdown
      // template's saved gate settings (old-app parity note).
      const s = (STATE && STATE.settings) || {};
      const moversAssigned = !!s.moversOn && !!s.moverBullTemplate && !!s.moverBearTemplate;
      if (status)
        status.innerHTML =
          `<span style="color:#b39ddb;font-weight:700">RUN</span> template <b style="color:#fff">${esc(r.name)}</b>` +
          ` (${esc(r.side || "")}) started` +
          ` <span style="color:#ff9800">- arm the engine to place live orders</span>` +
          (r.restored ? ` \u00b7 <span style="color:#ffd700">${num(r.restored)} strategy(s) restored</span>` : "") +
          (moversAssigned
            ? `<br><span style="color:#00d4aa">Top Movers assigned templates (gainers + losers) drive the live run per direction.</span>`
            : "");
    } else {
      if (status) status.innerHTML = `<span style="color:#ef5350">Run failed - ${esc((r && r.error) || "see Condition Log")}.</span>`;
    }
    refresh();
    loadAstSections();
  } catch (e) {
    if (status) status.innerHTML = `<span style="color:#ef5350">Run failed - ${esc(String(e))}</span>`;
  }
}
// ---------------------------------------------------------------------------
// Catalog + polling
// ---------------------------------------------------------------------------
function fillCatalog() {
  populateAstSelects();
}

async function loadCatalog() {
  try {
    const [ind, tfs, syms] = await Promise.all([get("/api/indicators"), get("/api/timeframes"), get("/api/symbols")]);
    CATALOG.indicators = Array.isArray(ind) ? ind : [];
    CATALOG.timeframes = Array.isArray(tfs) ? tfs : [];
    const raw = Array.isArray(syms) ? syms : syms && Array.isArray(syms.symbols) ? syms.symbols : [];
    CATALOG.symbols = raw
      .map((s) => {
        if (Array.isArray(s)) {
          // [name, id, exchangeSegment, instrument, optionChainId, optionChainExch, category]
          return {
            id: num(s[1]),
            name: String(s[0] || s[1] || ""),
            exch: s[2] || "NSE_FNO",
            inst: s[3] || "OPTIDX",
          };
        }
        return {
          id: num(s.id || s.securityId || s.security_id),
          name: s.name || s.symbol || s.trading_symbol || String(s.id || ""),
          exch: s.exch || s.exchange_segment || s.exchangeSegment || "NSE_FNO",
          inst: s.inst || s.instrument || s.instrument_type || "OPTIDX",
        };
      })
      .filter((s) => s.id > 0);
  } catch (e) {
    console.warn("realtime catalog load failed", e);
  }
  fillCatalog();
}

async function refresh() {
  if (refreshInFlight) {
    // Coalesce: run one more pass right after the current response lands, so a
    // refresh requested by a button/post never races the in-flight poll.
    refreshQueued = true;
    return;
  }
  refreshInFlight = true;
  const seq = ++refreshSeq;
  try {
    const s = await API.snapshot();
    // Single-flight means only one snapshot can resolve, so this holds; kept as
    // a belt-and-braces guard against any future parallel path.
    if (seq !== refreshSeq) return;
    renderSnapshot(s);
    const conn = document.getElementById("rtConn");
    if (conn && !document.body.classList.contains("link-alarm")) {
      conn.textContent = PAPER ? "Paper engine ready" : "Engine session ready";
      conn.className = "rt-pill on";
    }
    fillCatalog();
  } catch (e) {
    // A stale request that fails after a newer one succeeded must not flip the
    // connection pill back to "unavailable".
    if (seq !== refreshSeq) return;
    console.warn("realtime refresh failed", e);
    const conn = document.getElementById("rtConn");
    if (conn) {
      conn.textContent = PAPER ? "paper engine unavailable" : "engine unavailable";
      conn.className = "rt-pill off";
    }
  } finally {
    refreshInFlight = false;
    if (refreshQueued) {
      refreshQueued = false;
      refresh();
    }
  }
}

// Full closed-trade ledger, loaded on demand (not on the 1s poll). The snapshot
// carries only the newest slice plus the total count; when the count changes we
// re-pull the full ledger once, so every trade still shows without paying the
// payload cost every second.
async function loadClosed() {
  if (closedLoading) return;
  // Coalesce: a burst of closes must not kick off a multi-MB fetch on every
  // poll. One refresh per few seconds is plenty for a read-only ledger.
  if (Date.now() - closedLoadedAt < 3000) return;
  closedLoading = true;
  closedLoadedAt = Date.now();
  try {
    const r = await API.closed();
    if (r && Array.isArray(r.closed)) {
      CLOSED_CACHE = r.closed;
      renderClosed(STATE);
    }
  } catch (e) {
    console.warn("closed ledger load failed", e);
  } finally {
    closedLoading = false;
  }
}

// Day-scoped stats (`range=today`) feeding the engine summary strip. Re-pulled
// when the closed-trade count moves and at most every 15s otherwise, so the
// summary stays "today only" without hammering the stats endpoint each second.
async function loadDayStats(closedCount) {
  const sig = String(num(closedCount));
  if (dayStatsLoading) return;
  if (DAY_STATS && sig === dayStatsSig && Date.now() - dayStatsAt < 15000) return;
  dayStatsLoading = true;
  dayStatsAt = Date.now();
  try {
    const r = await API.stats("today");
    if (r && r.stats) {
      DAY_STATS = r.stats;
      dayStatsSig = sig;
    }
  } catch (e) {
    console.warn("day stats load failed", e);
  } finally {
    dayStatsLoading = false;
  }
}

function active() {
  const e = document.getElementById("tab-realtime");
  return !!e && e.classList.contains("active");
}

// ---------------------------------------------------------------------------
// Dhan link monitor
//
// One observer drives the connect/disconnect popups, the persistent red banner,
// the stale-data alarm and the chart "DISCONNECTED" veil. It runs in both the
// realtime window and the paper iframe (which pre-sets `window.__PAPER__`), and
// it polls the same `/api/feed/status` the server watchdog uses, so the UI and
// the server always agree on whether ticks are actually flowing.
//
// Server-side auto-reconnect already lives in the feed watchdog; the client only
// nudges it when a session exists, the market is open and the socket is silent.
// The access token is never persisted, so a full server/tunnel restart shows the
// red "offline" banner and asks the operator to Connect again.
// ---------------------------------------------------------------------------
let linkTimer = null;
const LINK = { state: "unknown", downSince: 0, nextRetry: 0, backoff: 0 };

function ensureLinkUi() {
  // The parent window owns the top-of-page popups/banner; the paper iframe only
  // needs its own pane-level alarm, so it never injects a second banner.
  if (!PAPER) {
    if (!document.getElementById("linkBanner")) {
      const b = document.createElement("div");
      b.id = "linkBanner";
      const body = document.querySelector("body");
      if (body && body.firstChild) body.insertBefore(b, body.firstChild);
      else if (body) body.appendChild(b);
    }
    if (!document.getElementById("linkToasts")) {
      const t = document.createElement("div");
      t.id = "linkToasts";
      document.body.appendChild(t);
    }
  }
  const cw = document.querySelector("#tab-chart .chart-wrap");
  if (cw && !document.getElementById("rtLinkVeil")) {
    const v = document.createElement("div");
    v.id = "rtLinkVeil";
    v.innerHTML =
      "<div>DISCONNECTED</div>" +
      '<div style="font-size:11px;font-weight:400;color:#c9c9e0;letter-spacing:0">' +
      "Live data paused - reconnecting automatically</div>";
    cw.appendChild(v);
  }
}

function linkToast(kind, text, ms) {
  if (PAPER) return; // parent window draws the popups
  ensureLinkUi();
  const wrap = document.getElementById("linkToasts");
  if (!wrap) return;
  const el = document.createElement("div");
  el.className = "link-toast " + (kind === "ok" ? "ok" : "err");
  el.textContent = text;
  wrap.appendChild(el);
  setTimeout(() => {
    el.style.opacity = "0";
    setTimeout(() => el.remove(), 350);
  }, ms || 4500);
}

function linkBanner(kind, text) {
  if (PAPER) return;
  ensureLinkUi();
  const b = document.getElementById("linkBanner");
  if (!b) return;
  if (!kind) {
    b.className = "";
    b.style.display = "none";
    return;
  }
  b.className = "show " + kind;
  b.textContent = text;
}

function setAlarm(on) {
  document.body.classList.toggle("link-alarm", !!on);
  const v = document.getElementById("rtLinkVeil");
  if (v) v.classList.toggle("show", !!on);
}

function clearStaleQuotes() {
  // A disconnected feed must not leave a frozen LTP looking live. The sidebar
  // rows are the only place the last pushed quote is still painted.
  if (PAPER) return;
  document
    .querySelectorAll(
      "#marketWatch .mw-ltp, #marketWatch .mw-chg, #commodityWatch .mw-ltp, #commodityWatch .mw-chg"
    )
    .forEach((el) => {
      el.textContent = "--";
      el.classList.remove("up", "down");
    });
  const sel = chartSelection();
  if (sel) delete sel.ltp;

  // RT order-placement card: drop the auto-filled LTP and broker margin readout
  // so the card cannot quote a stale price. A manually typed price is preserved.
  const pEl = document.querySelector("#rtEngMethodOpts .rtEng-price");
  if (pEl && priceAuto[activeMethod] !== false) pEl.value = "";
  const req = document.getElementById("rtEngReq");
  if (req) {
    delete req.dataset.live;
    req.textContent = "--";
  }
  const fr = document.getElementById("rtEngFreeze");
  if (fr) fr.textContent = "--";
}

function linkStatusText(state, info) {
  if (state === "offline") return (info && info.authError) || "Offline - enter Client ID + Token";
  if (state === "down") {
    const age = info && info.age != null ? " (last tick " + Math.round(info.age) + "s ago)" : "";
    return "Algo Disconnected - auto reconnecting" + age;
  }
  return null;
}

function applyLink(state, info) {
  info = info || {};
  const prev = window.__linkState || "unknown";
  window.__linkState = state;

  const wasAlarm = prev === "down" || prev === "offline";
  const isAlarm = state === "down" || state === "offline";

  // A connect is legitimately offline/down until the handshake completes; don't
  // fire the red popup, banner, veil or stale-wipe during that window.
  if (window.__connecting && state !== "live") return;

  if (state !== prev) {
    if (state === "live") {
      linkToast("ok", "Algo Connected - live Dhan feed streaming");
      linkBanner(null);
    } else if (state === "down" && !wasAlarm) {
      linkToast("err", "Algo Disconnected - auto reconnecting...", 6000);
    } else if (state === "offline" && !wasAlarm) {
      linkToast("err", "Algo Offline - Dhan not connected", 7000);
    }
  }

  if (isAlarm && !wasAlarm) {
    LINK.downSince = Date.now();
    clearStaleQuotes();
  }
  if (state === "live") {
    LINK.downSince = 0;
    LINK.backoff = 0;
    LINK.nextRetry = 0;
  }

  setAlarm(isAlarm);

  // Let the sidebar blank its panels while the link is down (and stop repainting
  // them from the server's last cached quote).
  // Paint the sidebar while the feed is streaming (live) or the session is up
  // but the exchange is shut (closed), so a closed market still shows each
  // symbol's previous close / daily change. Offline / down / connecting freeze
  // the last painted values instead.
  const paintOk = state === "live" || state === "closed";
  window.__feedDown = !paintOk;
  try {
    window.dispatchEvent(new CustomEvent("dhan-link", { detail: { state, live: paintOk, alarm: isAlarm, info } }));
  } catch (e) {
    /* CustomEvent unavailable: non-fatal */
  }

  // Pane pill (shared with refresh(); this is the authoritative value).
  const conn = document.getElementById("rtConn");
  if (conn) {
    if (state === "live") {
      conn.textContent = PAPER ? "Paper engine ready" : "Engine session ready";
      conn.className = "rt-pill on";
    } else if (state === "connecting") {
      conn.textContent = "Connecting...";
      conn.className = "rt-pill armed";
    } else if (state === "closed") {
      conn.textContent = "Market closed";
      conn.className = "rt-pill off";
    } else {
      conn.textContent = PAPER ? "Paper feed offline" : "Feed disconnected";
      conn.className = "rt-pill off";
    }
  }

  // Top-bar status + banner (main window only).
  if (!PAPER) {
    const st = document.getElementById("status");
    if (st) {
      if (state === "live") {
        st.textContent = "Connected - live feed streaming";
        st.className = "ok";
      } else if (state === "closed") {
        st.textContent = "Market closed - feed idle";
        st.className = "warn";
      } else if (state === "connecting") {
        st.textContent = "Connecting to live feed...";
        st.className = "warn";
      } else {
        st.textContent = linkStatusText(state, info);
        st.className = "error";
      }
    }
    if (state === "down") {
      const age = info.age != null ? " (last tick " + Math.round(info.age) + "s ago)" : "";
      linkBanner("err", "Algo DISCONNECTED - auto reconnecting" + age);
    } else if (state === "offline") {
      linkBanner("err", "Algo OFFLINE - Dhan not connected");
    } else if (state === "closed") {
      linkBanner("warn", "Market closed - feed idle");
    } else if (state === "connecting") {
      linkBanner("warn", "Connecting to live feed...");
    } else if (state === "live") {
      linkBanner(null);
    }
  }

  // Server-side auto-reconnect nudge: only while a session exists, the market is
  // open and the socket has gone silent. The server park cooldown is respected.
  if (state === "down" && info.connected && info.open && !info.parked && !PAPER) {
    const now = Date.now();
    if (LINK.downSince && now - LINK.downSince < 6000) return;
    if (now < LINK.nextRetry) return;
    LINK.backoff = LINK.backoff ? Math.min(LINK.backoff * 2, 60) : 12;
    LINK.nextRetry = now + LINK.backoff * 1000;
    fetch("/api/feed/restart", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    }).catch(() => {});
  }
}

// Fallback link derivation for when the server has not been rebuilt with the
// `link` field yet. Mirrors the server's `link_state` so the UI behaves the same
// in both cases. Market hours: IST 09:15-23:30, Mon-Fri (cash + MCX evening).
function istMarketOpen() {
  const now = new Date();
  const ist = new Date(now.getTime() + (330 + now.getTimezoneOffset()) * 60000);
  const day = ist.getDay();
  if (day === 0 || day === 6) return false;
  const mins = ist.getHours() * 60 + ist.getMinutes();
  return mins >= 9 * 60 + 15 && mins <= 23 * 60 + 30;
}

function deriveLink(d) {
  if (d.link) return d.link;
  if (!d.connected) return "offline";
  if (!istMarketOpen()) return "closed";
  const age = d.last_tick_age_sec;
  if (d.feed_up && age != null && age < 15) return "live";
  if (!d.feed_up && d.ws_running && d.feed_started_age_sec != null && d.feed_started_age_sec < 30) {
    return "connecting";
  }
  return "down";
}

async function linkPoll() {
  let d = null;
  let conn = null;
  try {
    const [fr, sr] = await Promise.all([
      fetch("/api/feed/status", { cache: "no-store" }),
      fetch("/api/status", { cache: "no-store" }),
    ]);
    d = await fr.json();
    try { conn = await sr.json(); } catch (e) { conn = null; }
  } catch (e) {
    d = null;
  }
  if (!d || d.status !== "success") {
    applyLink("offline", { authError: null });
    return;
  }
  if (conn && typeof conn.connected === "boolean" && typeof d.connected !== "boolean") {
    d.connected = conn.connected;
  }
  if (d.market_open == null) d.market_open = istMarketOpen();
  if (conn && conn.auth_error) d.auth_error = conn.auth_error;
  const info = {
    connected: !!d.connected,
    open: !!d.market_open,
    parked: num(d.reconnect_parked_sec) > 0,
    age: d.last_tick_age_sec,
    feedUp: !!d.feed_up,
    authError: d.auth_error || null,
  };
  applyLink(deriveLink(d), info);
}

function bootLinkMonitor() {
  if (linkTimer) return;
  ensureLinkUi();
  const kick = () => {
    if (!PAPER && document.hidden) return; // parent paused; iframe keeps its own
    linkPoll();
  };
  kick();
  linkTimer = setInterval(kick, 2000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) kick();
  });
}

// [accent, solid bg, edge, readable text] per section. Cycled across the engine
// rows so every section gets its own flat colour, ribbon and text hue.
const SECTION_COLORS = [
  ["#38bdf8", "#0f2433", "#1f4a63", "#e6eaf5"],
  ["#fbbf24", "#2a2314", "#57491f", "#e6eaf5"],
  ["#34d399", "#0f2a20", "#215340", "#e6eaf5"],
  ["#a78bfa", "#1b1830", "#3a3160", "#e6eaf5"],
  ["#f472b6", "#2a1626", "#552a48", "#e6eaf5"],
  ["#fb7185", "#2b1620", "#582936", "#e6eaf5"],
  ["#22d3ee", "#0e2530", "#1f4c5e", "#e6eaf5"],
  ["#a3e635", "#1f2a14", "#3c5228", "#e6eaf5"],
  ["#fb923c", "#2b1d12", "#573a22", "#e6eaf5"],
  ["#e879f9", "#271733", "#502c66", "#e6eaf5"],
  ["#2dd4bf", "#0e2a26", "#1f544b", "#e6eaf5"],
  ["#60a5fa", "#141d31", "#293a5a", "#e6eaf5"],
];

function paintSections() {
  const rt = document.getElementById("tab-realtime");
  if (!rt) return;
  const paint = (el, i) => {
    const c = SECTION_COLORS[i % SECTION_COLORS.length];
    el.style.setProperty("--sec-accent", c[0]);
    el.style.setProperty("--sec-bg", c[1]);
    el.style.setProperty("--sec-edge", c[2]);
    el.style.setProperty("--sec-text", c[3]);
  };
  rt.querySelectorAll(".rt-engine-row").forEach(paint);
  rt.querySelectorAll(".account-section").forEach((sec, i) => paint(sec, i + 3));
}

export function bootRealtime() {
  if (booted) return;
  booted = true;
  style();
  shell();
  paintSections();
  bootLinkMonitor();
  loadCatalog();
  refresh();
  timer = setInterval(() => {
    if (active()) refresh();
  }, 1000);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && active()) refresh();
  });
  // Gear on the Straight Line Consensus filter rows opens its per-filter
  // settings (Min net votes / Confirm bars / Swing strength / colors / width).
  document.addEventListener("click", (e) => {
    const t = e.target;
    if (t && t.classList && t.classList.contains("sc-gear")) {
      e.preventDefault();
      e.stopPropagation();
      openConsensusSettings();
    }
    if (t && t.classList && t.classList.contains("pt-gear")) {
      e.preventDefault();
      e.stopPropagation();
      openPivotTrendSettings(t.getAttribute("data-pt") === "resistance" ? "resistance" : "support");
    }
  });
}


export function realtimeTick(connected) {
  const conn = document.getElementById("rtConn");
  if (conn) {
    conn.textContent = connected ? "Connected" : "Disconnected";
    conn.className = "rt-pill " + (connected ? "on" : "off");
  }
}

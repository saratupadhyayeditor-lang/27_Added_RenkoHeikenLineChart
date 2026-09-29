//! Market-data sidebar backend: symbol catalog, MCX commodity list and the
//! `/ws` push channel.
//!
//! Quotes come only from the live Dhan session (REST snapshot + websocket
//! feed). The JSON shapes deliberately match the old app so the same UI
//! contract applies.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify};

use algo_core::model::Candle;

use crate::broker::DhanState;

/// Raw catalog extracted from the old app: `{symbols: [...], watchlists: {...}}`.
pub fn catalog_str() -> &'static str {
    include_str!("market.json")
}

pub fn catalog_json() -> Value {
    serde_json::from_str(catalog_str()).unwrap_or(Value::Null)
}

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Bucket size (seconds) of a chart timeframe for live bar patching. Weekly and
/// longer frames are intentionally excluded (0 = not tick-patchable), because
/// bucketing them needs calendar logic rather than a fixed step.
pub fn tf_bucket_secs(tf: &str) -> i64 {
    match tf {
        "1min" => 60,
        "2min" => 120,
        "3min" => 180,
        "4min" => 240,
        "5min" => 300,
        "10min" => 600,
        "15min" => 900,
        "30min" => 1800,
        "1hour" => 3600,
        "4hour" => 14400,
        "1day" | "day" | "daily" => 86400,
        _ => 0,
    }
}

/// IST seconds-of-day at which the regular session opens. The pre-open call
/// auction (IST 09:00-09:15) is not a real trading session, so a tick in that
/// window must not seed or advance a bar - otherwise the engine starts on a
/// pre-market candle that never exists on the real chart. Applied to every
/// timeframe; the MCX evening session (till 23:30) is unaffected because only
/// the morning pre-09:15 window is gated.
pub const IST_SESSION_OPEN_SECS: i64 = 9 * 3600 + 15 * 60;

/// Whether a tick timestamped `now_ist` (epoch seconds whose clock value is IST)
/// belongs to the regular session and may form/advance a candle.
pub fn ist_session_started(now_ist: i64) -> bool {
    now_ist.rem_euclid(86_400) >= IST_SESSION_OPEN_SECS
}

/// Bound on the number of tick-maintained candle series kept in memory.
const LIVE_BARS_CAP: usize = 2048;

/// Old-app quote cache key: indices live under `IDX_I:<sid>`; everything else is
/// the plain security id so an equity and an index can never collide.
pub fn quote_key(sid: i64, exch: &str) -> String {
    match exch.to_uppercase().as_str() {
        "NSE" | "BSE" | "IDX_I" => format!("IDX_I:{}", sid),
        _ => sid.to_string(),
    }
}

#[derive(Deserialize)]
struct RawCatalog {
    #[serde(default)]
    symbols: Vec<Value>,
    #[serde(default)]
    watchlists: HashMap<String, Vec<Value>>,
}

/// MCX near-month futures. The old app derived these live from the Dhan scrip
/// master (contract ids roll every expiry); we ship a representative static set
/// with the identical payload shape.
const COMMODITIES: &[(&str, &str, i64, f64, f64, &str, bool)] = &[
    ("GOLD", "GOLD28AUG26", 500001, 100.0, 0.01, "2026-08-28", true),
    ("SILVER", "SILVER05SEP26", 500002, 30.0, 1.0, "2026-09-05", true),
    ("CRUDEOIL", "CRUDEOIL19AUG26", 500003, 100.0, 1.0, "2026-08-19", true),
    ("NATURALGAS", "NATURALGAS26AUG26", 500004, 1250.0, 0.1, "2026-08-26", true),
    ("COPPER", "COPPER28AUG26", 500005, 2500.0, 0.05, "2026-08-28", true),
    ("ZINC", "ZINC28AUG26", 500006, 5000.0, 0.05, "2026-08-28", true),
    ("ALUMINIUM", "ALUMINIUM28AUG26", 500007, 5000.0, 0.05, "2026-08-28", true),
    ("LEAD", "LEAD28AUG26", 500008, 5000.0, 0.05, "2026-08-28", true),
    ("NICKEL", "NICKEL28AUG26", 500009, 1500.0, 0.1, "2026-08-28", true),
    ("SILVERM", "SILVERM30SEP26", 500010, 5.0, 1.0, "2026-09-30", false),
    ("GOLDM", "GOLDM30SEP26", 500011, 10.0, 1.0, "2026-09-30", false),
    ("GOLDPETAL", "GOLDPETAL30SEP26", 500012, 1.0, 1.0, "2026-09-30", false),
    ("GOLDGUINEA", "GOLDGUINEA30SEP26", 500013, 1.0, 1.0, "2026-09-30", false),
    ("MENTHAOIL", "MENTHAOIL28AUG26", 500014, 360.0, 0.1, "2026-08-28", true),
    ("COTTONCANDY", "COTTONCANDY28AUG26", 500015, 25.0, 10.0, "2026-08-28", false),
];

/// Live near-month MCX futures resolved from the Dhan scrip master, falling back
/// to the bundled representative set while the master is still warming. Dhan
/// rolls MCX contract ids every expiry, so a static list can neither quote nor
/// place real orders.
pub fn commodity_rows() -> Vec<crate::scrip::CommodityRow> {
    if let Some(sc) = crate::scrip::get() {
        let live = sc.commodity_futures();
        if !live.is_empty() {
            return live;
        }
    }
    COMMODITIES
        .iter()
        .map(|(name, symbol, sid, lot, _tick, exp, opts)| crate::scrip::CommodityRow {
            name: (*name).to_string(),
            trading_symbol: (*symbol).to_string(),
            security_id: *sid,
            lot: *lot,
            expiry: (*exp).to_string(),
            has_options: *opts,
        })
        .collect()
}

pub fn commodities_json() -> Value {
    let list: Vec<Value> = commodity_rows()
        .into_iter()
        .map(|c| {
            json!({
                "name": c.name,
                "symbol": c.trading_symbol,
                "security_id": c.security_id,
                "lot_size": c.lot,
                "tick_size": 0.0,
                "expiry": c.expiry,
                "has_options": c.has_options,
            })
        })
        .collect();
    json!({ "commodities": list })
}

/// Default MCX commodity security ids used when the operator has not added a
/// custom `commodityList` yet. Mirrors the old app's near-month FUTCOM set.
pub fn commodity_ids() -> Vec<i64> {
    commodity_rows().into_iter().map(|c| c.security_id).collect()
}

/// `Some(false)` when the security is a known MCX commodity with no listed
/// options (futures-only), `None` when unknown so callers can keep the default.
pub fn commodity_has_options(sid: i64) -> Option<bool> {
    commodity_rows()
        .into_iter()
        .find(|c| c.security_id == sid)
        .map(|c| c.has_options)
}

/// Every security the live feed subscribes to: the dropdown symbols, the
/// market-watch companies and the MCX commodities.
pub fn securities() -> Vec<(i64, String)> {
    let cat: RawCatalog = match serde_json::from_str(catalog_str()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<(i64, String)> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let push = |sid: i64, exch: &str, out: &mut Vec<(i64, String)>, seen: &mut std::collections::HashSet<i64>| {
        if sid <= 0 || !seen.insert(sid) {
            return;
        }
        out.push((sid, exch.to_string()));
    };
    for row in &cat.symbols {
        if let Some(arr) = row.as_array() {
            let sid = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
            let exch = arr.get(2).and_then(|v| v.as_str()).unwrap_or("NSE_EQ");
            push(sid, exch, &mut out, &mut seen);
        }
    }
    for items in cat.watchlists.values() {
        for row in items {
            if let Some(arr) = row.as_array() {
                let sid = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
                push(sid, "NSE_EQ", &mut out, &mut seen);
            }
        }
    }
    for c in commodity_rows() {
        push(c.security_id, "MCX_COMM", &mut out, &mut seen);
    }
    out
}

/// `(name, exchange_segment, instrument)` for a security id in the catalog,
/// watchlists or the MCX commodity set. Used by the realtime scanners to build
/// tradeable instruments (option resolution needs the underlying name).
pub fn symbol_meta(sid: i64) -> Option<(String, String, String)> {
    if sid <= 0 {
        return None;
    }
    let cat: RawCatalog = serde_json::from_str(catalog_str()).ok()?;
    for row in &cat.symbols {
        if let Some(arr) = row.as_array() {
            if arr.get(1).and_then(|v| v.as_i64()) == Some(sid) {
                let name = arr.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
                let seg = arr.get(2).and_then(|v| v.as_str()).unwrap_or("NSE_EQ").to_string();
                let inst = arr.get(3).and_then(|v| v.as_str()).unwrap_or("EQ").to_string();
                return Some((name, seg, inst));
            }
        }
    }
    for items in cat.watchlists.values() {
        for row in items {
            if let Some(arr) = row.as_array() {
                if arr.get(1).and_then(|v| v.as_i64()) == Some(sid) {
                    let name = arr.get(0).and_then(|v| v.as_str()).unwrap_or("").to_string();
                    return Some((name, "NSE_EQ".to_string(), "EQ".to_string()));
                }
            }
        }
    }
    for c in commodity_rows() {
        if c.security_id == sid {
            return Some((c.name, "MCX_COMM".to_string(), "FUTCOM".to_string()));
        }
    }
    None
}

/// Gate for the upstream REST snapshot: at most one Dhan round-trip in flight,
/// with a minimum gap between them. Fast client polls otherwise pile up calls
/// and trip Dhan's rate limit.
#[derive(Default)]
struct RestGate {
    busy: bool,
    last: Option<Instant>,
}

#[derive(Clone)]
pub struct MarketState {
    pub quotes: Arc<Mutex<HashMap<String, Value>>>,
    pub bc: broadcast::Sender<String>,
    /// Extra securities (option strikes registered from the option-chain tab)
    /// that the live feed subscribes to in addition to the static catalog.
    extra: Arc<Mutex<Vec<(i64, String)>>>,
    /// Bounds the upstream Dhan REST snapshot traffic.
    rest_gate: Arc<Mutex<RestGate>>,
    /// Tick-native candle series per `{security_id}:{timeframe}`, kept fresh by
    /// patching the forming bar from the live websocket feed. This is what
    /// removes the REST candle round-trip (and its ~2s staleness) from the
    /// entry hot path.
    live_bars: Arc<Mutex<HashMap<String, LiveBars>>>,
    /// Reverse index `security_id -> [bar keys]` so a tick only touches the
    /// handful of series that track that security.
    bar_index: Arc<Mutex<HashMap<i64, Vec<String>>>>,
    /// Woken on every applied tick so the position guardian can react to the
    /// freshest price instead of waiting for its polling interval.
    tick_notify: Arc<Notify>,
}

/// A live, tick-maintained candle series.
struct LiveBars {
    candles: Vec<Candle>,
    updated: Instant,
}

impl MarketState {
    pub fn new() -> Self {
        let (bc, _) = broadcast::channel(16);
        MarketState {
            quotes: Arc::new(Mutex::new(HashMap::new())),
            bc,
            extra: Arc::new(Mutex::new(Vec::new())),
            rest_gate: Arc::new(Mutex::new(RestGate::default())),
            live_bars: Arc::new(Mutex::new(HashMap::new())),
            bar_index: Arc::new(Mutex::new(HashMap::new())),
            tick_notify: Arc::new(Notify::new()),
        }
    }

    /// Handle to await on for the next applied market tick.
    pub fn tick_notify(&self) -> Arc<Notify> {
        self.tick_notify.clone()
    }

    /// Seed (or refresh) a live candle series from a REST fetch. After this the
    /// series is advanced by ticks, so the next scan reads it without a REST
    /// call.
    pub fn seed_bars(&self, sec_id: i64, tf: &str, candles: Vec<Candle>) {
        if sec_id <= 0 || candles.len() < 3 {
            return;
        }
        let key = format!("{sec_id}:{tf}");
        // Evict the least-recently-updated series when full so a long session
        // that resolves many option strikes cannot grow memory without bound.
        let evict: Option<String> = {
            let Ok(bars) = self.live_bars.lock() else { return };
            if bars.len() >= LIVE_BARS_CAP && !bars.contains_key(&key) {
                bars.iter().min_by_key(|(_, b)| b.updated).map(|(k, _)| k.clone())
            } else {
                None
            }
        };
        if let Some(old) = evict {
            if let Some((sid_s, _)) = old.split_once(':') {
                if let Ok(sid) = sid_s.parse::<i64>() {
                    if let Ok(mut idx) = self.bar_index.lock() {
                        if let Some(list) = idx.get_mut(&sid) {
                            list.retain(|k| k != &old);
                            if list.is_empty() {
                                idx.remove(&sid);
                            }
                        }
                    }
                }
            }
            if let Ok(mut bars) = self.live_bars.lock() {
                bars.remove(&old);
            }
        }
        if let Ok(mut bars) = self.live_bars.lock() {
            bars.insert(
                key.clone(),
                LiveBars {
                    candles,
                    updated: Instant::now(),
                },
            );
        }
        if let Ok(mut idx) = self.bar_index.lock() {
            let list = idx.entry(sec_id).or_default();
            if !list.contains(&key) {
                list.push(key);
            }
        }
    }

    /// Live series for `(sec_id, tf)` when it is younger than `max_age` and has
    /// enough bars to evaluate a strategy.
    pub fn live_bars_for(&self, sec_id: i64, tf: &str, max_age: Duration) -> Option<Vec<Candle>> {
        let key = format!("{sec_id}:{tf}");
        let bars = self.live_bars.lock().ok()?;
        let lb = bars.get(&key)?;
        if lb.candles.len() >= 3 && lb.updated.elapsed() <= max_age {
            Some(lb.candles.clone())
        } else {
            None
        }
    }

    /// Advance every live series tracking `sec_id` with a fresh trade price.
    /// `now_ist` is the IST wall-clock second the candle timestamps use.
    pub fn patch_tick(&self, sec_id: i64, price: f64, now_ist: i64) {
        if sec_id <= 0 || price <= 0.0 {
            return;
        }
        // No candle forms before the regular session opens (IST 09:15). The
        // pre-open call auction streams indicative ticks that would otherwise
        // create a pre-market bar the real chart never shows.
        if !ist_session_started(now_ist) {
            return;
        }
        let keys = match self.bar_index.lock() {
            Ok(idx) => match idx.get(&sec_id) {
                Some(k) if !k.is_empty() => k.clone(),
                _ => {
                    self.tick_notify.notify_waiters();
                    return;
                }
            },
            Err(_) => return,
        };
        if let Ok(mut bars) = self.live_bars.lock() {
            for key in keys {
                let tf = key.rsplit(':').next().unwrap_or("");
                let step = tf_bucket_secs(tf);
                if step <= 0 {
                    continue;
                }
                let Some(lb) = bars.get_mut(&key) else { continue };
                let bucket = now_ist - now_ist.rem_euclid(step);
                match lb.candles.last_mut() {
                    Some(last) if last.time == bucket => {
                        last.close = price;
                        if price > last.high {
                            last.high = price;
                        }
                        if price < last.low {
                            last.low = price;
                        }
                    }
                    Some(last) if bucket > last.time => {
                        lb.candles.push(Candle {
                            time: bucket,
                            open: price,
                            high: price,
                            low: price,
                            close: price,
                            volume: 0.0,
                        });
                        if lb.candles.len() > 5000 {
                            let n = lb.candles.len();
                            lb.candles.drain(0..n - 5000);
                        }
                    }
                    // Out-of-order tick: keep the bar, ignore the price.
                    _ => {}
                }
                lb.updated = Instant::now();
            }
        }
        self.tick_notify.notify_waiters();
    }

    /// Claim the right to start one upstream snapshot. `false` while a fetch is
    /// already in flight or before `min` has elapsed since the last one.
    pub fn try_rest(&self, min: Duration) -> bool {
        let Ok(mut g) = self.rest_gate.lock() else {
            return false;
        };
        if g.busy {
            return false;
        }
        if let Some(t) = g.last {
            if t.elapsed() < min {
                return false;
            }
        }
        g.busy = true;
        true
    }

    /// Release the gate and record completion time.
    pub fn rest_finished(&self) {
        if let Ok(mut g) = self.rest_gate.lock() {
            g.busy = false;
            g.last = Some(Instant::now());
        }
    }

    /// Register option strikes (or any ad-hoc security) so the live feed
    /// subscribes to them. Deduplicated by `(security_id, exchange_segment)`.
    pub fn register_extra(&self, secs: &[(i64, String)]) {
        let Ok(mut g) = self.extra.lock() else { return };
        for (sid, exch) in secs {
            if *sid == 0 {
                continue;
            }
            let entry = (*sid, exch.to_uppercase());
            if !g.contains(&entry) {
                g.push(entry);
            }
        }
    }

    pub fn extra_secs(&self) -> Vec<(i64, String)> {
        self.extra.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Insert/overwrite one quote without broadcasting.
    ///
    /// Carries the *previous* print's LTP forward as `prev_ltp` so consumers can
    /// read a premium's own tick-to-tick direction (the "+/-" of the LTP itself)
    /// without ever recomputing a change against a previous close.
    pub fn set_quote(&self, key: String, value: Value) {
        let Ok(mut g) = self.quotes.lock() else { return };
        let mut value = value;
        if let Some(ltp) = value.get("ltp").and_then(|v| v.as_f64()) {
            if ltp > 0.0 {
                let prev = g
                    .get(&key)
                    .and_then(|e| e.get("ltp"))
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                if prev > 0.0 && prev != ltp {
                    if let Some(obj) = value.as_object_mut() {
                        obj.insert("prev_ltp".to_string(), json!(prev));
                    }
                }
            }
        }
        g.insert(key, value);
    }

    /// Push a batch of quotes to every `/ws` subscriber.
    pub fn broadcast_quotes(&self, map: &HashMap<String, Value>) {
        if map.is_empty() {
            return;
        }
        let payload = json!({ "type": "quotes", "data": map }).to_string();
        let _ = self.bc.send(payload);
    }
}

impl Default for MarketState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct QuoteReq {
    #[serde(default)]
    pub securities: Vec<QuoteSec>,
}

#[derive(Deserialize)]
pub struct QuoteSec {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub exchange_segment: String,
}

pub async fn symbols() -> impl IntoResponse {
    axum::Json(catalog_json())
}

pub async fn commodities() -> impl IntoResponse {
    axum::Json(json!({ "status": "success", "data": commodities_json() }))
}

/// Grouped instrument catalogue for the testing-only "Manual Strike Select"
/// picker: cash indices, F&O underlyings and MCX commodities that have listed
/// options. Engine-agnostic, so the real and paper tabs share the exact lists.
pub async fn instruments() -> impl IntoResponse {
    let sc = crate::scrip::get();
    let has_options = |name: &str, seg: &str| -> bool {
        match sc.as_ref() {
            Some(s) => {
                let prefix = crate::scrip::fno_underlying(name);
                !prefix.is_empty() && s.has_options(&prefix, crate::scrip::scrip_exch(seg)) != Some(false)
            }
            // Master still warming: list everything and let the chain endpoint
            // answer "no options" for the rare non-derivative index.
            None => true,
        }
    };
    let cat: RawCatalog = serde_json::from_str(catalog_str()).unwrap_or(RawCatalog {
        symbols: Vec::new(),
        watchlists: HashMap::new(),
    });
    let mut indices: Vec<Value> = Vec::new();
    let mut fno: Vec<Value> = Vec::new();
    for row in &cat.symbols {
        let Some(arr) = row.as_array() else { continue };
        let name = arr.first().and_then(|v| v.as_str()).unwrap_or("");
        let sid = arr.get(1).and_then(|v| v.as_i64()).unwrap_or(0);
        let seg = arr.get(2).and_then(|v| v.as_str()).unwrap_or("NSE_EQ");
        if name.is_empty() || sid <= 0 || !has_options(name, seg) {
            continue;
        }
        let item = json!({ "name": name, "security_id": sid, "exchange_segment": seg });
        if arr.iter().any(|v| v.as_str() == Some("Indices")) {
            indices.push(item);
        } else if seg == "NSE_EQ" || seg == "BSE_EQ" {
            fno.push(item);
        }
    }
    let mut commodities: Vec<Value> = Vec::new();
    for c in commodity_rows() {
        if c.has_options {
            commodities.push(json!({
                "name": c.name,
                "security_id": c.security_id,
                "exchange_segment": "MCX_COMM",
                "lot": c.lot,
            }));
        }
    }
    axum::Json(json!({
        "status": "success",
        "data": { "indices": indices, "fno": fno, "commodities": commodities }
    }))
}

pub async fn quotes_post(
    State(st): State<DhanState>,
    axum::Json(req): axum::Json<QuoteReq>,
) -> impl IntoResponse {
    let mk = st.market.clone();
    // Quotes come only from a connected Dhan session. Without one the snapshot
    // is empty - never synthetic data.
    let list: Vec<(i64, String)> = req
        .securities
        .iter()
        .filter(|s| s.security_id > 0)
        .map(|s| (s.security_id, s.exchange_segment.clone()))
        .collect();
    // Bookkeeping is cheap and synchronous; the slow Dhan REST round-trip is not.
    // Remember the watch set so the daily-candle backfill can fill change_pct
    // for symbols Dhan reports with net_change == 0 (weekends / pre-open).
    st.register_watch(&list);
    // Any requested security (including option strikes) joins the live-feed
    // subscription set.
    mk.register_extra(&list);
    // Serve the cached snapshot immediately and refresh from Dhan in the
    // background (the old app returned cache + refreshed asynchronously), so the
    // sidebar never waits ~2-3s per poll.
    if st.is_connected().await && !list.is_empty() && mk.try_rest(Duration::from_millis(700)) {
        let st2 = st.clone();
        let mk2 = mk.clone();
        tokio::spawn(async move {
            if let Some(real) = st2.fetch_rest_quotes(&list).await {
                let mut accepted: HashMap<String, Value> = HashMap::new();
                for (k, v) in &real {
                    // A REST row with no derived close carries no previous close, so it
                    // must never overwrite a good value already seeded from daily
                    // candles (that clobber is exactly what zeroed the sidebar after
                    // hours). Mirror the old app, which only wrote `close > 0` rows.
                    let new_close = v.get("close").and_then(|c| c.as_f64()).unwrap_or(0.0);
                    if new_close <= 0.0 {
                        let existing_close = mk2
                            .quotes
                            .lock()
                            .ok()
                            .and_then(|g| g.get(k).and_then(|e| e.get("close")).and_then(|c| c.as_f64()))
                            .unwrap_or(0.0);
                        if existing_close > 0.0 {
                            continue;
                        }
                    }
                    mk2.set_quote(k.clone(), v.clone());
                    accepted.insert(k.clone(), v.clone());
                }
                mk2.broadcast_quotes(&accepted);
            }
            mk2.rest_finished();
        });
    }
    let map: HashMap<String, Value> = mk
        .quotes
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    let data = serde_json::to_value(map).unwrap_or_else(|_| json!({}));
    axum::Json(json!({ "status": "success", "data": data, "auth_error": Value::Null }))
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(st): State<DhanState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_loop(socket, st))
}

/// Client -> server frame asking the live feed to cover a set of securities.
#[derive(Deserialize)]
struct WsSubscribe {
    #[serde(default)]
    securities: Vec<QuoteSec>,
}

async fn ws_loop(mut socket: WebSocket, st: DhanState) {
    use futures_util::StreamExt;
    let mk = st.market.clone();
    let mut rx = mk.bc.subscribe();
    // Send the current snapshot immediately so the client never waits a full tick.
    let snapshot = {
        let data = mk
            .quotes
            .lock()
            .map(|g| Value::Object(g.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
            .unwrap_or_else(|_| json!({}));
        json!({ "type": "quotes", "data": data }).to_string()
    };
    if socket.send(Message::Text(snapshot)).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = socket.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        // The client declares the securities it wants on screen;
                        // funnel them into the live feed (no REST polling).
                        if let Ok(req) = serde_json::from_str::<WsSubscribe>(&text) {
                            let list: Vec<(i64, String)> = req
                                .securities
                                .iter()
                                .filter(|s| s.security_id > 0)
                                .map(|s| (s.security_id, s.exchange_segment.clone()))
                                .collect();
                            if !list.is_empty() {
                                st.register_watch(&list);
                                st.subscribe_options(&list).await;
                            }
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        let _ = socket.send(Message::Pong(p)).await;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(msg) => {
                        if socket.send(Message::Text(msg)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ist(h: i64, m: i64) -> i64 {
        h * 3600 + m * 60
    }

    #[test]
    fn no_candles_before_regular_session_open() {
        // Pre-open call auction (09:00-09:14:59) must not form a candle.
        assert!(!ist_session_started(ist(9, 0)));
        assert!(!ist_session_started(ist(9, 14) + 59));
        assert!(!ist_session_started(ist(8, 59) + 59));
        assert!(!ist_session_started(0));
        // Regular session onward forms candles, through the MCX evening close.
        assert!(ist_session_started(ist(9, 15)));
        assert!(ist_session_started(ist(15, 30)));
        assert!(ist_session_started(ist(23, 30)));
    }

    #[test]
    fn patch_tick_ignores_pre_market_window() {
        let m = MarketState::new();
        // Seed a previous-day last bar so the series is tick-patchable.
        let seed = vec![
            Candle { time: 86_400 + ist(9, 15), open: 100.0, high: 101.0, low: 99.0, close: 100.5, volume: 0.0 },
            Candle { time: 86_400 + ist(9, 16), open: 100.5, high: 102.0, low: 100.0, close: 101.0, volume: 0.0 },
            Candle { time: 86_400 + ist(9, 17), open: 101.0, high: 101.5, low: 100.5, close: 101.2, volume: 0.0 },
        ];
        m.seed_bars(7, "1min", seed.clone());
        // A pre-open tick must leave the series untouched.
        m.patch_tick(7, 555.0, 86_400 + ist(9, 5));
        let before = m.live_bars_for(7, "1min", Duration::from_secs(60)).unwrap();
        assert_eq!(before.len(), seed.len());
        assert_eq!(before.last().unwrap().close, 101.2);
        // The first post-open tick advances the series.
        m.patch_tick(7, 106.0, 86_400 + ist(9, 18));
        let after = m.live_bars_for(7, "1min", Duration::from_secs(60)).unwrap();
        assert_eq!(after.last().unwrap().close, 106.0);
    }
}

//! DhanHQ session management: credentials, validation, the live market feed and
//! the `/api/connect`, `/api/status`, `/api/feed/*` routes the header bar uses.
//!
//! The SDK lives in the `dhan-hq` crate; this module owns the runtime session
//! (which is intentionally in-memory only) and bridges live ticks into the
//! shared [`MarketState`] quote cache.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use dhan_hq::{
    CandleRow, DhanClient, DhanError, ExchangeSegment, FeedCommand, FeedMode, FeedPacket,
    FeedSubscription, Instrument, IntradayRequest, HistoricalRequest, MarketFeed, OptionChainData,
    OptionChainRequest,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, RwLock};

use algo_core::model::Candle;

use crate::market::{quote_key, securities, MarketState};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Session {
    client: DhanClient,
    client_id: String,
}

/// Consecutive feed socket failures that trip the circuit-breaker.
const MAX_FAIL_STREAK: u32 = 5;
/// Quiet period (seconds) the circuit-breaker parks the feed for so Dhan can
/// clear a rate-limit block instead of being hammered every few seconds.
const LONG_PARK_SECS: u64 = 300;

#[derive(Default)]
struct FeedHealth {
    ws_running: bool,
    feed_up: bool,
    subscribed: usize,
    last_tick: Option<Instant>,
    /// Last time ANY frame arrived on the socket (a trade packet, a PrevClose/OI
    /// packet, or a websocket heartbeat). Dhan pings every ~10s, so this stays
    /// fresh for a live socket even when the market is closed and no ticks flow.
    /// The watchdog uses this, not `last_tick`, to decide a socket is stalled.
    last_activity: Option<Instant>,
    /// When the current feed supervisor last (re)started. Lets the watchdog tell
    /// a socket that has simply not ticked yet from one that has gone silent.
    feed_started: Option<Instant>,
    /// True while the supervisor currently holds an open, handshaken socket that
    /// has not dropped yet. Stays false during reconnect backoff, so the
    /// watchdog can avoid resetting a growing backoff.
    socket_open: bool,
    /// When the current socket was opened (for the connected-but-silent check).
    connected_at: Option<Instant>,
    /// Consecutive dropped/failed sockets since the last successful stream. When
    /// Dhan rate-limits the client id it accepts a handshake, sends a packet or
    /// two and then goes silent, so a naive reconnect flaps every few seconds and
    /// keeps the block alive. Past a small threshold the consumer parks the feed
    /// for a long quiet period instead of hammering (see `LONG_PARK`).
    fail_streak: u32,
    parked_until: Option<Instant>,
    ws_task: Option<tokio::task::JoinHandle<()>>,
}

/// Aborts the wrapped task when dropped. A bare `tokio::task::JoinHandle`
/// merely detaches on drop, so aborting the feed supervisor used to leave the
/// inner `MarketFeed` websocket task running - leaking a Dhan connection slot
/// until Dhan timed it out (which then made later handshakes fail and Connect
/// appear to need a second click).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl FeedHealth {
    fn snapshot(&self) -> Value {
        let age = self
            .last_tick
            .map(|t| (t.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        let parked = self
            .parked_until
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs_f64())
            .unwrap_or(0.0);
        json!({
            "status": "success",
            "ws_running": self.ws_running && self.ws_task.as_ref().map(|t| !t.is_finished()).unwrap_or(false),
            "feed_up": self.feed_up,
            "subscribed": self.subscribed,
            "persist": self.subscribed,
            "last_tick_age_sec": age,
            "last_activity_age_sec": self
                .last_activity
                .map(|t| (t.elapsed().as_secs_f64() * 10.0).round() / 10.0),
            "feed_started_age_sec": self
                .feed_started
                .map(|t| t.elapsed().as_secs_f64())
                .map(|v| (v * 10.0).round() / 10.0),
            "reconnect_parked_sec": (parked * 10.0).round() / 10.0,
        })
    }
}

/// Runtime DhanHQ session + feed health. Holds only an in-memory access token;
/// nothing is written to disk.
#[derive(Clone)]
pub struct DhanState {
    pub market: MarketState,
    session: Arc<RwLock<Option<Session>>>,
    health: Arc<Mutex<FeedHealth>>,
    auth_error: Arc<Mutex<Option<String>>>,
    /// Every `(security_id, exchange_segment)` the browser has asked a quote for.
    /// The daily-candle backfill walks this set to fill change / change_pct for
    /// symbols Dhan's `/marketfeed/quote` reports with `net_change == 0`
    /// (weekends, holidays and pre-open, when the day-close equals the LTP).
    watch: Arc<Mutex<Vec<(i64, String)>>>,
    /// Serialises every outbound Dhan data call (quote / candle / option chain /
    /// portfolio) and keeps them >= ~1/s apart. Dhan rate-limits these surfaces
    /// together per user (DH-904 / error 805 "too many requests"); running the
    /// surfaces in parallel trips it and risks the account being blocked, so a
    /// single global slot is deliberate. Entry latency is instead cut by caching
    /// every LTP the entry path already fetched (see the option-leg resolver)
    /// so it makes the fewest possible calls.
    gate: Arc<tokio::sync::Mutex<Instant>>,
    /// Separate, much faster gate for the ORDER APIs. Dhan's order endpoints
    /// (place / modify / cancel / super / slice) have their own per-second budget
    /// that is far above the 1/s DATA budget (quotes / candles). Routing orders
    /// through the shared 1/s data gate made every entry wait behind in-flight
    /// chart and scanner candle fetches, so the fill landed seconds after the
    /// signal and at a much worse price. Orders get their own ~60ms gate.
    order_gate: Arc<tokio::sync::Mutex<Instant>>,
    /// Live control channel to the running feed, so newly registered option
    /// strikes are added with a subscribe message instead of reopening the
    /// socket (Dhan allows only a handful of concurrent feeds).
    feed_tx: Arc<Mutex<Option<mpsc::UnboundedSender<FeedCommand>>>>,
    /// Serialises `spawn_feed_task`. Two triggers (the watchdog and the browser's
    /// reconnect nudge) can fire at once and run on different runtime threads;
    /// without this lock both opened a supervisor, the second overwrote the first
    /// task handle (so the first was never aborted) and the account ended up with
    /// two concurrent Dhan feeds - exactly what trips Dhan's per-account
    /// connection cap and yields "client id is blocked" 429s.
    feed_spawn_lock: Arc<Mutex<()>>,
    /// Previous-session close per quote key, seeded from the REST quote API and
    /// the daily-candle backfill. Dhan's feed never sends a PrevClose packet, so
    /// the live loop reads this map to compute a real change / change_pct on
    /// every tick instead of publishing close = 0 (which used to clobber the
    /// sidebar's gain/loss column).
    closes: Arc<Mutex<HashMap<String, f64>>>,
    /// Last good candle series per `security:exchange:instrument:timeframe`.
    /// The chart, the engine scanners and the data pool all ask for the same
    /// series; caching the result coalesces those duplicate calls and, more
    /// importantly, lets a transient Dhan error (DH-906 token refresh / 429
    /// rate limit) serve the previous bars instead of blanking the chart.
    candle_cache: Arc<Mutex<HashMap<String, (Instant, Vec<Candle>)>>>,
    /// Monotonic time of the last operator-facing data request (chart candles,
    /// option chain, sidebar quotes). Every Dhan call shares one ~1/s slot, so
    /// the background pollers (account / movers / NIFTY scan / prev-close seeds)
    /// read this and skip their cycle while it is fresh - the operator's click
    /// then waits at most for the one in-flight call instead of queueing behind
    /// a wall of scanner fetches.
    last_user: Arc<Mutex<Instant>>,
}

impl DhanState {
    pub fn new(market: MarketState) -> Self {
        let st = Self {
            market,
            session: Arc::new(RwLock::new(None)),
            health: Arc::new(Mutex::new(FeedHealth::default())),
            auth_error: Arc::new(Mutex::new(None)),
            watch: Arc::new(Mutex::new(Vec::new())),
            gate: Arc::new(tokio::sync::Mutex::new(
                Instant::now() - Duration::from_secs(5),
            )),
            order_gate: Arc::new(tokio::sync::Mutex::new(
                Instant::now() - Duration::from_secs(5),
            )),
            feed_tx: Arc::new(Mutex::new(None)),
            feed_spawn_lock: Arc::new(Mutex::new(())),
            closes: Arc::new(Mutex::new(HashMap::new())),
            candle_cache: Arc::new(Mutex::new(HashMap::new())),
            last_user: Arc::new(Mutex::new(Instant::now() - Duration::from_secs(60))),
        };
        st.spawn_daily_fill();
        st.spawn_seed_prev_close();
        st
    }

    /// Record the securities a `/api/quotes` poll asked for so the daily-candle
    /// backfill covers them. Deduplicated; order is preserved.
    pub fn register_watch(&self, secs: &[(i64, String)]) {
        let Ok(mut g) = self.watch.lock() else { return };
        for (sid, exch) in secs {
            if *sid <= 0 {
                continue;
            }
            let entry = (*sid, exch.to_uppercase());
            if !g.contains(&entry) {
                g.push(entry);
            }
        }
    }

    /// Register option strikes with the synthetic ticker / REST watch set and,
    /// when the live feed is running, push them over the feed's control channel
    /// so they start streaming without reopening the websocket.
    pub async fn subscribe_options(&self, secs: &[(i64, String)]) {
        if secs.is_empty() {
            return;
        }
        self.market.register_extra(secs);
        self.register_watch(secs);
        let subs: Vec<FeedSubscription> = secs
            .iter()
            .filter(|(sid, _)| *sid > 0)
            .filter_map(|(sid, exch)| {
                exchange_segment(exch)
                    .map(|seg| FeedSubscription::with_mode(seg, *sid, FeedMode::Full))
            })
            .collect();
        if subs.is_empty() {
            return;
        }
        if let Ok(g) = self.feed_tx.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(FeedCommand::Subscribe(subs));
            }
        }
    }

    /// Synchronous, deduplicated "make sure these strikes are streaming" used by
    /// the entry hot path (the "+green premium" scan) so its candidate window can
    /// be quoted without awaiting a REST/feed round-trip. Returns immediately;
    /// the strikes' ticks land on the websocket on the following packets.
    pub fn watch_options_now(&self, secs: &[(i64, String)]) {
        if secs.is_empty() {
            return;
        }
        self.market.register_extra(secs);
        let mut fresh: Vec<(i64, String)> = Vec::new();
        {
            let Ok(mut g) = self.watch.lock() else { return };
            for (sid, exch) in secs {
                if *sid <= 0 {
                    continue;
                }
                let entry = (*sid, exch.to_uppercase());
                if !g.contains(&entry) {
                    g.push(entry.clone());
                    fresh.push(entry);
                }
            }
        }
        if fresh.is_empty() {
            return;
        }
        let subs: Vec<FeedSubscription> = fresh
            .iter()
            .filter_map(|(sid, exch)| {
                exchange_segment(exch).map(|seg| FeedSubscription::with_mode(seg, *sid, FeedMode::Full))
            })
            .collect();
        if subs.is_empty() {
            return;
        }
        if let Ok(g) = self.feed_tx.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(FeedCommand::Subscribe(subs));
            }
        }
    }

    /// Block until Dhan's data-API budget allows another request. Dhan meters the
    /// data endpoints together (~1/s per user on this account, error 805), so a
    /// single global slot is the only safe design; the entry path avoids extra
    /// calls by caching every LTP it already fetched.
    async fn throttle(&self) {
        let mut last = self.gate.lock().await;
        let wait = Duration::from_millis(1050);
        let elapsed = last.elapsed();
        if elapsed < wait {
            tokio::time::sleep(wait - elapsed).await;
        }
        *last = Instant::now();
    }

    /// Alias kept for the historical candle call sites. Dhan meters all data
    /// surfaces through the same budget, so these share the global slot.
    async fn hist_throttle(&self) {
        self.throttle().await;
    }

    /// Alias kept for the option-chain / expiry call sites (shared global slot).
    async fn oc_throttle(&self) {
        self.throttle().await;
    }

    /// Alias kept for the portfolio call sites (shared global slot).
    async fn acct_throttle(&self) {
        self.throttle().await;
    }

    /// Fast gate for order-API calls: keeps them clear of the 1/s data-API
    /// queue while still spacing bursts (Dhan allows many orders per second).
    async fn order_throttle(&self) {
        let mut last = self.order_gate.lock().await;
        let wait = Duration::from_millis(60);
        let elapsed = last.elapsed();
        if elapsed < wait {
            tokio::time::sleep(wait - elapsed).await;
        }
        *last = Instant::now();
    }

    async fn client(&self) -> Option<DhanClient> {
        self.session.read().await.as_ref().map(|s| s.client.clone())
    }

    async fn client_id(&self) -> Option<String> {
        self.session.read().await.as_ref().map(|s| s.client_id.clone())
    }

    /// Public session accessors for the realtime trading engine.
    pub async fn session_client(&self) -> Option<DhanClient> {
        self.client().await
    }

    pub async fn session_client_id(&self) -> Option<String> {
        self.client_id().await
    }

    pub async fn is_connected(&self) -> bool {
        self.session.read().await.is_some()
    }

    /// True while the broker websocket is actually streaming (the first packet
    /// has been seen). The realtime engine uses the false -> true edge to detect
    /// a reconnect and resync its book with the broker, so a trade whose native
    /// stop leg fired - or a manual fill - while we were blind is not missed.
    pub fn feed_up(&self) -> bool {
        self.health.lock().map(|g| g.feed_up).unwrap_or(false)
    }

    /// True only while the broker socket is streaming AND a tick landed within
    /// the last 15s. This is the single gate the sidebar refresh, the previous
    /// close backfill and the trading engine use, so cached / REST / historical
    /// values can never be acted on while the live feed is silent, closed or
    /// disconnected.
    pub fn feed_live(&self) -> bool {
        let Ok(g) = self.health.lock() else {
            return false;
        };
        g.feed_up
            && g.last_tick
                .map(|t| t.elapsed().as_secs_f64() < 15.0)
                .unwrap_or(false)
    }

    /// Record an operator-facing data request (chart candles / option chain /
    /// sidebar quotes) so the background pollers yield Dhan's single data slot
    /// to it for the next moment.
    pub fn mark_user_activity(&self) {
        if let Ok(mut g) = self.last_user.lock() {
            *g = Instant::now();
        }
    }

    /// True while an operator-facing data request arrived within the last
    /// ~1.8s. Background pollers return early while this holds; the 100ms engine
    /// tick re-checks, so they resume the instant the operator goes quiet.
    pub fn user_active(&self) -> bool {
        self.last_user
            .lock()
            .map(|g| g.elapsed() < Duration::from_millis(1800))
            .unwrap_or(false)
    }

    /// Serialised, rate-limited Dhan REST call gate (shared with the chart).
    /// This is the QUOTE slot (marketfeed quote / ltp) at ~1/s.
    pub async fn dhan_throttle(&self) {
        self.throttle().await;
    }

    /// Historical candle slot (~3/s) for the realtime engine's candle fetches.
    pub async fn dhan_hist_throttle(&self) {
        self.hist_throttle().await;
    }

    /// Option-chain / expiry slot (~1 call / 3s).
    pub async fn dhan_oc_throttle(&self) {
        self.oc_throttle().await;
    }

    /// Portfolio slot (~1/s) for positions / funds / holdings / margin.
    pub async fn dhan_acct_throttle(&self) {
        self.acct_throttle().await;
    }

    /// Fast order-API gate: use for place / modify / cancel / super orders so an
    /// entry never waits behind background candle fetches.
    pub async fn dhan_order_throttle(&self) {
        self.order_throttle().await;
    }

    fn set_auth_error(&self, msg: Option<String>) {
        if let Ok(mut g) = self.auth_error.lock() {
            *g = msg;
        }
    }

    // -----------------------------------------------------------------------
    // Candles (real Dhan data when connected)
    // -----------------------------------------------------------------------

    /// Fetch candles for the chart. Returns `Err` when not connected or when
    /// Dhan rejects the request, so the caller can fall back to synthetic data.
    pub async fn fetch_candles(
        &self,
        sec_id: i64,
        exch: &str,
        inst: &str,
        tf: &str,
    ) -> Result<Vec<Candle>, DhanError> {
        let key = format!(
            "{sec_id}:{}:{}:{tf}",
            exch.to_uppercase(),
            inst.to_uppercase()
        );
        // A just-fetched series serves the chart, the engine scanners and the
        // data pool without a second Dhan call. 4s window so a symbol switch
        // back and forth (and the chart's own reload) reuses the last bars
        // instead of spending another 1/s data slot.
        if let Some(fresh) = self.cached_candles(&key, Duration::from_secs(4)) {
            return Ok(fresh);
        }
        let client = self.client().await.ok_or(DhanError::NotConnected)?;
        let segment = exchange_segment(exch)
            .ok_or_else(|| DhanError::Message(format!("unknown exchange segment: {exch}")))?;
        let instrument = instrument_type(inst);
        let ist_now = now_secs() + 19800;

        // Two attempts: Dhan answers a transient DH-906 while a token refresh is
        // in flight and 429 under short bursts, so one spaced retry recovers most
        // chart loads instead of showing an empty series.
        let mut last_err: Option<DhanError> = None;
        for attempt in 0..2 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            let rows: Vec<CandleRow> = match timeframe_plan(tf) {
                TfPlan::Intraday { interval, .. } => {
                    let from = ist_now - 30 * 86400;
                    let mut rows = match self
                        .intraday_rows(&client, sec_id, segment, instrument, interval, from, ist_now)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            last_err = Some(e);
                            continue;
                        }
                    };
                    // Dhan distinguishes index options (OPTIDX) from stock options
                    // (OPTSTK); sending the wrong one yields an empty payload or a
                    // single junk bar - the old app saw exactly one "fat candle" from
                    // this. Retry the alternate type and keep whichever has more bars
                    // (port of old `fetch_intraday_candles` fallback).
                    if rows.len() <= 2 {
                        if let Some(alt) = alt_option_instrument(instrument) {
                            if let Ok(alt_rows) = self
                                .intraday_rows(&client, sec_id, segment, alt, interval, from, ist_now)
                                .await
                            {
                                if alt_rows.len() > rows.len() {
                                    rows = alt_rows;
                                }
                            }
                        }
                    }
                    rows
                }
                TfPlan::Daily => self.daily_rows(&client, sec_id, segment, instrument).await,
            };

            if rows.is_empty() {
                last_err = Some(DhanError::Invalid("Dhan returned no candles".into()));
                continue;
            }
            // Dhan timestamps are UTC epoch; shift to the IST wall clock the rest
            // of the app (and the synthetic generator) uses.
            let mut shifted: Vec<CandleRow> = rows
                .into_iter()
                .map(|mut r| {
                    r.time += 19800;
                    r
                })
                .collect();
            shifted.sort_by_key(|r| r.time);

            let step = tf_step_secs(tf);
            let out = if let TfPlan::Intraday { resample, .. } = timeframe_plan(tf) {
                match resample {
                    Some(secs) => resample_candles(&shifted, secs),
                    None => to_candles(shifted),
                }
            } else {
                resample_candles(&shifted, step)
            };
            if !out.is_empty() {
                if let Ok(mut c) = self.candle_cache.lock() {
                    c.insert(key.clone(), (Instant::now(), out.clone()));
                }
                return Ok(out);
            }
            last_err = Some(DhanError::Invalid("no candles after resample".into()));
        }

        // Both attempts failed. Rather than blank the chart, serve the last good
        // series (bounded, so a long-disconnected session still surfaces an
        // error) — mirrors the old app keeping the previous grid on screen.
        if let Some(stale) = self.cached_candles(&key, Duration::from_secs(900)) {
            return Ok(stale);
        }
        Err(last_err.unwrap_or(DhanError::NotConnected))
    }

    /// One Dhan `/charts/intraday` call, throttled, returning the raw rows.
    async fn intraday_rows(
        &self,
        client: &DhanClient,
        sec_id: i64,
        segment: ExchangeSegment,
        instrument: Instrument,
        interval: u32,
        from: i64,
        to: i64,
    ) -> Result<Vec<CandleRow>, DhanError> {
        let req = IntradayRequest {
            security_id: sec_id.to_string(),
            exchange_segment: segment,
            instrument,
            interval: interval.to_string(),
            oi: false,
            from_date: fmt_datetime(from),
            to_date: fmt_datetime(to),
        };
        self.hist_throttle().await;
        Ok(client.intraday(&req).await?.rows())
    }

    /// Cached candle series for `key` when younger than `max_age`.
    fn cached_candles(&self, key: &str, max_age: Duration) -> Option<Vec<Candle>> {
        let g = self.candle_cache.lock().ok()?;
        let (at, v) = g.get(key)?;
        if !v.is_empty() && at.elapsed() <= max_age {
            Some(v.clone())
        } else {
            None
        }
    }

    // -----------------------------------------------------------------------
    // Option chain (real Dhan data when connected)
    // -----------------------------------------------------------------------

    /// Dhan's option-chain expiry list for an underlying. `Err` when not
    /// connected or rejected, so the caller can fall back to synthetic dates.
    pub async fn fetch_option_expiries(
        &self,
        sec_id: i64,
        exch: &str,
    ) -> Result<Vec<String>, DhanError> {
        let client = self.client().await.ok_or(DhanError::NotConnected)?;
        let segment = exchange_segment(exch)
            .ok_or_else(|| DhanError::Message(format!("unknown exchange segment: {exch}")))?;
        self.oc_throttle().await;
        let req = OptionChainRequest {
            underlying_scrip: sec_id,
            underlying_seg: segment,
            expiry: None,
        };
        client.option_chain_expiry_list(&req).await
    }

    /// Dhan's option chain for one expiry.
    pub async fn fetch_option_chain(
        &self,
        sec_id: i64,
        exch: &str,
        expiry: &str,
    ) -> Result<OptionChainData, DhanError> {
        let client = self.client().await.ok_or(DhanError::NotConnected)?;
        let segment = exchange_segment(exch)
            .ok_or_else(|| DhanError::Message(format!("unknown exchange segment: {exch}")))?;
        self.oc_throttle().await;
        let req = OptionChainRequest {
            underlying_scrip: sec_id,
            underlying_seg: segment,
            expiry: Some(expiry.to_string()),
        };
        client.option_chain(&req).await
    }

    // -----------------------------------------------------------------------
    // REST quotes (market feed snapshot)

    // -----------------------------------------------------------------------

    /// Fetch a REST market-quote snapshot for the requested `(security_id,
    /// exchange_segment)` pairs. Returns `None` when not connected or when Dhan
    /// rejects/returns nothing, so callers can fall back to the cached/synthetic
    /// quote surface. Unlike the websocket feed this still returns the last
    /// traded price when the market is closed.
    pub async fn fetch_rest_quotes(
        &self,
        secs: &[(i64, String)],
    ) -> Option<HashMap<String, Value>> {
        let client = self.client().await?;
        let mut groups: std::collections::BTreeMap<String, Vec<i64>> =
            std::collections::BTreeMap::new();
        for (sid, exch) in secs {
            if *sid <= 0 {
                continue;
            }
            groups.entry(exch.to_uppercase()).or_default().push(*sid);
        }
        if groups.is_empty() {
            return None;
        }
        // Dhan caps `/marketfeed/quote` at 1000 security ids per request
        // ("Requested Number of SecurityId Limit exceeded"). A loaded option
        // chain easily exceeds that, so split each segment into <=1000-id
        // batches and merge the responses instead of failing the whole call.
        const QUOTE_CHUNK: usize = 1000;
        let mut resp: dhan_hq::MarketFeedResponse = std::collections::BTreeMap::new();
        let mut any_ok = false;
        for (seg, ids) in &groups {
            for chunk in ids.chunks(QUOTE_CHUNK) {
                self.throttle().await;
                let one: dhan_hq::SegmentInstruments =
                    std::iter::once((seg.clone(), chunk.to_vec())).collect();
                match client.market_feed_quote(&one).await {
                    Ok(r) => {
                        any_ok = true;
                        for (s, m) in r {
                            resp.entry(s).or_default().extend(m);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Dhan market quote failed (segment={seg}, n={}): {e}",
                            chunk.len()
                        );
                    }
                }
            }
        }
        if !any_ok {
            return None;
        }
        let now = now_secs();
        let mut out: HashMap<String, Value> = HashMap::new();
        for (sid, exch) in secs {
            if *sid <= 0 {
                continue;
            }
            let seg = exch.to_uppercase();
            let Some(q) = resp.get(&seg).and_then(|m| m.get(&sid.to_string())) else {
                continue;
            };
            let ltp = q.last_price;
            if !(ltp > 0.0) {
                continue;
            }
            // Authoritative previous close: Dhan reports `previous_close_price`
            // for many instruments; when it is missing fall back to
            // `ltp - net_change` (only while net_change is non-zero). Never use
            // `ohlc.close` - that is the day close (== ltp during the session),
            // and using it would zero out change / change_pct.
            let change = q.net_change;
            let mut close = q.previous_close_price;
            if close <= 0.0 && change != 0.0 {
                close = ltp - change;
            }
            let pct = if close != 0.0 {
                change / close * 100.0
            } else {
                0.0
            };
            let bid = q
                .depth
                .as_ref()
                .and_then(|d| d.buy.first())
                .map(|l| l.price)
                .unwrap_or(0.0);
            let ask = q
                .depth
                .as_ref()
                .and_then(|d| d.sell.first())
                .map(|l| l.price)
                .unwrap_or(0.0);
            let mut v = json!({
                "ltp": round2(ltp),
                "change": round2(change),
                "close": round2(close),
                "change_pct": round2(pct),
                "at": now,
                "live": 1,
            });
            // Carry the same Full-mode extras the websocket feed publishes, so a
            // freshly loaded option chain shows real OI / Volume / Bid-Ask at once
            // instead of a wall of zeros until the first ticks land.
            if q.volume > 0.0 {
                v["volume"] = json!(round2(q.volume));
            }
            if q.oi > 0.0 {
                v["oi"] = json!(round2(q.oi));
            }
            if bid > 0.0 {
                v["bid"] = json!(round2(bid));
            }
            if ask > 0.0 {
                v["ask"] = json!(round2(ask));
            }
            out.insert(quote_key(*sid, exch), v);
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Batch-seed the shared quote cache for a freshly loaded option chain.
    ///
    /// Mirrors the old app's `_apply_live_quotes` + `_seed_chain_quotes`: a single
    /// `/marketfeed/quote` call fills LTP / change / prev-close / volume / OI /
    /// bid / ask for every CE+PE strike at once, and Black-Scholes fills IV +
    /// delta/theta/gamma/vega. It also resolves the underlying's real spot (the
    /// client never sends one) which both centres the ATM window and feeds the
    /// greeks. Crucially it writes the derived previous close into the live
    /// feed's [`Self::closes`] map, so the websocket tick path computes a real
    /// change instead of zeroing it. Without this the chain only populated via
    /// the shared 1-call/second seed loops, which is why the live columns were
    /// missing / very slow to appear.
    pub async fn seed_chain_quotes(&self, secs: Vec<(i64, String)>, under: i64, under_exch: &str) {
        if self.client().await.is_none() {
            return;
        }
        // One batched call also covers the underlying, so the spot is free.
        let mut fetch = secs.clone();
        if under > 0 && !under_exch.is_empty() {
            fetch.push((under, under_exch.to_string()));
        }
        let Some(real) = self.fetch_rest_quotes(&fetch).await else {
            return;
        };
        // First pass: publish the underlying spot before any greek is derived.
        let under_key = quote_key(under, under_exch);
        if under > 0 {
            if let Some(spot) = real
                .get(&under_key)
                .and_then(|v| v.get("ltp"))
                .and_then(|v| v.as_f64())
            {
                crate::optionchain::set_index_spot(under, spot);
            }
        }
        let mut sid_by_key: HashMap<String, i64> = HashMap::with_capacity(secs.len());
        for (sid, exch) in &secs {
            sid_by_key.insert(quote_key(*sid, exch), *sid);
        }
        let mut accepted: HashMap<String, Value> = HashMap::with_capacity(real.len());
        for (key, mut v) in real {
            if let Some(sid) = sid_by_key.get(&key).copied() {
                let ltp = v.get("ltp").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if let Some(g) = crate::optionchain::opt_greeks(sid, ltp) {
                    if let Some(obj) = v.as_object_mut() {
                        for field in ["iv", "delta", "vega", "gamma", "theta"] {
                            if let Some(val) = g.get(field) {
                                obj.insert(field.to_string(), val.clone());
                            }
                        }
                    }
                }
            }
            let close = v.get("close").and_then(|c| c.as_f64()).unwrap_or(0.0);
            if close > 0.0 {
                if let Ok(mut c) = self.closes.lock() {
                    c.insert(key.clone(), close);
                }
            }
            self.market.set_quote(key.clone(), v.clone());
            accepted.insert(key, v);
        }
        if !accepted.is_empty() {
            self.market.broadcast_quotes(&accepted);
        }
    }

    // -----------------------------------------------------------------------
    // Daily-candle backfill (previous close when the quote API has no change)
    // -----------------------------------------------------------------------

    /// Last two daily closes for a security, oldest->newest (`(last, prev)`).
    ///
    /// The old app derives change_pct this way when Dhan's `/marketfeed/quote`
    /// returns `net_change == 0` (weekends, holidays, pre-open). On a weekend the
    /// last candle is the most recent session (Friday) and the second-to-last is
    /// the session before it (Thursday), so the diff is the correct display value
    /// rather than 0.
    pub async fn fetch_daily_closes(
        &self,
        sec_id: i64,
        exch: &str,
        inst: &str,
    ) -> Option<(f64, f64)> {
        let client = self.client().await?;
        let segment = exchange_segment(exch)?;
        let instrument = instrument_type(inst);
        let mut rows = self.daily_rows(&client, sec_id, segment, instrument).await;
        if rows.len() < 2 {
            return None;
        }
        rows.sort_by_key(|r| r.time);
        let last = rows[rows.len() - 1].close;
        let prev = rows[rows.len() - 2].close;
        if last > 0.0 && prev > 0.0 {
            Some((last, prev))
        } else {
            None
        }
    }

    /// Raw daily candles (no IST shift - the caller shifts) from Dhan's
    /// `/charts/historical`, with a resample fallback. Dhan's daily endpoint
    /// answers DH-905 for several instruments (INDIA VIX, GIFT NIFTY, SENSEX) and
    /// for index requests before the session opens; the old app covered exactly
    /// that case by resampling 15-minute intraday rows into daily bars.
    async fn daily_rows(
        &self,
        client: &DhanClient,
        sec_id: i64,
        segment: ExchangeSegment,
        instrument: Instrument,
    ) -> Vec<CandleRow> {
        let ist_now = now_secs() + 19800;
        self.hist_throttle().await;
        let req = HistoricalRequest {
            security_id: sec_id.to_string(),
            exchange_segment: segment,
            instrument,
            expiry_code: 0,
            oi: false,
            from_date: fmt_date(ist_now - 730 * 86400),
            to_date: fmt_date(ist_now),
        };
        match client.historical(&req).await {
            Ok(d) => {
                let rows = d.rows();
                if rows.len() >= 2 {
                    return rows;
                }
            }
            Err(e) => {
                tracing::debug!("daily historical unavailable sec={sec_id} exch={exch:?}: {e}", exch = segment)
            }
        }
        // Fallback: resample 15-minute intraday rows into IST-day bars.
        self.hist_throttle().await;
        let req = IntradayRequest {
            security_id: sec_id.to_string(),
            exchange_segment: segment,
            instrument,
            interval: "15".to_string(),
            oi: false,
            from_date: fmt_datetime(ist_now - 90 * 86400),
            to_date: fmt_datetime(ist_now),
        };
        let Ok(d) = client.intraday(&req).await else {
            return Vec::new();
        };
        let mut rows = d.rows();
        rows.sort_by_key(|r| r.time);
        let mut out: Vec<CandleRow> = Vec::new();
        let mut day = i64::MIN;
        for r in rows {
            let ist = r.time + 19800;
            let d = ist - ist.rem_euclid(86400);
            if d != day {
                day = d;
                out.push(CandleRow {
                    time: d - 19800,
                    open: r.open,
                    high: r.high,
                    low: r.low,
                    close: r.close,
                    volume: r.volume,
                    open_interest: r.open_interest,
                });
            } else if let Some(c) = out.last_mut() {
                c.high = c.high.max(r.high);
                c.low = c.low.min(r.low);
                c.close = r.close;
                c.volume += r.volume;
            }
        }
        out
    }

    fn spawn_daily_fill(&self) {
        let st = self.clone();
        tokio::spawn(async move { st.daily_fill_loop().await });
    }

    /// Walk the registered watch set and backfill change / change_pct from daily
    /// candles for every symbol that still has no usable close. Runs only while
    /// a session is connected and skips symbols whose cache already carries a
    /// close, so a completed pass costs no further Dhan calls until reconnect.
    async fn daily_fill_loop(&self) {
        // Let the REST quote snapshot seed the cache first (the old app waited
        // 12s) so most symbols are already filled and we never flood Dhan.
        tokio::time::sleep(Duration::from_secs(12)).await;
        loop {
            // Seed as long as a Dhan session exists - a closed market must still
            // show each symbol's previous close / daily change. A disconnected
            // (no-session) state stops this, so nothing is seeded while offline.
            if self.client().await.is_none() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            // Yield Dhan's single data slot while the operator is interacting
            // (chart / option chain / quotes), then resume this pass.
            if self.user_active() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            let mut list = self.watch.lock().map(|g| g.clone()).unwrap_or_default();
            // Backfill the WHOLE sidebar catalog (indices / F&O / commodities),
            // not only the ids a `/api/quotes` poll happened to register. Which
            // ids were registered depended on poll timing, so the daily-change
            // backfill used to cover the sidebar in one session and miss it in
            // the next - the exact "works, then stops" behaviour. The seed loop
            // already unions `securities()`; the daily fill now does too.
            {
                let mut seen: std::collections::HashSet<i64> = list.iter().map(|(s, _)| *s).collect();
                for (sid, exch) in securities() {
                    if seen.insert(sid) {
                        list.push((sid, exch));
                    }
                }
            }
            if list.is_empty() {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            // Indices and commodities first: few entries, most visible, and the
            // old app explicitly prioritised MCX so their rows did not wait for
            // the long NSE_EQ queue.
            list.sort_by_key(|(_, exch)| match exch.as_str() {
                "IDX_I" => 0,
                "MCX_COMM" => 1,
                "NSE_EQ" => 2,
                _ => 3,
            });
            for (sid, exch) in list {
                let key = quote_key(sid, &exch);
                let cached = self
                    .market
                    .quotes
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&key).cloned());
                let is_synth = cached.as_ref().and_then(|v| v.get("synth")).is_some();
                let has_close = !is_synth
                    && cached
                        .as_ref()
                        .and_then(|v| v.get("close"))
                        .and_then(|c| c.as_f64())
                        .map(|c| c > 0.0)
                        .unwrap_or(false);
                if has_close {
                    continue;
                }
                // Option strikes get their previous close from the batched chain
                // seed (`seed_chain_quotes`); the daily-candle path below would
                // fetch them with a futures instrument type and corrupt change.
                // Scanner-discovered options are not in the option-chain map, so
                // also ask the scrip master: a FUTSTK fetch on an option id
                // returns the UNDERLYING's candles, which once poisoned the
                // option's LTP/previous close and booked phantom P&L.
                if crate::optionchain::known_option_sid(sid)
                    || crate::scrip::get().map(|s| s.is_option(sid)).unwrap_or(false)
                {
                    continue;
                }
                let inst = match exch.as_str() {
                    "IDX_I" => "INDEX",
                    "MCX_COMM" => "FUTCOM",
                    "NSE_FNO" | "BSE_FNO" => "FUTSTK",
                    _ => "EQUITY",
                };
                if let Some((last, prev)) = self.fetch_daily_closes(sid, &exch, inst).await {
                    let ltp = cached
                        .as_ref()
                        .filter(|v| v.get("synth").is_none())
                        .and_then(|v| v.get("ltp"))
                        .and_then(|c| c.as_f64())
                        .filter(|v| *v > 0.0)
                        .unwrap_or(last);
                    let chg = ltp - prev;
                    let pct = if prev != 0.0 { chg / prev * 100.0 } else { 0.0 };
                    // Feed the live loop's prev-close map too, otherwise the next
                    // tick would republish close = 0 and undo this backfill.
                    if let Ok(mut c) = self.closes.lock() {
                        c.insert(key.clone(), prev);
                    }
                    let q = json!({
                        "ltp": round2(ltp),
                        "change": round2(chg),
                        "close": round2(prev),
                        "change_pct": round2(pct),
                        "at": now_secs(),
                        "live": 1,
                    });
                    self.market.set_quote(key.clone(), q.clone());
                    let mut one: HashMap<String, Value> = HashMap::new();
                    one.insert(key, q);
                    self.market.broadcast_quotes(&one);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Previous-close seeding (REST quote snapshot)
    // -----------------------------------------------------------------------

    fn spawn_seed_prev_close(&self) {
        let st = self.clone();
        tokio::spawn(async move { st.seed_prev_close_loop().await });
    }

    /// Refresh the shared previous-close map from the REST quote API so every
    /// live tick carries a real change / change_pct, and push the seeded rows so
    /// a quiet symbol (index / commodity) still refreshes in the sidebar. This
    /// mirrors the old app's `_seed_watchlist_prev_close`, which is why its
    /// sidebar showed live change even though the feed delivers no PrevClose
    /// packet.
    async fn seed_prev_close_loop(&self) {
        // Let the session settle before the first call.
        tokio::time::sleep(Duration::from_secs(3)).await;
        loop {
            // Refresh the previous close as long as a Dhan session exists, so a
            // closed market still shows history. Offline (no session) stops it.
            if self.client().await.is_none() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            // Yield Dhan's single data slot while the operator is interacting
            // (chart / option chain / quotes), then resume this pass.
            if self.user_active() {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            let mut list = self.watch.lock().map(|g| g.clone()).unwrap_or_default();
            let mut seen: std::collections::HashSet<i64> = list.iter().map(|(s, _)| *s).collect();
            // Seed the whole sidebar watch set, not only the securities the
            // browser has explicitly asked about, so the first pass covers it.
            for (sid, exch) in securities() {
                if seen.insert(sid) {
                    list.push((sid, exch));
                }
            }
            // Only ask for symbols that still lack a previous close. A prev-close
            // does not change during the session, so re-requesting the whole set
            // every cycle only burns Dhan's ~1 req/s budget and trips 429s that
            // then starve the chart's candle fetch.
            list.retain(|(sid, exch)| {
                let key = quote_key(*sid, exch);
                let cached = self
                    .market
                    .quotes
                    .lock()
                    .ok()
                    .and_then(|g| g.get(&key).cloned());
                let is_synth = cached.as_ref().and_then(|v| v.get("synth")).is_some();
                let has_close = !is_synth
                    && cached
                        .as_ref()
                        .and_then(|v| v.get("close"))
                        .and_then(|c| c.as_f64())
                        .map(|c| c > 0.0)
                        .unwrap_or(false);
                !has_close
            });
            if !list.is_empty() {
                if let Some(real) = self.fetch_rest_quotes(&list).await {
                    let mut accepted: HashMap<String, Value> = HashMap::new();
                    for (k, v) in &real {
                        let close = v.get("close").and_then(|c| c.as_f64()).unwrap_or(0.0);
                        if close > 0.0 {
                            if let Ok(mut c) = self.closes.lock() {
                                c.insert(k.clone(), close);
                            }
                        }
                    }
                    for (k, v) in &real {
                        // A row with no derived close must never overwrite a good
                        // value already seeded from the daily-candle backfill.
                        let close = v.get("close").and_then(|c| c.as_f64()).unwrap_or(0.0);
                        if close <= 0.0 {
                            let existing_close = self
                                .market
                                .quotes
                                .lock()
                                .ok()
                                .and_then(|g| {
                                    g.get(k)
                                        .and_then(|e| e.get("close"))
                                        .and_then(|c| c.as_f64())
                                })
                                .unwrap_or(0.0);
                            if existing_close > 0.0 {
                                continue;
                            }
                        }
                        self.market.set_quote(k.clone(), v.clone());
                        accepted.insert(k.clone(), v.clone());
                    }
                    if !accepted.is_empty() {
                        self.market.broadcast_quotes(&accepted);
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(120)).await;
        }
    }

    // -----------------------------------------------------------------------
    // Live feed lifecycle
    // -----------------------------------------------------------------------

    async fn start_feed(&self, creds: Session) {
        self.stop_feed();

        // Honour an active cooldown: /api/feed/reset parks the feed so Dhan can
        // release its concurrent-connection slots. Without this, every Connect
        // click during the cooldown would reopen the socket and re-trip the
        // limit, so the limit would never clear. /api/feed/restart clears the
        // park explicitly when the user (or the UI) wants to retry.
        if let Ok(g) = self.health.lock() {
            if let Some(until) = g.parked_until {
                if until > Instant::now() {
                    // Schedule one automatic retry for when the cooldown ends,
                    // so a manual Connect during the park is not left stranded.
                    // Calls spawn_feed_task (not start_feed) to avoid an
                    // infinitely recursive async type.
                    let st = self.clone();
                    let wait = until.saturating_duration_since(Instant::now());
                    tokio::spawn(async move {
                        tokio::time::sleep(wait).await;
                        if st.feed_running() {
                            return;
                        }
                        if let Some(s) = st.session.read().await.clone() {
                            if let Ok(mut g) = st.health.lock() {
                                g.parked_until = None;
                            }
                            st.spawn_feed_task(s).await;
                        }
                    });
                    return;
                }
            }
        }

        self.spawn_feed_task(creds).await;
    }

    async fn spawn_feed_task(&self, creds: Session) {
        // Serialise the whole stop-then-spawn so two concurrent callers cannot
        // each leave a live supervisor behind (see `feed_spawn_lock`). There is
        // no await inside this method, so a std mutex guard is safe.
        let _spawn_guard = self
            .feed_spawn_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Single-flight: never let two supervisors run at once. Each one holds a
        // Dhan websocket slot, and Dhan caps concurrent feeds per account, so a
        // stray old supervisor makes the next connect come back rejected.
        self.stop_feed();
        let mut subs = feed_subscriptions();
        // Option strikes registered by the option-chain tab ride along with the
        // static catalog so they get real websocket ticks too.
        for (sid, exch) in self.market.extra_secs() {
            if let Some(seg) = exchange_segment(&exch) {
                if !subs.iter().any(|s| s.security_id == sid.to_string()) {
                    subs.push(FeedSubscription::with_mode(seg, sid, FeedMode::Full));
                }
            }
        }
        let market = self.market.clone();
        let health = self.health.clone();
        let closes = self.closes.clone();
        let client_id = creds.client_id.clone();
        let token = creds.client.access_token().to_string();

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<FeedCommand>();
        if let Ok(mut g) = self.feed_tx.lock() {
            *g = Some(cmd_tx);
        }

        let task = tokio::spawn(async move {
            run_feed(client_id, token, subs, market, health, closes, cmd_rx).await;
        });
        if let Ok(mut g) = self.health.lock() {
            g.ws_task = Some(task);
        }
    }

    fn stop_feed(&self) {
        if let Ok(mut g) = self.feed_tx.lock() {
            *g = None;
        }
        if let Ok(mut g) = self.health.lock() {
            if let Some(t) = g.ws_task.take() {
                t.abort();
            }
            g.ws_running = false;
            g.feed_up = false;
            g.feed_started = None;
            g.socket_open = false;
            g.connected_at = None;
        }
    }

    /// True while a feed supervisor task is alive (including reconnect backoff).
    fn feed_running(&self) -> bool {
        if let Ok(g) = self.health.lock() {
            return g.ws_task.as_ref().map(|t| !t.is_finished()).unwrap_or(false);
        }
        false
    }

    /// True while a `/api/feed/reset` cooldown is still counting down.
    fn park_active(&self) -> bool {
        if let Ok(g) = self.health.lock() {
            return g
                .parked_until
                .map(|until| until > Instant::now())
                .unwrap_or(false);
        }
        false
    }

    /// Counts one feed failure and trips the circuit-breaker once too many
    /// happen in a row. Returns true when it parked the feed, so the caller must
    /// NOT respawn immediately - the watchdog re-spawns only after the park.
    fn note_feed_failure(&self) -> bool {
        if let Ok(mut g) = self.health.lock() {
            g.fail_streak = g.fail_streak.saturating_add(1);
            if g.fail_streak >= MAX_FAIL_STREAK {
                g.parked_until = Some(Instant::now() + Duration::from_secs(LONG_PARK_SECS));
                return true;
            }
        }
        false
    }

    /// Clears the failure streak after the watchdog observes a genuinely
    /// streaming socket (so an occasional drop on a healthy feed never trips it).
    fn clear_feed_streak(&self) {
        if let Ok(mut g) = self.health.lock() {
            g.fail_streak = 0;
        }
    }

    /// Background watchdog: if a session exists but no supervisor is alive
    /// (because the socket died in a way the inner task could not recover from,
    /// or the process restarted while a token was still valid), bring the feed
    /// back automatically. It also restarts a supervisor that is *alive but not
    /// streaming* - Dhan sometimes accepts the handshake and then never delivers
    /// a packet (connection-slot limit, silent rejection), which used to leave
    /// every panel blank until the user pressed Connect again. This is what makes
    /// the app stream from a single Connect without depending on the browser tab.
    pub fn spawn_watchdog(&self) {
        let st = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if st.park_active() {
                    continue;
                }
                let Some(session) = st.session.read().await.clone() else {
                    continue;
                };
                if !st.feed_running() {
                    if st.note_feed_failure() {
                        tracing::warn!(
                            "feed circuit-breaker: supervisor gone repeatedly, parking {}s",
                            LONG_PARK_SECS
                        );
                        continue;
                    }
                    if let Ok(mut g) = st.health.lock() {
                        g.parked_until = None;
                    }
                    tracing::warn!("feed watchdog: supervisor not running, restarting");
                    st.spawn_feed_task(session).await;
                    continue;
                }
                // Alive but silent: give a fresh socket 30s to produce its first
                // packet, and restart one that ticked and then went quiet for
                // 25s. Only during exchange hours, otherwise a closed market
                // (which legitimately delivers nothing) would churn connections.
                if !market_open_now() {
                    continue;
                }
                let (silent, healthy) = st
                    .health
                    .lock()
                    .map(|g| {
                        let fresh = g.feed_up
                            && g.last_tick
                                .map(|t| t.elapsed() < Duration::from_secs(25))
                                .unwrap_or(false);
                        // A socket that has streamed but then received NOTHING at
                        // all for a while is a stall. Crucially this watches any
                        // frame (heartbeat / PrevClose / OI), not just trade ticks:
                        // on a closed holiday the market is quiet yet Dhan keeps
                        // pinging every ~10s, so a live socket never looks stalled.
                        let silent = g.feed_up
                            && g.last_activity
                                .map(|t| t.elapsed() > Duration::from_secs(45))
                                .unwrap_or(false);
                        (silent, fresh)
                    })
                    .unwrap_or((false, false));
                if healthy {
                    st.clear_feed_streak();
                }
                if silent {
                    if st.note_feed_failure() {
                        tracing::warn!(
                            "feed circuit-breaker: repeated silent sockets, parking {}s",
                            LONG_PARK_SECS
                        );
                        st.stop_feed();
                        continue;
                    }
                    tracing::warn!("feed watchdog: socket alive but silent, restarting");
                    st.spawn_feed_task(session).await;
                }
            }
        });
    }

    /// Automatic Dhan REST ("API") reconnect. The market *feed* healed itself on
    /// its own, but the Dhan *session* was only ever (re)validated by the Connect
    /// button, so a dropped or silently-rotated session kept the app "connected"
    /// in the UI while every REST call (chart, option chain, orders) failed until
    /// the user pressed Connect again.
    ///
    /// This watchdog re-runs the exact `profile()` authentication that Connect
    /// uses - every 2s while the link is down during exchange hours - so a
    /// transient disconnect heals with no user action. It never touches a feed
    /// supervisor that is already alive (that one has its own capped backoff, and
    /// forcing it would only re-trip Dhan's connection limit), and it honours the
    /// `/api/feed/reset` park cooldown.
    ///
    /// Note: an *expired daily token* still needs a fresh paste - the app holds no
    /// app-secret/TOTP, so it cannot mint a new token on its own.
    pub fn spawn_api_reconnect(&self) {
        let st = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(2));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_err: Option<String> = None;
            loop {
                tick.tick().await;
                if st.park_active() || !market_open_now() {
                    continue;
                }
                let Some(session) = st.session.read().await.clone() else {
                    continue;
                };
                // Already streaming: nothing to reconnect.
                let live = st
                    .health
                    .lock()
                    .map(|g| {
                        g.feed_up
                            && g.last_tick
                                .map(|t| t.elapsed() < Duration::from_secs(15))
                                .unwrap_or(false)
                    })
                    .unwrap_or(false);
                if live {
                    continue;
                }
                match session.client.profile().await {
                    Ok(_) => {
                        st.set_auth_error(None);
                        last_err = None;
                        if !st.feed_running() {
                            tracing::warn!("api auto-reconnect: session valid, restarting feed");
                            st.spawn_feed_task(session).await;
                        }
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if last_err.as_deref() != Some(msg.as_str()) {
                            tracing::warn!("api auto-reconnect failed: {msg}");
                            last_err = Some(msg.clone());
                        }
                        st.set_auth_error(Some(msg));
                    }
                }
            }
        });
    }
}

/// True during the Indian exchange session (IST 09:15-23:30, Mon-Fri), which
/// covers both the cash/F&O day and the MCX evening session. Used to gate the
/// silent-feed watchdog so a closed market does not cause reconnect churn.
fn market_open_now() -> bool {
    let ist = now_secs() + 19_800; // UTC -> IST
    let days = ist.div_euclid(86_400);
    let weekday = (days + 4).rem_euclid(7); // 1970-01-01 was a Thursday
    if weekday == 0 || weekday == 6 {
        return false;
    }
    let secs = ist.rem_euclid(86_400);
    secs >= 9 * 3600 + 15 * 60 && secs <= 23 * 3600 + 30 * 60
}

/// Derived connection link state for the header/popups and the disconnect
/// monitor. Pure (no state) so it can be unit-tested exhaustively:
/// - `offline`    no Dhan session at all -> needs Client ID + Token
/// - `closed`     session exists but the exchange is closed -> idle, not an error
/// - `live`       socket streaming and a tick landed within the last 15s
/// - `connecting` socket is up but has not delivered its first packet yet
/// - `down`       connected + market open, but the feed is silent/not streaming
fn link_state(
    connected: bool,
    market_open: bool,
    feed_up: bool,
    last_tick_age_sec: Option<f64>,
    ws_running: bool,
    feed_started_age_sec: Option<f64>,
) -> &'static str {
    if !connected {
        return "offline";
    }
    if !market_open {
        return "closed";
    }
    let fresh = feed_up && last_tick_age_sec.map(|a| a < 15.0).unwrap_or(false);
    if fresh {
        return "live";
    }
    let still_handshaking = !feed_up
        && ws_running
        && feed_started_age_sec.map(|a| a < 30.0).unwrap_or(false);
    if still_handshaking {
        return "connecting";
    }
    "down"
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ConnectReq {
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub access_token: String,
}

pub async fn connect(
    State(st): State<DhanState>,
    Json(req): Json<ConnectReq>,
) -> (StatusCode, Json<Value>) {
    let client_id = req.client_id.trim();
    let access_token = req.access_token.trim();
    if client_id.is_empty() || access_token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status":"error","message":"Client ID and Access Token required"})),
        );
    }

    let client = DhanClient::new(client_id, access_token);
    match client.profile().await {
        Ok(profile) => {
            st.set_auth_error(None);
            // A fresh login is a clean slate: clear any accumulated failure streak
            // so the circuit-breaker does not immediately re-park the new session.
            if let Ok(mut g) = st.health.lock() {
                g.fail_streak = 0;
            }
            let id = if profile.dhan_client_id.is_empty() {
                client_id.to_string()
            } else {
                profile.dhan_client_id.clone()
            };
            *st.session.write().await = Some(Session {
                client: client.clone(),
                client_id: id.clone(),
            });
            let creds = Session {
                client,
                client_id: id.clone(),
            };
            st.start_feed(creds).await;
            (
                StatusCode::OK,
                Json(json!({"status":"success","message":"Connected successfully","client_id":id})),
            )
        }
        Err(e) => {
            st.set_auth_error(Some(e.to_string()));
            // 4xx (not 5xx): Cloudflare in front of the preview tunnel rewrites
            // origin 5xx responses with its own opaque "error code: 502" page,
            // which hides the real Dhan message from the UI.
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"status":"error","message":e.to_string(),"auth_error":e.to_string()})),
            )
        }
    }
}

pub async fn status(State(st): State<DhanState>) -> impl IntoResponse {
    let connected = st.session.read().await.is_some();
    let client_id = st.client_id().await.unwrap_or_default();
    let auth_error = st.auth_error.lock().ok().and_then(|g| g.clone());
    Json(json!({
        "connected": connected,
        "client_id": client_id,
        "auth_error": auth_error,
    }))
}

pub async fn feed_status(State(st): State<DhanState>) -> impl IntoResponse {
    let connected = st.session.read().await.is_some();
    let auth_error = st.auth_error.lock().ok().and_then(|g| g.clone());
    let market_open = market_open_now();
    let mut snap = st.health.lock().map(|g| g.snapshot()).unwrap_or(Value::Null);
    // Derive the single link state the UI reacts to (popups + auto reconnect).
    let (feed_up, ws_running, age, started) = {
        let o = snap.as_object();
        let tick_age = o.and_then(|o| o.get("last_tick_age_sec")).and_then(|v| v.as_f64());
        let activity_age = o
            .and_then(|o| o.get("last_activity_age_sec"))
            .and_then(|v| v.as_f64());
        // Liveness for the UI: a socket kept alive by Dhan's heartbeats (every
        // ~10-15s) counts as streaming even when the market is quiet and trade
        // ticks are sparse (holiday / off-hours), so the header does not flicker
        // to a false "down" while the connection is actually healthy.
        let age = match (tick_age, activity_age) {
            (Some(t), Some(a)) => Some(t.min(a)),
            (t, a) => t.or(a),
        };
        (
            o.and_then(|o| o.get("feed_up")).and_then(|v| v.as_bool()).unwrap_or(false),
            o.and_then(|o| o.get("ws_running")).and_then(|v| v.as_bool()).unwrap_or(false),
            age,
            o.and_then(|o| o.get("feed_started_age_sec")).and_then(|v| v.as_f64()),
        )
    };
    let link = link_state(connected, market_open, feed_up, age, ws_running, started);
    if let Some(o) = snap.as_object_mut() {
        o.insert("connected".into(), json!(connected));
        o.insert("market_open".into(), json!(market_open));
        o.insert("link".into(), json!(link));
        o.insert("auth_error".into(), json!(auth_error));
    }
    Json(snap)
}

pub async fn feed_reset(State(st): State<DhanState>) -> impl IntoResponse {
    const COOLDOWN: u64 = 90;
    st.stop_feed();
    if let Ok(mut g) = st.health.lock() {
        g.last_tick = None;
        // Never shorten an already-running (circuit-breaker) park: the browser's
        // automatic reset repeatedly calls this, and shortening it would defeat
        // the long quiet period Dhan needs to clear the rate limit.
        let until = Instant::now() + Duration::from_secs(COOLDOWN);
        if !g.parked_until.map(|u| u > until).unwrap_or(false) {
            g.parked_until = Some(until);
        }
    }
    Json(json!({
        "status": "success",
        "message": "Feed stopped. Connection slots are being released by Dhan.",
        "cooldown": COOLDOWN,
    }))
}

pub async fn feed_restart(State(st): State<DhanState>) -> (StatusCode, Json<Value>) {
    let sess = st.session.read().await.clone();
    match sess {
        Some(s) => {
            // This endpoint is also the browser's automatic nudge. Two cases
            // must NOT tear the supervisor down:
            //  * an active cooldown is deliberate (/api/feed/reset parks the
            //    feed so Dhan releases the connection slot). Clearing it here
            //    reopened the socket mid-cooldown and immediately re-tripped the
            //    block, so the cooldown could never complete.
            //  * a supervisor is already alive - it reconnects on its own with a
            //    growing backoff. Respawning resets that backoff to 2s and hammers
            //    Dhan, which is what kept the client id blocked.
            if st.park_active() {
                return (
                    StatusCode::OK,
                    Json(json!({"status":"success","message":"Feed cooling down; leaving it parked"})),
                );
            }
            if st.feed_running() {
                return (
                    StatusCode::OK,
                    Json(json!({"status":"success","message":"Feed already running"})),
                );
            }
            st.start_feed(s).await;
            (
                StatusCode::OK,
                Json(json!({"status":"success","message":"Feed threads restarted"})),
            )
        }
        None => (
            StatusCode::CONFLICT,
            Json(json!({"status":"error","message":"Not connected"})),
        ),
    }
}

// ---------------------------------------------------------------------------
// Feed runtime
// ---------------------------------------------------------------------------

fn segment_exchange(seg: u8) -> Option<&'static str> {
    Some(match seg {
        0 => "IDX_I",
        1 => "NSE_EQ",
        2 => "NSE_FNO",
        3 => "NSE_CURRENCY",
        4 => "BSE_EQ",
        5 => "MCX_COMM",
        7 => "BSE_CURRENCY",
        8 => "BSE_FNO",
        _ => return None,
    })
}

fn feed_subscriptions() -> Vec<FeedSubscription> {
    securities()
        .into_iter()
        .filter_map(|(sid, exch)| {
            // Watchlist symbols ride on Ticker mode: Dhan does not stream index
            // (and most commodity) packets for Full-mode subscriptions, which is
            // why indices had no live ticks and their candles never formed.
            exchange_segment(&exch)
                .map(|s| FeedSubscription::with_mode(s, sid, FeedMode::Ticker))
        })
        .collect()
}

fn feed_quote(ltp: f64, close: f64, at: i64, volume: f64, oi: f64, bid: f64, ask: f64) -> Value {
    let change = if close != 0.0 { ltp - close } else { 0.0 };
    let pct = if close != 0.0 { change / close * 100.0 } else { 0.0 };
    let mut v = json!({
        "ltp": round2(ltp),
        "change": round2(change),
        "close": round2(close),
        "change_pct": round2(pct),
        "at": at,
        "live": 1,
    });
    // Only publish extra fields when present so a Ticker-mode symbol is not given
    // a fake volume/OI/depth (the old app merged them in per packet type).
    if volume > 0.0 {
        v["volume"] = json!(round2(volume));
    }
    if oi > 0.0 {
        v["oi"] = json!(round2(oi));
    }
    if bid > 0.0 {
        v["bid"] = json!(round2(bid));
    }
    if ask > 0.0 {
        v["ask"] = json!(round2(ask));
    }
    v
}

/// Build a live quote for one tick, merging the accumulated Volume / OI / Bid /
/// Ask extras and inferring IV + delta + vega for option strikes (Dhan's feed
/// carries no IV, so the old app inverted Black-Scholes from the live premium on
/// every tick, throttled to ~4/sec per strike).
fn live_quote(
    key: &str,
    sid: i64,
    ltp: f64,
    now: i64,
    closes: &HashMap<String, f64>,
    market: &MarketState,
    extras: &HashMap<String, (f64, f64, f64, f64)>,
    iv_at: &mut HashMap<String, Instant>,
    oi_base: &mut HashMap<String, f64>,
) -> Value {
    // Dhan's feed never sends a PrevClose packet, so the authoritative previous
    // close comes from the REST / daily-candle seed map. When a symbol is missing
    // there, fall back to the last cached close so a live tick can never zero out
    // an already-good change / change_pct.
    let mut close = closes.get(key).copied().unwrap_or(0.0);
    if close <= 0.0 {
        close = market
            .quotes
            .lock()
            .ok()
            .and_then(|g| {
                g.get(key)
                    .and_then(|e| e.get("close"))
                    .and_then(|c| c.as_f64())
            })
            .unwrap_or(0.0);
    }
    let (vol, oi, bid, ask) = extras.get(key).copied().unwrap_or((0.0, 0.0, 0.0, 0.0));
    let mut q = feed_quote(ltp, close, now, vol, oi, bid, ask);
    // Dhan's feed carries no previous-OI packet, so seed the OI baseline from the
    // first OI seen this session and derive the live change against it. This keeps
    // "Chg OI" ticking on the websocket instead of polling the REST chain.
    if oi > 0.0 {
        let base = *oi_base.entry(key.to_string()).or_insert(oi);
        if base > 0.0 {
            q["chg_oi"] = json!(round2(oi - base));
        }
    }
    if !key.starts_with("IDX_I:") && ltp > 0.0 {
        let due = iv_at
            .get(key)
            .map(|t| t.elapsed() >= Duration::from_millis(250))
            .unwrap_or(true);
        if due {
            iv_at.insert(key.to_string(), Instant::now());
            if let Some(g) = crate::optionchain::opt_greeks(sid, ltp) {
                for field in ["iv", "delta", "vega", "theta", "gamma"] {
                    if let Some(v) = g.get(field).and_then(|v| v.as_f64()) {
                        q[field] = json!(v);
                    }
                }
            }
        }
    }
    q
}

/// [`live_quote`] with the shared previous-close map locked for the duration of
/// the call (the REST seeder writes the same map from another task).
#[allow(clippy::too_many_arguments)]
fn live_quote_locked(
    closes: &Arc<Mutex<HashMap<String, f64>>>,
    key: &str,
    sid: i64,
    ltp: f64,
    now: i64,
    market: &MarketState,
    extras: &HashMap<String, (f64, f64, f64, f64)>,
    iv_at: &mut HashMap<String, Instant>,
    oi_base: &mut HashMap<String, f64>,
) -> Value {
    let guard = closes.lock().unwrap_or_else(|e| e.into_inner());
    live_quote(key, sid, ltp, now, &guard, market, extras, iv_at, oi_base)
}

async fn run_feed(
    client_id: String,
    token: String,
    subs: Vec<FeedSubscription>,
    market: MarketState,
    health: Arc<Mutex<FeedHealth>>,
    closes: Arc<Mutex<HashMap<String, f64>>>,
    cmd_rx: mpsc::UnboundedReceiver<FeedCommand>,
) {
    let count = subs.len();
    let feed = MarketFeed::new(client_id, token, subs);
    let (tx, mut rx) = mpsc::channel::<FeedPacket>(4096);
    let _runner = AbortOnDrop(tokio::spawn(feed.run(tx, cmd_rx)));

    if let Ok(mut g) = health.lock() {
        g.ws_running = true;
        // `feed_up` means "the broker socket is actually streaming", so it stays
        // false until the first packet arrives - a spawned task that is stuck
        // reconnecting must not look healthy.
        g.feed_up = false;
        g.socket_open = false;
        g.connected_at = None;
        g.feed_started = Some(Instant::now());
        g.subscribed = count;
    }

    // Accumulated (volume, oi, bid, ask) per quote key so a Ticker/Quote packet
    // keeps the Full-mode extras already received for the same strike.
    let mut extras: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
    let mut iv_at: HashMap<String, Instant> = HashMap::new();
    let mut oi_base: HashMap<String, f64> = HashMap::new();
    let mut pending: HashMap<String, Value> = HashMap::new();
    let mut flush = tokio::time::interval(Duration::from_millis(400));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            maybe = rx.recv() => {
                let Some(pkt) = maybe else { break };
                let now = now_secs();
                // Link is a control signal, not market data: it must not mark
                // the feed as "up"/fresh (that made a socket stuck in reconnect
                // backoff look live and masked the outage).
                if let FeedPacket::Link { up } = pkt {
                    let mut park_long = false;
                    if let Ok(mut g) = health.lock() {
                        g.socket_open = up;
                        if up {
                            g.connected_at = Some(Instant::now());
                        } else {
                            g.feed_up = false;
                            g.fail_streak = g.fail_streak.saturating_add(1);
                            if g.fail_streak >= MAX_FAIL_STREAK {
                                g.parked_until =
                                    Some(Instant::now() + Duration::from_secs(LONG_PARK_SECS));
                                park_long = true;
                            }
                        }
                    }
                    if park_long {
                        tracing::warn!(
                            "feed circuit-breaker: {} consecutive failures, parking {}s so Dhan can clear the rate limit",
                            MAX_FAIL_STREAK,
                            LONG_PARK_SECS
                        );
                        // End this supervisor; the watchdog re-spawns only after
                        // the park expires, giving Dhan a real quiet window.
                        break;
                    }
                    continue;
                }
                // Only a real trade packet counts as "the feed is streaming". A
                // PrevClose / OI / MarketStatus packet is sent even on a closed
                // market, so treating those as ticks made a holiday socket look
                // live and then "stall" 25s later, churning the connection.
                let is_live_tick = matches!(
                    &pkt,
                    FeedPacket::Ticker { .. }
                        | FeedPacket::Quote { .. }
                        | FeedPacket::Full { .. }
                );
                if is_live_tick {
                    if let Ok(mut g) = health.lock() {
                        g.last_tick = Some(Instant::now());
                        g.feed_up = true;
                    }
                }
                // Any frame (trade tick, PrevClose/OI snapshot, or a later
                // heartbeat) proves the socket is alive.
                if let Ok(mut g) = health.lock() {
                    g.last_activity = Some(Instant::now());
                }
                match pkt {
                    FeedPacket::PrevClose { segment, security_id, prev_close, .. } => {
                        if let Some(exch) = segment_exchange(segment) {
                            if let Ok(mut g) = closes.lock() {
                                g.insert(quote_key(security_id as i64, exch), prev_close as f64);
                            }
                        }
                    }
                    FeedPacket::Oi { segment, security_id, oi } => {
                        if let Some(exch) = segment_exchange(segment) {
                            let key = quote_key(security_id as i64, exch);
                            extras.entry(key).or_insert((0.0, 0.0, 0.0, 0.0)).1 = oi as f64;
                        }
                    }
                    FeedPacket::Ticker { segment, security_id, ltp, .. } => {
                        if let Some(exch) = segment_exchange(segment) {
                            let key = quote_key(security_id as i64, exch);
                            let q = live_quote_locked(&closes, &key, security_id as i64, ltp as f64, now, &market, &extras, &mut iv_at, &mut oi_base);
                            if key.starts_with("IDX_I:") {
                                crate::optionchain::set_index_spot(security_id as i64, ltp as f64);
                            }
                            market.patch_tick(security_id as i64, ltp as f64, now + 19800);
                            market.set_quote(key.clone(), q.clone());
                            pending.insert(key, q);
                        }
                    }
                    FeedPacket::Quote { segment, security_id, ltp, volume, .. } => {
                        if let Some(exch) = segment_exchange(segment) {
                            let key = quote_key(security_id as i64, exch);
                            extras.entry(key.clone()).or_insert((0.0, 0.0, 0.0, 0.0)).0 = volume as f64;
                            let q = live_quote_locked(&closes, &key, security_id as i64, ltp as f64, now, &market, &extras, &mut iv_at, &mut oi_base);
                            if key.starts_with("IDX_I:") {
                                crate::optionchain::set_index_spot(security_id as i64, ltp as f64);
                            }
                            market.patch_tick(security_id as i64, ltp as f64, now + 19800);
                            market.set_quote(key.clone(), q.clone());
                            pending.insert(key, q);
                        }
                    }
                    FeedPacket::Full { segment, security_id, ltp, volume, oi, depth, .. } => {
                        if let Some(exch) = segment_exchange(segment) {
                            let key = quote_key(security_id as i64, exch);
                            let (bid, ask) = depth
                                .first()
                                .map(|d| (d.bid_price as f64, d.ask_price as f64))
                                .unwrap_or((0.0, 0.0));
                            let e = extras.entry(key.clone()).or_insert((0.0, 0.0, 0.0, 0.0));
                            e.0 = volume as f64;
                            e.1 = oi as f64;
                            if bid > 0.0 {
                                e.2 = bid;
                            }
                            if ask > 0.0 {
                                e.3 = ask;
                            }
                            let q = live_quote_locked(&closes, &key, security_id as i64, ltp as f64, now, &market, &extras, &mut iv_at, &mut oi_base);
                            if key.starts_with("IDX_I:") {
                                crate::optionchain::set_index_spot(security_id as i64, ltp as f64);
                            }
                            market.patch_tick(security_id as i64, ltp as f64, now + 19800);
                            market.set_quote(key.clone(), q.clone());
                            pending.insert(key, q);
                        }
                    }
                    FeedPacket::MarketStatus { .. } => {}
                    FeedPacket::Heartbeat => {
                        // Liveness only; `last_activity` was already refreshed.
                    }
                    FeedPacket::Disconnect { .. } => {
                        // Dhan closes long-lived sockets periodically and the
                        // inner `MarketFeed` supervisor reconnects on its own
                        // with backoff. A disconnect must therefore NOT tear
                        // this consumer down: just mark the socket as not
                        // streaming and keep consuming. Treating it as terminal
                        // is what used to silently freeze the chart, ticker,
                        // option chain and running-strategy P&L until a manual
                        // reconnect.
                        if let Ok(mut g) = health.lock() {
                            g.feed_up = false;
                        }
                    }
                    FeedPacket::Link { .. } => {
                        // Handled above (it is not market data).
                    }
                    FeedPacket::Unknown { .. } => {}
                }
            }
            _ = flush.tick() => {
                market.broadcast_quotes(&pending);
                pending.clear();
            }
        }
    }

    // `_runner` (the inner websocket task) is aborted when it drops here.
    if let Ok(mut g) = health.lock() {
        g.ws_running = false;
        g.feed_up = false;
        g.feed_started = None;
        g.socket_open = false;
        g.connected_at = None;
    }
}

// ---------------------------------------------------------------------------
// Timeframe mapping (mirrors the old app's TIMEFRAME_CONFIG)
// ---------------------------------------------------------------------------

enum TfPlan {
    Intraday { interval: u32, resample: Option<i64> },
    Daily,
}

fn timeframe_plan(tf: &str) -> TfPlan {
    match tf {
        "1min" => TfPlan::Intraday { interval: 1, resample: None },
        "2min" => TfPlan::Intraday { interval: 1, resample: Some(120) },
        "3min" => TfPlan::Intraday { interval: 1, resample: Some(180) },
        "4min" => TfPlan::Intraday { interval: 1, resample: Some(240) },
        "5min" => TfPlan::Intraday { interval: 5, resample: None },
        "10min" => TfPlan::Intraday { interval: 5, resample: Some(600) },
        "15min" => TfPlan::Intraday { interval: 15, resample: None },
        "30min" => TfPlan::Intraday { interval: 5, resample: Some(1800) },
        "1hour" => TfPlan::Intraday { interval: 60, resample: None },
        "4hour" => TfPlan::Intraday { interval: 60, resample: Some(14400) },
        _ => TfPlan::Daily,
    }
}

fn tf_step_secs(tf: &str) -> i64 {
    match tf {
        "week" => 604800,
        "month" => 2592000,
        "year" => 31536000,
        "1min" | "2min" | "3min" | "4min" | "5min" | "10min" | "15min" | "30min" | "1hour" => 300,
        "4hour" => 14400,
        _ => 86400,
    }
}

fn to_candles(rows: Vec<CandleRow>) -> Vec<Candle> {
    rows.into_iter()
        .map(|r| Candle {
            time: r.time,
            open: r.open,
            high: r.high,
            low: r.low,
            close: r.close,
            volume: r.volume,
        })
        .collect()
}

/// Bucket candles into `step`-second bars (open=first, close=last, high=max,
/// low=min, volume=sum). Rows must already be sorted by time.
fn resample_candles(rows: &[CandleRow], step: i64) -> Vec<Candle> {
    if step <= 0 || rows.is_empty() {
        return to_candles(rows.to_vec());
    }
    let mut out: Vec<Candle> = Vec::new();
    let mut bucket = i64::MIN;
    let mut cur: Option<Candle> = None;
    for r in rows {
        let b = r.time - r.time.rem_euclid(step);
        if b != bucket {
            if let Some(c) = cur.take() {
                out.push(c);
            }
            bucket = b;
            cur = Some(Candle {
                time: b,
                open: r.open,
                high: r.high,
                low: r.low,
                close: r.close,
                volume: r.volume,
            });
        } else if let Some(c) = cur.as_mut() {
            c.high = c.high.max(r.high);
            c.low = c.low.min(r.low);
            c.close = r.close;
            c.volume += r.volume;
        }
    }
    if let Some(c) = cur {
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Enum + date helpers
// ---------------------------------------------------------------------------

fn exchange_segment(s: &str) -> Option<ExchangeSegment> {
    Some(match s.to_uppercase().as_str() {
        "IDX_I" => ExchangeSegment::IdxI,
        "NSE_EQ" => ExchangeSegment::NseEq,
        "NSE_FNO" => ExchangeSegment::NseFno,
        "NSE_CURRENCY" => ExchangeSegment::NseCurrency,
        "BSE_EQ" => ExchangeSegment::BseEq,
        "MCX_COMM" => ExchangeSegment::McxComm,
        "BSE_CURRENCY" => ExchangeSegment::BseCurrency,
        "BSE_FNO" => ExchangeSegment::BseFno,
        _ => return None,
    })
}

/// The alternate option instrument type Dhan may accept when the scrip-master /
/// resolved one returns no intraday bars: index options (OPTIDX) sometimes only
/// answer as OPTSTK and vice versa. Returns `None` for non-option instruments.
fn alt_option_instrument(inst: Instrument) -> Option<Instrument> {
    match inst {
        Instrument::OptIdx => Some(Instrument::OptStk),
        Instrument::OptStk => Some(Instrument::OptIdx),
        _ => None,
    }
}

fn instrument_type(s: &str) -> Instrument {
    match s.to_uppercase().as_str() {
        "INDEX" => Instrument::Index,
        "FUTIDX" => Instrument::FutIdx,
        "OPTIDX" => Instrument::OptIdx,
        "FUTSTK" => Instrument::FutStk,
        "OPTSTK" => Instrument::OptStk,
        "FUTCOM" => Instrument::FutCom,
        "OPTFUT" => Instrument::OptFut,
        "FUTCUR" => Instrument::FutCur,
        "OPTCUR" => Instrument::OptCur,
        _ => Instrument::Equity,
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Days since Unix epoch to (year, month, day) - Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn fmt_date(epoch_ist: i64) -> String {
    let days = epoch_ist.div_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn fmt_datetime(epoch_ist: i64) -> String {
    let days = epoch_ist.div_euclid(86400);
    let secs = epoch_ist.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
mod chart_instrument_tests {
    use super::*;

    #[test]
    fn alt_option_instrument_flips_index_and_stock_options() {
        assert_eq!(alt_option_instrument(Instrument::OptIdx), Some(Instrument::OptStk));
        assert_eq!(alt_option_instrument(Instrument::OptStk), Some(Instrument::OptIdx));
    }

    #[test]
    fn alt_option_instrument_ignores_non_options() {
        for inst in [
            Instrument::Index,
            Instrument::Equity,
            Instrument::FutIdx,
            Instrument::FutStk,
            Instrument::FutCom,
        ] {
            assert_eq!(alt_option_instrument(inst), None);
        }
    }

    #[test]
    fn instrument_type_maps_option_strings() {
        assert_eq!(instrument_type("OPTIDX"), Instrument::OptIdx);
        assert_eq!(instrument_type("optstk"), Instrument::OptStk);
        assert_eq!(instrument_type("OPTFUT"), Instrument::OptFut);
    }
}

#[cfg(test)]
mod live_quote_tests {
    use super::*;

    #[test]
    fn live_quote_derives_chg_oi_from_first_seen_baseline() {
        let market = MarketState::new();
        let mut closes: HashMap<String, f64> = HashMap::new();
        closes.insert("IDX_I:13".to_string(), 100.0);
        let mut extras: HashMap<String, (f64, f64, f64, f64)> = HashMap::new();
        extras.insert("IDX_I:13".to_string(), (0.0, 1200.0, 0.0, 0.0));
        let mut iv_at: HashMap<String, Instant> = HashMap::new();
        let mut oi_base: HashMap<String, f64> = HashMap::new();

        // First OI seen seeds the baseline, so the change starts at zero.
        let q = live_quote(
            "IDX_I:13", 13, 101.0, 0, &closes, &market, &extras, &mut iv_at, &mut oi_base,
        );
        assert_eq!(q.get("chg_oi").and_then(|v| v.as_f64()), Some(0.0));

        // A later tick measures against the seeded baseline, not the last tick.
        extras.insert("IDX_I:13".to_string(), (0.0, 1500.0, 0.0, 0.0));
        let q2 = live_quote(
            "IDX_I:13", 13, 101.0, 0, &closes, &market, &extras, &mut iv_at, &mut oi_base,
        );
        assert_eq!(q2.get("chg_oi").and_then(|v| v.as_f64()), Some(300.0));
    }
}

#[cfg(test)]
mod link_state_tests {
    use super::*;

    #[test]
    fn offline_when_no_session_regardless_of_market() {
        assert_eq!(link_state(false, true, true, Some(0.0), true, Some(1.0)), "offline");
        assert_eq!(link_state(false, false, false, None, false, None), "offline");
    }

    #[test]
    fn closed_when_session_but_market_shut() {
        assert_eq!(link_state(true, false, true, Some(0.0), true, Some(5.0)), "closed");
        assert_eq!(link_state(true, false, false, None, false, None), "closed");
    }

    #[test]
    fn live_when_fresh_tick() {
        assert_eq!(link_state(true, true, true, Some(0.0), true, Some(60.0)), "live");
        assert_eq!(link_state(true, true, true, Some(14.9), true, Some(60.0)), "live");
    }

    #[test]
    fn stale_tick_is_down_not_live() {
        // feed_up but the last tick is too old: a stalled socket must not look live.
        assert_eq!(link_state(true, true, true, Some(15.0), true, Some(600.0)), "down");
        assert_eq!(link_state(true, true, true, None, true, Some(600.0)), "down");
    }

    #[test]
    fn connecting_while_socket_opens() {
        assert_eq!(link_state(true, true, false, None, true, Some(0.0)), "connecting");
        assert_eq!(link_state(true, true, false, None, true, Some(29.0)), "connecting");
        // handshake window elapsed with still no data -> real outage.
        assert_eq!(link_state(true, true, false, None, true, Some(31.0)), "down");
        // socket not running at all -> outage.
        assert_eq!(link_state(true, true, false, None, false, Some(5.0)), "down");
    }
}


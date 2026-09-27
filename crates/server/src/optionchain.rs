//! Option-chain backend: expiries, the CE/PE chain, ATM strike selection, lot
//! sizes and option-contract lookup.
//!
//! Mirrors the old Flask app's option-chain endpoints (same request/response
//! contract). The chain comes from the scrip master and Dhan's `/optionchain`
//! API; without a live session the endpoints return no data rather than any
//! synthetic chain.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use dhan_hq::OptionChainData;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use algo_core::oi_trend::OiRecord;
use algo_core::option as bs;

use crate::broker::DhanState;
use crate::market;
use crate::scrip;

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct OcReq {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub exchange_segment: String,
    #[serde(default)]
    pub symbol_name: String,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub spot: f64,
}

#[derive(Deserialize)]
pub struct AutoStrikesReq {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub exchange_segment: String,
    #[serde(default)]
    pub symbol_name: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default = "three")]
    pub count: i64,
    #[serde(default)]
    pub option_type: String,
    #[serde(default)]
    pub spot: f64,
}

fn three() -> i64 {
    3
}

#[derive(Deserialize)]
pub struct OptionSecurityReq {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub symbol_name: String,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub strike: f64,
    #[serde(default)]
    pub option_type: String,
    #[serde(default = "nse_fno")]
    pub exchange_segment: String,
}

fn nse_fno() -> String {
    "NSE_FNO".to_string()
}

#[derive(Deserialize)]
pub struct OcSubReq {
    #[serde(default)]
    pub securities: Vec<OcSubSec>,
}

#[derive(Deserialize)]
pub struct OcSubSec {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub exchange_segment: String,
}

/// Testing-only "Manual Strike Select" picker request: the operator-chosen
/// underlying (and optionally a specific expiry). Returns the expiry ladder plus
/// a wide strike window with CE/PE LTP so a strike can be picked by hand.
#[derive(Deserialize)]
pub struct ManualStrikeReq {
    #[serde(default)]
    pub security_id: i64,
    #[serde(default)]
    pub exchange_segment: String,
    #[serde(default)]
    pub symbol_name: String,
    #[serde(default)]
    pub expiry: String,
    #[serde(default)]
    pub spot: f64,
    /// Strikes to show on each side of ATM (default 30, clamped 1..=60).
    #[serde(default)]
    pub window: i64,
}

// ---------------------------------------------------------------------------
// In-memory chain cache + Dhan rate-limit cooldown
// ---------------------------------------------------------------------------

/// Short-lived RAM cache for option chains plus a global cooldown after Dhan
/// rate-limits us. Mirrors the old app's `_oc_cache` / `oc_rate_limited`
/// behaviour: a fresh cache hit is served instantly; a stale entry is served
/// with `partial: true` while the client keeps polling for the real upgrade.
#[derive(Default)]
struct OcCache {
    chains: HashMap<String, (Instant, Value)>,
    cooldown_until: Option<Instant>,
    /// `(at, list, authoritative)` per underlying: the scrip-master ladder is
    /// served instantly as non-authoritative (`stale: true`) while a background
    /// Dhan call replaces it with the broker's own list (old app's
    /// `cacheSet("expiries", ...)` after `_fetch_option_expiries`).
    expiries: HashMap<String, (Instant, Vec<String>, bool)>,
    expiry_inflight: std::collections::HashSet<String>,
}

fn oc_cache() -> &'static Mutex<OcCache> {
    static C: OnceLock<Mutex<OcCache>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(OcCache::default()))
}

const OC_TTL: Duration = Duration::from_secs(3);
const OC_COOLDOWN: Duration = Duration::from_secs(30);
/// Authoritative expiry lists change rarely; keep them for 5 minutes.
const EXP_TTL: Duration = Duration::from_secs(300);

fn cache_key(sid: i64, exch: &str, expiry: &str) -> String {
    format!("{}|{}|{}", sid, exch.to_uppercase(), expiry)
}

fn expiry_key(sid: i64, exch: &str) -> String {
    format!("{}|{}", sid, exch.to_uppercase())
}

/// `(fresh, list, authoritative)`. An expired non-authoritative entry still
/// returns its list so the dropdown never blanks while the refresh runs.
fn cache_get_expiries(key: &str) -> Option<(bool, Vec<String>, bool)> {
    let g = oc_cache().lock().ok()?;
    let (at, list, auth) = g.expiries.get(key)?;
    Some((at.elapsed() < EXP_TTL, list.clone(), *auth))
}

fn cache_put_expiries(key: &str, list: Vec<String>, authoritative: bool) {
    if let Ok(mut g) = oc_cache().lock() {
        g.expiries
            .insert(key.to_string(), (Instant::now(), list, authoritative));
        if g.expiries.len() > 64 {
            g.expiries
                .retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(1800));
        }
    }
}

fn mark_expiry_inflight(key: &str) -> bool {
    oc_cache()
        .lock()
        .map(|mut g| g.expiry_inflight.insert(key.to_string()))
        .unwrap_or(false)
}

fn clear_expiry_inflight(key: &str) {
    if let Ok(mut g) = oc_cache().lock() {
        g.expiry_inflight.remove(key);
    }
}

/// `(fresh, value)` for a cached chain.
fn cache_get(key: &str) -> Option<(bool, Value)> {
    let g = oc_cache().lock().ok()?;
    let (at, v) = g.chains.get(key)?;
    Some((at.elapsed() < OC_TTL, v.clone()))
}

fn cache_put(key: &str, v: Value) {
    if let Ok(mut g) = oc_cache().lock() {
        g.chains.insert(key.to_string(), (Instant::now(), v));
        // Bound growth: the tab only ever looks at a handful of expiries.
        if g.chains.len() > 64 {
            g.chains.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(600));
        }
    }
}

fn cooldown_active() -> bool {
    oc_cache()
        .lock()
        .map(|g| g.cooldown_until.map(|t| t > Instant::now()).unwrap_or(false))
        .unwrap_or(false)
}

fn set_cooldown() {
    if let Ok(mut g) = oc_cache().lock() {
        g.cooldown_until = Some(Instant::now() + OC_COOLDOWN);
    }
}

fn is_rate_limit(e: &dhan_hq::DhanError) -> bool {
    let s = e.to_string().to_uppercase();
    s.contains("DH-904") || s.contains("429") || s.contains("TOO MANY") || s.contains("RATE")
}

// ---------------------------------------------------------------------------
// Live option greeks (IV inferred from the traded premium)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct OptMeta {
    under: i64,
    strike: f64,
    expiry: String,
    is_call: bool,
    lot: f64,
}

fn opt_meta() -> &'static Mutex<HashMap<i64, OptMeta>> {
    static M: OnceLock<Mutex<HashMap<i64, OptMeta>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

fn index_spot() -> &'static Mutex<HashMap<i64, f64>> {
    static S: OnceLock<Mutex<HashMap<i64, f64>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Remember the latest traded spot for an index underlying. Called from the live
/// feed on every index tick and from chain registration for synthetic data.
pub fn set_index_spot(sid: i64, price: f64) {
    if price > 0.0 {
        if let Ok(mut g) = index_spot().lock() {
            g.insert(sid, price);
        }
    }
}

fn remember_meta(
    under: i64,
    strike: f64,
    expiry: &str,
    leg: &bs::ChainLeg,
    is_call: bool,
    lot: f64,
) {
    if leg.sid == 0 {
        return;
    }
    if let Ok(mut g) = opt_meta().lock() {
        g.insert(
            leg.sid,
            OptMeta {
                under,
                strike,
                expiry: expiry.to_string(),
                is_call,
                lot,
            },
        );
        if g.len() > 8192 {
            g.clear();
        }
    }
}

/// Infer IV + delta + vega from a live option premium using Black-Scholes. Dhan's
/// feed carries no IV, so the old app inverted it on every tick (throttled); this
/// keeps that behaviour for subscribed strikes of a loaded chain.
pub fn opt_greeks(sid: i64, ltp: f64) -> Option<Value> {
    let meta = opt_meta().lock().ok()?.get(&sid).cloned()?;
    if ltp <= 0.0 {
        return None;
    }
    let spot = *index_spot()
        .lock()
        .ok()?
        .get(&meta.under)
        .unwrap_or(&0.0);
    if spot <= 0.0 {
        return None;
    }
    let t = bs::ttm_years(&meta.expiry, market::now_secs());
    let iv = bs::implied_vol(spot, meta.strike, t, bs::BS_RATE, ltp, meta.is_call)?;
    let delta = bs::bs_delta(spot, meta.strike, t, bs::BS_RATE, iv, meta.is_call);
    let vega = bs::bs_vega(spot, meta.strike, t, bs::BS_RATE, iv);
    let gamma = bs::bs_gamma(spot, meta.strike, t, bs::BS_RATE, iv);
    // Analytic theta is per-year; the chain shows the old app's per-day figure.
    let theta = bs::bs_theta(spot, meta.strike, t, bs::BS_RATE, iv, meta.is_call) / 365.0;
    let _ = meta.lot;
    Some(json!({
        "iv": (iv * 10000.0).round() / 100.0,
        "delta": delta,
        "vega": vega,
        "gamma": gamma,
        "theta": theta,
    }))
}

/// Option F&O exchange segment for an option-chain underlying segment.
fn option_seg(exch: &str) -> &'static str {
    let u = exch.to_uppercase();
    if u.contains("BSE") {
        "BSE_FNO"
    } else if u.contains("MCX") {
        "MCX_COMM"
    } else {
        "NSE_FNO"
    }
}

/// Register every strike in a Dhan chain with the live feed. The DhanState
/// debounces the resubscribe.
async fn register_chain(
    st: &DhanState,
    rows: &[bs::ChainRow],
    exch: &str,
    under: i64,
    expiry: &str,
    spot: f64,
    lot: f64,
) {
    let seg = option_seg(exch);
    let mut secs: Vec<(i64, String)> = Vec::with_capacity(rows.len() * 2);
    for r in rows {
        if r.ce.sid != 0 {
            remember_meta(under, r.strike, expiry, &r.ce, true, lot);
            secs.push((r.ce.sid, seg.to_string()));
        }
        if r.pe.sid != 0 {
            remember_meta(under, r.strike, expiry, &r.pe, false, lot);
            secs.push((r.pe.sid, seg.to_string()));
        }
    }
    if spot > 0.0 {
        set_index_spot(under, spot);
    }
    st.subscribe_options(&secs).await;
    // Immediately publish authoritative LTP / change / prev-close / volume / OI /
    // bid-ask / greeks for the whole chain in one batched REST call, in the
    // background so the chain response itself stays instant. Skipped during a
    // Dhan rate-limit cooldown so it cannot deepen the limit.
    if !cooldown_active() {
        let seed_st = st.clone();
        let under_exch = exch.to_string();
        tokio::spawn(async move {
            seed_st.seed_chain_quotes(secs, under, &under_exch).await;
        });
    }
}

/// True when `sid` is a strike of a chain that has been registered, so the live
/// quote seed (not the generic daily-candle backfill, which would fetch it with a
/// futures instrument type) owns its previous close.
pub fn known_option_sid(sid: i64) -> bool {
    opt_meta()
        .lock()
        .map(|g| g.contains_key(&sid))
        .unwrap_or(false)
}

/// Best-known underlying spot for the chain window and greeks: the request value
/// when the client sent one, else the live spot map (index/underlying ticks), else
/// the shared quote cache for the underlying's own segment. The client does not
/// send a spot, so without this the window centres on the middle strike and the
/// Black-Scholes greeks are computed against a wrong spot.
fn known_spot(st: &DhanState, under: i64, exch: &str, given: f64) -> f64 {
    if given > 0.0 {
        return given;
    }
    let cached = index_spot()
        .lock()
        .ok()
        .and_then(|g| g.get(&under).copied())
        .unwrap_or(0.0);
    if cached > 0.0 {
        return cached;
    }
    let key = market::quote_key(under, exch);
    let ltp = st
        .market
        .quotes
        .lock()
        .ok()
        .and_then(|g| {
            g.get(&key)
                .and_then(|v| v.get("ltp"))
                .and_then(|v| v.as_f64())
        })
        .unwrap_or(0.0);
    if ltp > 0.0 {
        set_index_spot(under, ltp);
    }
    ltp
}

// ---------------------------------------------------------------------------
// Underlying helpers
// ---------------------------------------------------------------------------

fn underlying_name(sid: i64, given: &str) -> String {
    if !given.trim().is_empty() {
        return given.trim().to_string();
    }
    if let Some(arr) = market::catalog_json().get("symbols").and_then(|v| v.as_array()) {
        for row in arr {
            if let Some(a) = row.as_array() {
                if a.get(1).and_then(|v| v.as_i64()) == Some(sid) {
                    if let Some(n) = a.first().and_then(|v| v.as_str()) {
                        return n.to_string();
                    }
                }
            }
        }
    }
    format!("SECURITY {sid}")
}

/// Index lot sizes (old app's `_build_lot_size_map` names). Unknown symbols fall
/// back to 1 until a broker session/scrip master is available.
fn lot_size_for(name: &str, sid: i64) -> f64 {
    let upper = name.to_uppercase();
    let key = upper.split_whitespace().next().unwrap_or("");
    match key {
        "NIFTY" if upper.contains("BANK") => 35.0,
        "NIFTY" => 75.0,
        "BANKNIFTY" => 35.0,
        "FINNIFTY" => 65.0,
        "MIDCPNIFTY" => 120.0,
        "SENSEX" => 20.0,
        "GIFT" => 15.0,
        "INDIA" => 15.0,
        _ => match sid {
            13 => 75.0,
            25 => 35.0,
            27 => 65.0,
            51 => 20.0,
            442 => 120.0,
            5024 => 15.0,
            _ => 1.0,
        },
    }
}

/// Resolve the authoritative scrip-master lot size + contract trading symbol
/// (old `_resolve_lot_size` / `_oc_instrument_meta`), falling back to the
/// hard-coded index lots + the plain symbol name when the master is not loaded.
fn resolve_lot(name: &str, sid: i64, segment: &str) -> (f64, String) {
    if let Some((lot, sym)) = scrip::get().and_then(|sc| sc.lot_for(name, segment)) {
        return (lot, sym);
    }
    (lot_size_for(name, sid), name.replace(' ', "").to_uppercase())
}

// ---------------------------------------------------------------------------
// Scrip-master instant chain (old app's `_build_oc_instant`)
// ---------------------------------------------------------------------------

/// A chain leg built from the live quote cache. The security id always comes
/// from the scrip master; LTP/OI/IV/volume/bid/ask come from the quote feed and
/// stay 0 until the feed warms up (exactly the old app's partial behaviour).
fn quote_leg(mkt: &market::MarketState, sid: i64, seg: &str) -> bs::ChainLeg {
    let mut leg = bs::ChainLeg {
        sid,
        ..Default::default()
    };
    if sid == 0 {
        return leg;
    }
    let key = market::quote_key(sid, seg);
    let q = mkt.quotes.lock().ok().and_then(|g| g.get(&key).cloned());
    if let Some(q) = q {
        let f = |k: &str| q.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
        leg.ltp = f("ltp");
        leg.chg = f("change");
        leg.chg_pct = f("change_pct");
        leg.oi = f("oi");
        leg.chg_oi = f("chg_oi");
        leg.vol = f("volume");
        leg.iv = f("iv");
        leg.bid = f("bid");
        leg.ask = f("ask");
    }
    leg
}

/// Build the ATM +/- 10 strike window for an expiry from the scrip master (old
/// `_oc_view_and_ids`, window = 10). Returns `None` when the master has no
/// strikes for the symbol/expiry so the caller can fall back.
fn scrip_partial_chain(
    sc: &scrip::Scrip,
    prefix: &str,
    exch: &str,
    expiry: &str,
    spot_in: f64,
    seg: &str,
    mkt: &market::MarketState,
) -> Option<(Vec<bs::ChainRow>, f64)> {
    let bucket = sc.bucket(exch, prefix, expiry)?;
    let strikes: Vec<i64> = bucket.keys().copied().collect();
    if strikes.is_empty() {
        return None;
    }
    let atm_idx = if spot_in > 0.0 {
        let mut best = 0usize;
        let mut best_d = f64::MAX;
        for (i, sk) in strikes.iter().enumerate() {
            let d = ((*sk as f64 / 100.0) - spot_in).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    } else {
        strikes.len() / 2
    };
    let lo = atm_idx.saturating_sub(10);
    let hi = (atm_idx + 11).min(strikes.len());
    let mut rows: Vec<bs::ChainRow> = Vec::with_capacity(hi - lo);
    for sk in &strikes[lo..hi] {
        let Some(ent) = bucket.get(sk) else { continue };
        rows.push(bs::ChainRow {
            strike: *sk as f64 / 100.0,
            ce: ent
                .ce
                .as_ref()
                .map(|(sid, _)| quote_leg(mkt, *sid, seg))
                .unwrap_or_default(),
            pe: ent
                .pe
                .as_ref()
                .map(|(sid, _)| quote_leg(mkt, *sid, seg))
                .unwrap_or_default(),
        });
    }
    if rows.is_empty() {
        return None;
    }
    let spot = if spot_in > 0.0 {
        spot_in
    } else {
        rows[rows.len() / 2].strike
    };
    Some((rows, spot))
}

/// Launch the background Dhan call that replaces a scrip-master expiry ladder
/// with the broker's authoritative list (old app's background
/// `_fetch_option_expiries` cache fill). Deduplicated per underlying.
fn spawn_expiry_refresh(st: DhanState, key: String, sid: i64, seg: String, name: String) {
    if !mark_expiry_inflight(&key) {
        return;
    }
    tokio::spawn(async move {
        if !cooldown_active() {
            let (fno_sid, fno_seg) = scrip::get()
                .map(|sc| sc.resolve_underlying(&name, sid, &seg, ""))
                .unwrap_or_else(|| (sid, seg.clone()));
            match st.fetch_option_expiries(fno_sid, &fno_seg).await {
                Ok(list) if !list.is_empty() => cache_put_expiries(&key, list, true),
                Err(e) => {
                    if is_rate_limit(&e) {
                        set_cooldown();
                    }
                }
                _ => {}
            }
        }
        clear_expiry_inflight(&key);
    });
}

// ---------------------------------------------------------------------------
// Dhan mapping
// ---------------------------------------------------------------------------

fn dhan_rows(data: &OptionChainData, now: i64) -> Vec<bs::ChainRow> {
    let _ = now;
    let mut out: Vec<bs::ChainRow> = Vec::with_capacity(data.oc.len());
    for (strike_str, legs) in &data.oc {
        let strike: f64 = strike_str.parse().unwrap_or(0.0);
        let map_leg = |leg: &Option<dhan_hq::OptionLeg>| -> bs::ChainLeg {
            let Some(l) = leg else {
                return bs::ChainLeg::default();
            };
            let g = l.greeks.as_ref();
            let chg = l.last_price - l.previous_close_price;
            let chg_pct = if l.previous_close_price != 0.0 {
                chg / l.previous_close_price * 100.0
            } else {
                0.0
            };
            bs::ChainLeg {
                sid: l.security_id,
                ltp: l.last_price,
                chg,
                chg_pct,
                oi: l.oi,
                chg_oi: l.oi - l.previous_oi,
                vol: l.volume,
                iv: l.implied_volatility,
                bid: l.top_bid_price,
                ask: l.top_ask_price,
                delta: g.map(|x| x.delta).unwrap_or(0.0),
                theta: g.map(|x| x.theta).unwrap_or(0.0),
                gamma: g.map(|x| x.gamma).unwrap_or(0.0),
                vega: g.map(|x| x.vega).unwrap_or(0.0),
            }
        };
        out.push(bs::ChainRow {
            strike,
            ce: map_leg(&legs.ce),
            pe: map_leg(&legs.pe),
        });
    }
    out.sort_by(|a, b| a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn instrument_json(name: &str, lot: f64, trading_symbol: &str) -> Value {
    json!({
        "symbol_name": name,
        "trading_symbol": trading_symbol,
        "lot_size": lot,
    })
}

pub async fn expiries(State(st): State<DhanState>, Json(req): Json<OcReq>) -> impl IntoResponse {
    let name = underlying_name(req.security_id, &req.symbol_name);
    let (lot, tsym) = resolve_lot(&name, req.security_id, &req.exchange_segment);
    let key = expiry_key(req.security_id, &req.exchange_segment);

    // Fresh cache hit (authoritative Dhan list once the background fill ran,
    // otherwise the scrip ladder) is served without touching the network.
    if let Some((true, list, auth)) = cache_get_expiries(&key) {
        if !list.is_empty() {
            return Json(json!({ "status": "success", "data": list, "stale": !auth }))
                .into_response();
        }
    }

    // Local scrip-master ladder: instant, no Dhan dependency (old app returns
    // `_scrip_expiries` on a cold cache and never blocks the dropdown).
    if let Some(sc) = scrip::get() {
        let prefix = scrip::fno_underlying(&name);
        let exch = scrip::scrip_exch(&req.exchange_segment);
        if let Some(list) = sc.expiries_for(&prefix, exch) {
            let today = (market::now_secs() + 19800) / 86400;
            let list: Vec<String> = list
                .into_iter()
                .filter(|e| bs::parse_ymd(e).map(|d| d >= today).unwrap_or(true))
                .collect();
            if !list.is_empty() {
                cache_put_expiries(&key, list.clone(), false);
                spawn_expiry_refresh(
                    st.clone(),
                    key.clone(),
                    req.security_id,
                    req.exchange_segment.clone(),
                    name.clone(),
                );
                return Json(json!({ "status": "success", "data": list, "stale": true }))
                    .into_response();
            }
        }
        if matches!(exch, "MCX" | "NCDEX") && sc.has_options(&prefix, exch) == Some(false) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "status": "error",
                    "message": format!("No options listed for {}", name.to_uppercase()),
                    "instrument": instrument_json(&name, lot, &tsym),
                })),
            )
                .into_response();
        }
    }

    // Stale cached ladder (e.g. scrip master not loaded yet this session).
    if let Some((_, list, auth)) = cache_get_expiries(&key) {
        if !list.is_empty() {
            spawn_expiry_refresh(
                st.clone(),
                key.clone(),
                req.security_id,
                req.exchange_segment.clone(),
                name.clone(),
            );
            return Json(json!({ "status": "success", "data": list, "stale": !auth }))
                .into_response();
        }
    }

    // Futures-only MCX/NCDEX commodities have no listed options; never hit Dhan
    // (its empty body would arm the 30s cooldown and surface as "Rate limited").
    if market::commodity_has_options(req.security_id) == Some(false) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "status": "error",
                "message": format!("No options listed for {}", name.to_uppercase()),
                "instrument": instrument_json(&name, lot, &tsym),
            })),
        )
            .into_response();
    }
    if !cooldown_active() {
        let (fno_sid, fno_seg) = scrip::get()
            .map(|sc| sc.resolve_underlying(&name, req.security_id, &req.exchange_segment, ""))
            .unwrap_or_else(|| (req.security_id, req.exchange_segment.clone()));
        match st.fetch_option_expiries(fno_sid, &fno_seg).await {
            Ok(list) if !list.is_empty() => {
                cache_put_expiries(&key, list.clone(), true);
                return Json(json!({ "status": "success", "data": list })).into_response();
            }
            Err(e) => {
                if is_rate_limit(&e) {
                    set_cooldown();
                }
            }
            _ => {}
        }
    }
    // No live session / Dhan returned nothing: return an empty list rather than
    // any synthetic expiry ladder.
    Json(json!({
        "status": "success",
        "data": [],
        "stale": true,
    }))
    .into_response()
}

fn build_chain_value(
    name: &str,
    lot: f64,
    trading_symbol: &str,
    rows: &[bs::ChainRow],
    spot: f64,
    partial: bool,
) -> Value {
    let data: Vec<Value> = rows.iter().map(|r| r.to_json()).collect();
    json!({
        "status": "success",
        "data": data,
        "spot_price": spot,
        "count": rows.len(),
        "partial": partial,
        "instrument": instrument_json(name, lot, trading_symbol),
    })
}

/// Tag a chain that could not be refreshed because Dhan's option-chain surface
/// is in its 30s cooldown, so the client shows the old app's "Rate limited"
/// status instead of silently rendering stale data.
fn with_cooldown(mut v: Value) -> Value {
    if cooldown_active() {
        if let Some(o) = v.as_object_mut() {
            o.insert("cooldown".to_string(), Value::Bool(true));
            o.insert(
                "message".to_string(),
                Value::String("Rate limited - option chain temporarily unavailable".into()),
            );
        }
    }
    v
}

/// Ensure an all-expiries chain carries the `records` array and `expiry` key the
/// client renders (the single-chain route names it `data` and omits `expiry`).
fn with_records(mut v: Value, expiry: &str) -> Value {
    let recs = v
        .get("records")
        .cloned()
        .or_else(|| v.get("data").cloned())
        .unwrap_or_else(|| json!([]));
    if let Some(o) = v.as_object_mut() {
        o.insert("expiry".to_string(), json!(expiry));
        o.insert("records".to_string(), recs);
    }
    v
}

fn no_options_response(name: &str) -> Json<Value> {
    Json(json!({
        "status": "error",
        "no_options": true,
        "message": format!("No options listed for {name}"),
        "data": [],
        "count": 0,
    }))
}

pub async fn option_chain(
    State(st): State<DhanState>,
    Json(req): Json<OcReq>,
) -> impl IntoResponse {
    let name = underlying_name(req.security_id, &req.symbol_name);
    let (lot, tsym) = resolve_lot(&name, req.security_id, &req.exchange_segment);
    if req.expiry.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": "expiry is required" })),
        )
            .into_response();
    }
    if bs::parse_ymd(&req.expiry).is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "status": "error", "message": "Invalid expiry for this symbol" })),
        )
            .into_response();
    }
    // Futures-only MCX commodities have no options at all.
    if market::commodity_has_options(req.security_id) == Some(false) {
        return no_options_response(&name).into_response();
    }

    let key = cache_key(req.security_id, &req.exchange_segment, &req.expiry);

    // Fresh cache hit: serve instantly.
    if let Some((true, v)) = cache_get(&key) {
        return Json(v).into_response();
    }

    // Stale cache: serve it as-is. Live LTP/OI/Volume/IV/Greeks are pushed over
    // the websocket feed and merged into the cached records client-side, so no
    // REST chain refresh is needed here.
    if let Some((_, v)) = cache_get(&key) {
        return Json(v).into_response();
    }

    // Instant scrip-master chain (old app's `_build_oc_instant`): real strikes
    // and CE/PE security ids render in milliseconds; `subscribe_options` arms the
    // live websocket feed for those ids. Every live column (LTP/Chg/Chg%/OI/
    // Chg OI/Volume/IV/Bid/Ask/Greeks) then arrives over the feed; only the
    // expiry ladder and contract lookup still use REST.
    if let Some(sc) = scrip::get() {
        let prefix = scrip::fno_underlying(&name);
        let exch = scrip::scrip_exch(&req.exchange_segment);
        let spot_in = known_spot(&st, req.security_id, &req.exchange_segment, req.spot);
        if let Some((rows, spot)) = scrip_partial_chain(
            &sc,
            &prefix,
            exch,
            &req.expiry,
            spot_in,
            option_seg(&req.exchange_segment),
            &st.market,
        ) {
            register_chain(
                &st,
                &rows,
                &req.exchange_segment,
                req.security_id,
                &req.expiry,
                spot,
                lot,
            )
            .await;
            // The chain structure is final; live columns stream over the
            // websocket, so it is not "partial" (no REST upgrade to wait for).
            let v = build_chain_value(&name, lot, &tsym, &rows, spot, false);
            cache_put(&key, v.clone());
            return Json(v).into_response();
        }
        if matches!(exch, "MCX" | "NCDEX") && sc.has_options(&prefix, exch) == Some(false) {
            return no_options_response(&name).into_response();
        }
    } else if scrip::pending() {
        // First-ever load while the scrip master is still downloading: answer the
        // old app's `status:"loading"`; the client retries until it lands.
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "loading",
                "message": "Option chain loading",
                "instrument": instrument_json(&name, lot, &tsym),
            })),
        )
            .into_response();
    }

    // No connected session produced a chain: return an empty response rather
    // than any synthetic chain.
    Json(with_cooldown(json!({
        "status": "success",
        "data": [],
        "spot_price": req.spot,
        "count": 0,
        "partial": true,
        "instrument": instrument_json(&name, lot, &tsym),
    })))
    .into_response()
}

/// Testing-only Manual Strike Select picker: for a chosen underlying (and,
/// optionally, a specific expiry) return the expiry ladder plus a wide strike
/// window centred on ATM, each row carrying the CE/PE security id and LTP. The
/// returned legs are registered with the live feed so their LTP streams while the
/// operator picks. Engine-agnostic - the real and paper tabs share it.
pub async fn manual_strike_chain(
    State(st): State<DhanState>,
    Json(req): Json<ManualStrikeReq>,
) -> impl IntoResponse {
    let name = underlying_name(req.security_id, &req.symbol_name);
    let (lot, tsym) = resolve_lot(&name, req.security_id, &req.exchange_segment);
    if market::commodity_has_options(req.security_id) == Some(false) {
        return no_options_response(&name).into_response();
    }
    let Some(sc) = scrip::get() else {
        return (
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "loading",
                "message": "Scrip master loading - retry in a moment",
                "instrument": instrument_json(&name, lot, &tsym),
            })),
        )
            .into_response();
    };
    let prefix = scrip::fno_underlying(&name);
    let exch = scrip::scrip_exch(&req.exchange_segment);
    let expiries: Vec<String> = sc.expiries_for(&prefix, exch).unwrap_or_default();
    // Honour an explicit, still-listed expiry; otherwise take the nearest one.
    let expiry = if !req.expiry.trim().is_empty() && expiries.iter().any(|e| e == req.expiry.trim()) {
        req.expiry.trim().to_string()
    } else {
        expiries.first().cloned().unwrap_or_default()
    };
    if expiry.is_empty() {
        return no_options_response(&name).into_response();
    }
    let Some(bucket) = sc.bucket(exch, &prefix, &expiry) else {
        return no_options_response(&name).into_response();
    };
    let strikes: Vec<i64> = bucket.keys().copied().collect();
    if strikes.is_empty() {
        return no_options_response(&name).into_response();
    }
    let spot = known_spot(&st, req.security_id, &req.exchange_segment, req.spot);
    let atm = if spot > 0.0 {
        let mut best = 0usize;
        let mut best_d = f64::MAX;
        for (i, sk) in strikes.iter().enumerate() {
            let d = ((*sk as f64 / 100.0) - spot).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best
    } else {
        strikes.len() / 2
    };
    let w = if req.window <= 0 { 30 } else { req.window.clamp(1, 60) } as usize;
    let lo = atm.saturating_sub(w);
    let hi = (atm + w + 1).min(strikes.len());
    let seg = option_seg(&req.exchange_segment);
    let mut rows: Vec<bs::ChainRow> = Vec::with_capacity(hi - lo);
    for sk in &strikes[lo..hi] {
        let Some(ent) = bucket.get(sk) else { continue };
        rows.push(bs::ChainRow {
            strike: *sk as f64 / 100.0,
            ce: ent
                .ce
                .as_ref()
                .map(|(sid, _)| quote_leg(&st.market, *sid, seg))
                .unwrap_or_default(),
            pe: ent
                .pe
                .as_ref()
                .map(|(sid, _)| quote_leg(&st.market, *sid, seg))
                .unwrap_or_default(),
        });
    }
    // Arm the live feed + seed previous close / LTP / OI for the whole window.
    register_chain(&st, &rows, &req.exchange_segment, req.security_id, &expiry, spot, lot).await;
    let data: Vec<Value> = rows.iter().map(|r| r.to_json()).collect();
    Json(json!({
        "status": "success",
        "symbol_name": name,
        "trading_symbol": tsym,
        "lot_size": lot,
        "expiry": expiry,
        "expiries": expiries,
        "spot": spot,
        "count": data.len(),
        "data": data,
    }))
    .into_response()
}

pub async fn option_chain_all(
    State(st): State<DhanState>,
    Json(req): Json<OcReq>,
) -> impl IntoResponse {
    let name = underlying_name(req.security_id, &req.symbol_name);
    let (lot, tsym) = resolve_lot(&name, req.security_id, &req.exchange_segment);
    if market::commodity_has_options(req.security_id) == Some(false) {
        return no_options_response(&name);
    }
    let scrip_ctx = scrip::get().map(|sc| {
        let prefix = scrip::fno_underlying(&name);
        let exch = scrip::scrip_exch(&req.exchange_segment).to_string();
        (sc, prefix, exch)
    });
    let expiries = match scrip_ctx.as_ref().and_then(|(sc, prefix, exch)| {
        sc.expiries_for(prefix, exch)
    }) {
        Some(list) if !list.is_empty() => {
            let today = (market::now_secs() + 19800) / 86400;
            list.into_iter()
                .filter(|e| bs::parse_ymd(e).map(|d| d >= today).unwrap_or(true))
                .collect::<Vec<String>>()
        }
        _ => match st
            .fetch_option_expiries(req.security_id, &req.exchange_segment)
            .await
        {
            Ok(list) if !list.is_empty() => list,
            _ => Vec::new(),
        },
    };
    let mut chains: Vec<Value> = Vec::new();
    let mut errors: Vec<Value> = Vec::new();
    for expiry in &expiries {
        let key = cache_key(req.security_id, &req.exchange_segment, expiry);
        if let Some((true, v)) = cache_get(&key) {
            chains.push(with_records(v, expiry));
            continue;
        }
        // Instant scrip-master chain for this expiry (real strikes + ids). The
        // live columns arrive over the websocket feed, so no REST chain fetch.
        if let Some((sc, prefix, exch)) = scrip_ctx.as_ref() {
            let spot_in = known_spot(&st, req.security_id, &req.exchange_segment, req.spot);
            if let Some((rows, spot)) = scrip_partial_chain(
                sc,
                prefix,
                exch,
                expiry,
                spot_in,
                option_seg(&req.exchange_segment),
                &st.market,
            ) {
                register_chain(
                    &st,
                    &rows,
                    &req.exchange_segment,
                    req.security_id,
                    expiry,
                    spot,
                    lot,
                )
                .await;
                chains.push(json!({
                    "expiry": expiry,
                    "records": rows.iter().map(|r| r.to_json()).collect::<Vec<Value>>(),
                    "spot_price": spot,
                    "count": rows.len(),
                    "partial": false,
                    "instrument": instrument_json(&name, lot, &tsym),
                }));
                continue;
            }
        }
        errors.push(json!({ "expiry": expiry, "error": "no option chain available" }));
    }
    let mut resp = json!({
        "status": "success",
        "expiries": expiries,
        "chains": chains,
        "errors": errors,
        "instrument": instrument_json(&name, lot, &tsym),
    });
    if cooldown_active() {
        if let Some(o) = resp.as_object_mut() {
            o.insert("cooldown".to_string(), Value::Bool(true));
            o.insert(
                "message".to_string(),
                Value::String("Rate limited - option chain temporarily unavailable".into()),
            );
        }
    }
    Json(resp)
}

/// Subscribe option strikes registered by the tab to the live quote feed.
/// Body: `{securities:[{security_id, exchange_segment}]}`.
pub async fn oc_subscribe(
    State(st): State<DhanState>,
    Json(req): Json<OcSubReq>,
) -> impl IntoResponse {
    let list: Vec<(i64, String)> = req
        .securities
        .iter()
        .filter(|s| s.security_id != 0 && !s.exchange_segment.is_empty())
        .map(|s| (s.security_id, s.exchange_segment.to_uppercase()))
        .collect();
    st.subscribe_options(&list).await;
    Json(json!({ "status": "success", "subscribed": list.len() }))
}

pub async fn auto_strikes(
    State(st): State<DhanState>,
    Json(req): Json<AutoStrikesReq>,
) -> impl IntoResponse {
    let name = underlying_name(req.security_id, &req.symbol_name);
    let (lot, _tsym) = resolve_lot(&name, req.security_id, &req.exchange_segment);
    let mode_owned = if req.mode.is_empty() {
        "both_atm".to_string()
    } else {
        req.mode.clone()
    };
    let mode = mode_owned.as_str();
    let count = req.count.max(1) as usize;

    let expiry = match st
        .fetch_option_expiries(req.security_id, &req.exchange_segment)
        .await
    {
        Ok(list) if !list.is_empty() => list[0].clone(),
        _ => {
            return Json(json!({
                "status": "error",
                "message": "no expiries available (not connected to Dhan)",
                "data": [],
            }));
        }
    };

    let (rows, spot) = match st
        .fetch_option_chain(req.security_id, &req.exchange_segment, &expiry)
        .await
    {
        Ok(data) => {
            let rows = dhan_rows(&data, market::now_secs());
            if rows.is_empty() {
                return Json(json!({
                    "status": "error",
                    "message": "no option chain available",
                    "data": [],
                }));
            }
            (rows, data.last_price)
        }
        Err(_) => {
            return Json(json!({
                "status": "error",
                "message": "no option chain available (not connected to Dhan)",
                "data": [],
            }));
        }
    };

    let strikes: Vec<f64> = rows.iter().map(|r| r.strike).collect();
    let spot_use = if req.spot > 0.0 { req.spot } else { spot };
    let atm = bs::atm_index(&strikes, spot_use);

    let pick: Vec<usize> = match mode {
        "above" => ((atm + 1)..=(atm + 1 + count)).filter(|i| *i < rows.len()).collect(),
        "below" => (atm.saturating_sub(count)..atm).collect(),
        "above_atm" => (atm..=(atm + count)).filter(|i| *i < rows.len()).collect(),
        "below_atm" => (atm.saturating_sub(count)..=atm).collect(),
        "both_atm_inc" => ((atm.saturating_sub(count))..=(atm + count))
            .filter(|i| *i < rows.len())
            .collect(),
        "atm" => vec![atm],
        _ => {
            let mut v: Vec<usize> = (atm.saturating_sub(count)..atm).collect();
            v.extend(((atm + 1)..=(atm + count)).filter(|i| *i < rows.len()));
            v
        }
    };

    let data: Vec<Value> = pick
        .into_iter()
        .filter_map(|i| rows.get(i))
        .map(|r| {
            json!({
                "strike": r.strike,
                "ce_ltp": r.ce.ltp,
                "pe_ltp": r.pe.ltp,
                "ce_chg": r.ce.chg,
                "pe_chg": r.pe.chg,
                "ce_chg_pct": r.ce.chg_pct,
                "pe_chg_pct": r.pe.chg_pct,
                "ce_delta": r.ce.delta,
                "pe_delta": r.pe.delta,
                "ce_oi": r.ce.oi,
                "pe_oi": r.pe.oi,
                "ce_chg_oi": r.ce.chg_oi,
                "pe_chg_oi": r.pe.chg_oi,
                "ce_volume": r.ce.vol,
                "pe_volume": r.pe.vol,
                "ce_sid": r.ce.sid,
                "pe_sid": r.pe.sid,
                "lot_size": lot,
            })
        })
        .collect();

    let option_type = if req.option_type.is_empty() {
        "both".to_string()
    } else {
        req.option_type.clone()
    };
    Json(json!({
        "status": "success",
        "spot": spot_use,
        "expiry": expiry,
        "mode": mode,
        "count": count,
        "option_type": option_type,
        "lot_size": lot,
        "data": data,
    }))
}

/// Instrument type for an option contract: MCX → OPTFUT, an index underlying →
/// OPTIDX, otherwise OPTSTK.
fn option_instrument_type(name: &str, seg: &str) -> &'static str {
    if seg.to_uppercase().contains("MCX") {
        return "OPTFUT";
    }
    let u = name.trim().to_uppercase();
    let idx = u.contains("NIFTY")
        || u.contains("SENSEX")
        || u.contains("VIX");
    if idx {
        "OPTIDX"
    } else {
        "OPTSTK"
    }
}

pub async fn option_security(
    State(st): State<DhanState>,
    Json(req): Json<OptionSecurityReq>,
) -> impl IntoResponse {
    if req.symbol_name.trim().is_empty()
        || req.expiry.trim().is_empty()
        || req.strike <= 0.0
        || (req.option_type != "CE" && req.option_type != "PE")
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"status":"error","message":"symbol_name, expiry, strike and option_type are required"})),
        )
            .into_response();
    }
    let is_call = req.option_type == "CE";
    let inst = option_instrument_type(&req.symbol_name, &req.exchange_segment);
    let (lot, _tsym) = resolve_lot(&req.symbol_name, req.security_id, &req.exchange_segment);

    // Real scrip-master resolution (old app's `_resolve_option_security`):
    // exact exchange+prefix+expiry+strike, then nearest expiry, then a
    // cross-listed exchange. Never requires a broker session.
    if let Some(sc) = scrip::get() {
        if let Some(r) = sc.resolve(
            &req.symbol_name,
            &req.expiry,
            req.strike,
            &req.option_type,
            &req.exchange_segment,
        ) {
            return Json(json!({
                "status": "success",
                "data": {
                    "security_id": r.security_id,
                    "exchange_segment": req.exchange_segment,
                    "instrument_type": inst,
                    "trading_symbol": r.trading_symbol,
                    "lot_size": r.lot,
                    "expiry": r.expiry,
                }
            }))
            .into_response();
        }
    }

    // Fallback: look the contract up in Dhan's option chain when a session is
    // connected; otherwise use the deterministic synthetic id.
    if req.security_id != 0 {
        let expiries = st
            .fetch_option_expiries(req.security_id, &req.exchange_segment)
            .await
            .unwrap_or_default();
        let expiry = expiries
            .iter()
            .find(|e| e.as_str() == req.expiry.as_str())
            .cloned()
            .or_else(|| expiries.first().cloned())
            .unwrap_or_else(|| req.expiry.clone());
        if let Ok(data) = st
            .fetch_option_chain(req.security_id, &req.exchange_segment, &expiry)
            .await
        {
            let rows = dhan_rows(&data, market::now_secs());
            if let Some(row) = rows.iter().find(|r| (r.strike - req.strike).abs() < 0.001) {
                let leg = if is_call { &row.ce } else { &row.pe };
                if leg.sid != 0 {
                    return Json(json!({
                        "status": "success",
                        "data": {
                            "security_id": leg.sid,
                            "exchange_segment": req.exchange_segment,
                            "instrument_type": inst,
                            "trading_symbol": format!(
                                "{}{}{}",
                                req.symbol_name.replace(' ', "").to_uppercase(),
                                req.strike as i64,
                                req.option_type
                            ),
                            "lot_size": lot,
                        }
                    }))
                    .into_response();
                }
            }
        }
    }

    // Not resolvable via the scrip master or Dhan: no contract id to return.
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "status": "error",
            "message": "could not resolve option contract (not connected to Dhan)",
        })),
    )
        .into_response()
}

pub async fn lot_sizes() -> impl IntoResponse {
    // Preferred source: the scrip master's real SEM_LOT_UNITS for every F&O
    // underlying + the index UI names (old `_build_lot_size_map`).
    if let Some(sc) = scrip::get() {
        let (by_prefix, by_name) = sc.lot_map();
        if !by_prefix.is_empty() {
            return Json(json!({
                "status": "success",
                "data": { "by_prefix": by_prefix, "by_name": by_name }
            }));
        }
    }
    let by_name = json!({
        "NIFTY 50": 75.0,
        "BANK NIFTY": 35.0,
        "FINNIFTY": 65.0,
        "SENSEX": 20.0,
        "MIDCPNIFTY": 120.0,
        "GIFT NIFTY": 15.0,
    });
    let by_prefix = json!({
        "NIFTY": 75.0, "BANKNIFTY": 35.0, "FINNIFTY": 65.0,
        "SENSEX": 20.0, "MIDCPNIFTY": 120.0, "GIFTNIFTY": 15.0,
    });
    Json(json!({"status":"success","data":{"by_prefix":by_prefix,"by_name":by_name}}))
}

// ---------------------------------------------------------------------------
// OI Trend helper: option-chain rows -> OiRecord (used by the chart overlay)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
pub fn rows_to_oi(rows: &[bs::ChainRow]) -> Vec<OiRecord> {
    rows.iter().map(|r| r.oi_record()).collect()
}

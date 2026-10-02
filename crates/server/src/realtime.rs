//! Realtime Trading Engine.
//!
//! New, self-contained Rust implementation of the old Flask app's "Realtime
//! Trading Engine" tab. It runs the same shape of live strategy engine as AI
//! Smart Trading and routes every order to the *real* Dhan broker adapter. Only
//! live Dhan orders are supported - there is no paper or simulated mode.
//!
//! Safety: the engine is DISARMED by default. No entry and no exit order is sent
//! unless the operator arms it. Square-off (auto or manual) is always allowed.
//!
//! Universal settings, the four order methods (Normal / Super / Forever / Slice)
//! and the per-method side trail behaviour mirror the old app:
//!   * Normal / Forever / Slice -> app-side tick trail (TICK_MS = 300).
//!   * Super -> the algo ratchets the stop up by the configured percent AND
//!     mirrors it onto Dhan's native STOP_LOSS_LEG, which keeps trailing by its
//!     own fixed points jump (both mechanisms run together).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use dhan_hq::models::{
    ExchangeSegment, LegName, MarginRequest, MarginResponse, ModifyOrderRequest, OrderRequest, OrderType,
    ProductType, TransactionType, Validity,
};
use algo_core::model::Candle;

use crate::broker::DhanState;
use crate::market::MarketState;
use crate::scrip;

// ---------------------------------------------------------------------------
// JSON helpers (engine state is stored as flexible JSON maps, like the old app)
// ---------------------------------------------------------------------------

fn jf(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}
fn ji(v: &Value, k: &str) -> i64 {
    v.get(k).and_then(|x| x.as_i64()).or_else(|| v.get(k).and_then(|x| x.as_f64()).map(|f| f as i64)).unwrap_or(0)
}
fn jb(v: &Value, k: &str) -> bool {
    v.get(k).and_then(|x| x.as_bool()).unwrap_or(false)
}
fn js(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn jarr(v: &Value, k: &str) -> Vec<Value> {
    v.get(k).and_then(|x| x.as_array()).cloned().unwrap_or_default()
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn gen_id(prefix: &str) -> String {
    format!("{prefix}_{}", now_ms())
}

/// Group Manual Strike Select picks into `(underlying security id, has CE, has
/// PE)` so the synthetic scanner universe can be rebuilt from them: one side per
/// option type the operator actually picked. Pure and order-stable.
fn manual_target_specs(strikes: &[ManualStrike]) -> Vec<(i64, bool, bool)> {
    let mut map: std::collections::BTreeMap<i64, (bool, bool)> = std::collections::BTreeMap::new();
    for m in strikes {
        if m.security_id <= 0 {
            continue;
        }
        let ce = m.option_type.eq_ignore_ascii_case("CE");
        let pe = m.option_type.eq_ignore_ascii_case("PE");
        if !ce && !pe {
            continue;
        }
        let e = map.entry(m.security_id).or_insert((false, false));
        if ce {
            e.0 = true;
        } else {
            e.1 = true;
        }
    }
    map.into_iter().map(|(sid, (ce, pe))| (sid, ce, pe)).collect()
}

/// Dhan's order `correlationId` must be a short, unique token: the engine's
/// internal strategy ids (`scan:12345:CE`, `rtstrat_1789...`) carry `:` / `_`
/// and repeat on every entry, which Dhan rejects with
/// `DH-905 (Input_Exception): Invalid correlationId` - so every order is sent a
/// fresh sanitized value instead. Kept alphanumeric and <= 23 chars.
fn corr_id(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut safe: String = tag.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    if safe.is_empty() {
        safe.push_str("ord");
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{safe}{}{:04}", now_ms() % 100_000_000_000, n % 10_000)
}

#[cfg(test)]
mod corr_tests {
    use super::corr_id;

    /// Dhan rejects ids that are non-alphanumeric / longer than 25 chars with
    /// `DH-905`, so every generated id must stay within those bounds even for
    /// the engine's `scan:12345:CE` / `rtstrat_...` strategy ids.
    #[test]
    fn correlation_ids_are_alphanumeric_short_and_unique() {
        let ids: Vec<String> = (0..200)
            .map(|i| corr_id(if i % 2 == 0 { "scan:12345:CE" } else { "rtstrat_1789836974927" }))
            .collect();
        for id in &ids {
            assert!(id.chars().all(|c| c.is_ascii_alphanumeric()), "non-alnum: {id}");
            assert!(id.len() <= 25, "too long: {id}");
            assert!(!id.is_empty());
        }
        let mut uniq = ids.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), ids.len(), "correlation ids must be unique");
    }
}

/// Current IST wall-clock as minutes past midnight.
fn ist_minutes() -> i64 {
    let secs = crate::market::now_secs() + 19800;
    (secs.rem_euclid(86400)) / 60
}

fn hhmm_to_minutes(s: &str) -> Option<i64> {
    let (h, m) = s.split_once(':')?;
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    Some(h * 60 + m)
}

/// One operator-defined intraday trading window (Trade times -> sessions).
/// `start`/`end` are IST `HH:MM` strings; the window is inclusive on both ends.
/// `enabled` lets a session be parked without deleting it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TradeSession {
    pub start: String,
    pub end: String,
    pub enabled: bool,
}

impl Default for TradeSession {
    fn default() -> Self {
        Self { start: "09:15".into(), end: "15:30".into(), enabled: true }
    }
}

/// One operator-picked option contract for the testing-only "Manual Strike
/// Select" mode. When the mode is on, the engine resolves a strategy's option
/// leg to one of these contracts (matching underlying + side) instead of the
/// automatic ATM / ITM / OTM selection. Only the underlying identity + the
/// strike intent are stored; the exact option contract id is resolved from the
/// scrip master at entry time (so a rolled contract id can never go stale).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ManualStrike {
    /// UI symbol name of the underlying, e.g. `NIFTY 50`, `RELIANCE`, `CRUDEOIL`.
    pub symbol_name: String,
    /// Underlying (cash/index/futures) security id, used for matching + display.
    pub security_id: i64,
    /// Underlying exchange segment, e.g. `IDX_I`, `NSE_EQ`, `MCX_COMM`.
    pub exchange_segment: String,
    /// Option expiry `YYYY-MM-DD`.
    pub expiry: String,
    pub strike: f64,
    /// `CE` | `PE`.
    pub option_type: String,
    /// Lot size for the underlying (display only; the engine re-resolves it).
    pub lot: f64,
}

impl Default for ManualStrike {
    fn default() -> Self {
        Self {
            symbol_name: String::new(),
            security_id: 0,
            exchange_segment: String::new(),
            expiry: String::new(),
            strike: 0.0,
            option_type: String::new(),
            lot: 0.0,
        }
    }
}

/// Operator-assigned option leg for a scanner underlying - one Top Movers index
/// (`movers_index_legs`) or one commodity (`commodity_legs`). The underlying
/// only trades the side the operator explicitly picked - a bullish view runs CE,
/// a bearish view PE - and an index with no entry is skipped entirely (a
/// commodity with no entry keeps the old global-side behaviour). The assigned
/// leg is authoritative: it wins over the NIFTY straight-line lock, the "Run
/// Strategy In" override and the Top Movers auto bias for that instrument only,
/// and it selects which indicator-filter side applies (CE = bullish filters,
/// PE = bearish filters).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MoversIndexLeg {
    /// Index security id, e.g. `13` (NIFTY 50).
    pub security_id: i64,
    /// `CE` | `PE`.
    pub side: String,
}

impl Default for MoversIndexLeg {
    fn default() -> Self {
        Self {
            security_id: 0,
            side: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Universal settings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub lot_size: f64,
    pub lots: f64,
    /// Margin budget as a percentage (0-100) of the live available balance.
    /// The engine locks at most this share across running trades and blocks the
    /// next entry whose required margin would exceed the remaining budget.
    pub margin_pct: f64,
    /// Manual margin amount (rupees). When `> 0` it takes priority over
    /// `margin_pct`: the engine locks at most this many rupees (capped by the
    /// live available balance) across running trades. `0` = use the percentage.
    pub margin_amount: f64,
    /// When ON, an entry signal that cannot be filled right now because the
    /// margin budget is exhausted (required > available) is SKIPPED immediately:
    /// the signal edge is consumed so the engine never re-queues/retries it and
    /// never fills it minutes later at a worse price. Default OFF (legacy retry,
    /// which releases the edge and lets the same signal fill once margin frees).
    pub skip_on_margin_block: bool,
    pub sl_auto: bool,
    pub tf_1min: bool,
    pub tf_5min: bool,
    pub mtf: bool,
    pub use_own_settings: bool,
    pub ai_sl: bool,
    pub ai_trail_tp: bool,
    pub ai_tp_pct: bool,
    pub manual_sl: bool,
    pub manual_sl_pct: f64,
    pub manual_trail_sl: bool,
    pub manual_trail_sl_pct: f64,
    /// Dhan-native points trail (Super-Order style): a fixed price jump behind
    /// the best price. Applied broker-side on real orders, simulated in paper.
    pub manual_point_trail_sl: bool,
    pub manual_point_trail_sl_points: f64,
    pub manual_trail_tp: bool,
    pub manual_trail_tp_pct: f64,
    pub rr_enabled: bool,
    pub rr_value: f64,
    pub trade_limit: bool,
    pub trade_limit_count: i64,
    pub ai_trades: bool,
    /// Global order-rate cap in orders per second. Every entry the engine opens
    /// (live or paper) is throttled so no more than this many orders are sent in
    /// any trailing one-second window. Dhan's API allows ~6/sec; clamped 1..=30.
    pub order_per_sec: i64,
    /// "Engine Scan" throttle toggle. When ON, the engine re-scans its entry
    /// conditions only every `scan_interval_ms` instead of the ultrafast ~100ms
    /// tick. Exits (SL / trail / TP) are NEVER throttled - they stay on the
    /// dedicated guardian task. When OFF, the default ~100ms cadence is used.
    pub scan_interval_on: bool,
    /// "Engine Scan" interval in milliseconds. Only meaningful while
    /// `scan_interval_on`; normalised to 50..=86_400_000 (24h) with a 50ms floor
    /// so a stray 0 can never spin the loop. Default 100 = the normal tick.
    pub scan_interval_ms: i64,
    /// Engine Scan sub-mode: instead of a fixed interval, scan once per bar close
    /// - i.e. the instant the selected timeframe rolls to a new bar. When on, the
    /// interval inputs are ignored (the UI disables them).
    pub scan_bar_close: bool,
    /// Timeframe whose bar close triggers the scan: `1min` | `5min`. Only
    /// meaningful while `scan_bar_close` is on.
    pub scan_bar_close_tf: String,
    pub start_after_enabled: bool,
    pub start_after: String,
    pub no_trade_after_enabled: bool,
    pub no_trade_after: String,
    pub auto_square_off_enabled: bool,
    pub auto_square_off_time: String,
    /// Operator-defined intraday trading sessions (multiple windows per day).
    /// When at least one enabled, well-formed session exists, new entries are
    /// only allowed inside one of them; the single Start/No-trade envelope above
    /// still applies as an outer bound. Empty list = no extra restriction.
    pub trade_sessions: Vec<TradeSession>,
    pub option_type: String,
    pub strike_mode: String,
    pub strike_count: i64,
    /// "Only +green premium strikes": when ON, the engine walks the strikes in
    /// the direction chosen by the "Execute Trade In" dropdown (`strike_mode`) and
    /// only a strike whose premium shows a plus on BOTH its live LTP
    /// (tick-to-tick) and its published change is eligible. A minus/flat strike is
    /// skipped and the walk continues to the next strike in the same leg (NOT
    /// capped by `strike_count`); `None` - the entry is skipped - only when no
    /// strike in that whole direction is plus, so a falling premium is never
    /// bought. Signs are read from the live websocket cache, never derived from a
    /// previous close.
    pub only_positive: bool,
    /// "Opposite side fallback": only meaningful while `only_positive` is ON and a
    /// single-sided "Execute Trade In" mode is chosen (Above/Below, incl. ATM).
    /// When the ENTIRE selected side has a live quote on every strike and none of
    /// them is plus (all minus), the engine automatically walks the OPPOSITE side
    /// of ATM (Above -> Below and vice-versa) in the SAME option leg (CE/PE) and
    /// trades the first plus strike found there. While the selected side still has
    /// unquoted candidates the walk keeps warming them instead of falling back, so
    /// a plus strike on the chosen side is never missed. ON is the default: the
    /// "Execute Trade In" dropdown is designed to run on the +green base.
    pub fallback_opposite_side: bool,
    /// "Pick fastest positive rising LTP": when ON, the engine scans the SAME
    /// direction chosen by the "Execute Trade In" dropdown and, among the strikes
    /// whose premium is plus on BOTH its live LTP (tick-to-tick) and its published
    /// change, picks the one with the HIGHEST published `change_pct` (LTP change %),
    /// breaking ties by the absolute LTP `change`. Only a rising premium is ever
    /// eligible, so a falling strike can never be picked. Works together with
    /// "Only +green premium strikes" (which narrows to rising premiums) and with
    /// "Opposite side fallback" (when the selected side is entirely minus). When
    /// OFF the +green nearest-strike walk is used. `fastest_count` is kept for the
    /// UI round-trip but does NOT cap the scan.
    pub fastest_rising: bool,
    pub fastest_count: i64,
    /// Testing-only "Manual Strike Select": when on, the ATM / strike-mode /
    /// count preferences are ignored and the engine
    /// trades only the operator-picked contracts in `manual_strikes`.
    pub manual_strikes_enabled: bool,
    /// Operator-picked option contracts for Manual Strike Select.
    pub manual_strikes: Vec<ManualStrike>,
    pub premium_only: bool,
    pub run_index: String,
    pub run_fno: String,
    pub run_comm: String,
    /// Live Data Pool readout toggle (UI diagnostic; the strategies read the
    /// exact same pool regardless).
    pub data_pool: bool,
    // --- Trade-in chart selection (old AST "Trade should be executed in") ---
    pub trade_in_index: String,
    pub trade_in_fno: String,
    pub trade_in_comm: String,
    pub trade_in_default: bool,
    pub run_in_default: bool,
    // --- Top Movers ---
    pub movers_on: bool,
    pub movers_gainers: i64,
    pub movers_losers: i64,
    pub movers_indices: Vec<i64>,
    /// Operator-assigned CE/PE leg per Top Movers index. An index in
    /// `movers_indices` with no entry here is skipped (index trading needs an
    /// explicit trend call); with an entry it trades ONLY that leg.
    pub movers_index_legs: Vec<MoversIndexLeg>,
    // --- NIFTY Trend Following ---
    pub nifty_trend_on: bool,
    pub nifty_trend_pct_on: bool,
    pub nifty_trend_pct: f64,
    pub nifty_trend_conf_inds: Vec<String>,
    /// Number of straight-line points used to read each confirmation indicator's
    /// trend. The direction is the net move across that window of points - the
    /// same straight segment the chart draws - so a single bar's blip can no
    /// longer flip the read (the old one-point slope did exactly that).
    /// Minimum 3 points; higher = smoother. Clamped to >= 3 by `nifty_slope_len`.
    pub nifty_trend_slope_len: i64,
    /// NIFTY Trend Following confirmation read mode. When ON, each selected
    /// indicator's direction is read from the COLOUR the chart draws on the line's
    /// last closed candle (green = bullish, red = bearish) instead of the windowed
    /// net move; when OFF the normal `nifty_trend_slope_len` window read is used.
    pub nifty_trend_color: bool,
    /// Strict NET-direction gate for the NIFTY-trend universe (Top Movers
    /// Gainers/Losers + NIFTY-trend picks). When ON, a Top Gainer (CE) leg may
    /// only arm while the assigned straight-line confirmation indicators' NET
    /// direction is bullish, and a Top Loser (PE) leg only while it is bearish.
    /// A mismatch - including a tie (net 0) - blocks the leg outright, even when
    /// the ordinary gate (majority / AI Brain) would otherwise allow it.
    pub nifty_net_strict: bool,
    /// Strict ALL-LINES-AGREE gate for the same universe. When ON, a Top Gainer
    /// (CE) leg may only arm while EVERY assigned straight-line confirmation
    /// indicator reads bullish (no dissenting/bearish line), and a Top Loser (PE)
    /// leg only while EVERY assigned line reads bearish. Any mixed read blocks.
    pub nifty_all_agree_strict: bool,
    // --- Commodities ---
    pub commodity_on: bool,
    pub commodity_list: Vec<i64>,
    /// Operator-assigned CE/PE leg per commodity, exactly like
    /// `movers_index_legs` for indices: the chosen leg is the only leg the
    /// commodity trades and it drives which indicator filters apply (CE trades
    /// the bullish filter set, PE the bearish one). A commodity in
    /// `commodity_list` with no entry here follows the global scanner side as
    /// before. Only commodities that actually list options (OPTFUT) can trade a
    /// CE/PE leg.
    pub commodity_legs: Vec<MoversIndexLeg>,
    /// Underlying security ids the operator removed from the scanner picks
    /// (Top Movers gainers/losers). An excluded id never enters the
    /// scanner universe, so it is not ranked, not resolved to an option leg and
    /// not traded until restored. Persisted with the rest of the settings.
    pub scanner_exclude: Vec<i64>,
    // --- Run mode (old AST run-mode section) ---
    /// Run mode: `false` = Normal mode (run the ticked/enabled/AI-picked
    /// strategies), `true` = Indicator-filters mode (trade the scanner universe
    /// - Top Movers / Commodities - with the ticked
    /// Bullish/Bearish filters agreeing by majority; tick `all_in_one` to demand
    /// every filter, or enable AI Brain for a score/veto gate).
    pub filter_mode: bool,
    // --- Entry gate modes ---
    pub all_in_one: bool,
    pub dir_guard: bool,
    pub overall_dir: bool,
    /// "Filter-side routing": when ON, the executed option leg is decided SOLELY
    /// by the strategy's own filter side - a bullish-filtered strategy trades CE,
    /// a bearish-filtered one trades PE - and no other direction source (NIFTY
    /// straight-line lock, Top-Movers auto side, Run-Strategy-In override or the
    /// Overall Bullish/Bearish toggle) may flip it. A bullish filter that detects
    /// bearish simply fails its gate, so it never opens a PE trade (and vice
    /// versa). When OFF the normal multi-source routing applies.
    pub filter_side_route: bool,
    /// Strict straight-line gate for the Indicator-filters scanner. When ON, every
    /// scanner / NIFTY-trend leg must ALSO be confirmed by EVERY straight-line
    /// filter ticked on its OWN side, from both list sections: the "Straight Line
    /// Indicators" line-DIRECTION rows (`BullSl*` / `BearSl*`, Volume line `Vl`)
    /// and the "Straight Line Indicator Color Detection" rows (`BullSlColor*` /
    /// `BearSlColor*`, green on the CE side / red on the PE side). Any ticked line
    /// reading the other way blocks the entry outright - even when the ordinary
    /// gate (majority / AI Brain) would otherwise let the leg through. Normal-mode
    /// strategies and cases with no line filter ticked are unaffected.
    ///
    /// NIFTY Trend Following exception: while `nifty_trend_on` is set, the
    /// opposite-side block for the scanner universe is owned by the ASSIGNED NIFTY
    /// confirmation indicators (the `nifty_net_strict` / `nifty_all_agree_strict`
    /// gates), NOT by the manually ticked straight-line filters. This manual
    /// all-agree gate is therefore skipped for scanner legs while NIFTY trend is
    /// on, so a handful of ticked lines cannot stall every entry.
    pub strict_line_color: bool,
    /// "One trade per strike until the next fresh filter signal" for the
    /// Indicator-filters scanner. When ON, once a top gainer / top loser (or
    /// NIFTY-trend) leg has executed a trade on a strike, NO further entry is
    /// placed on that same strike until the leg's selected indicator-filter
    /// condition resets and meets again (a brand-new fresh signal). A gate that
    /// stays true, or a re-entry right after the position closes, is blocked.
    pub once_per_signal: bool,
    /// Spot forming-candle colour gate. When ON, a bullish leg (Top Gainer / CE
    /// side) may enter ONLY while the spot chart's live forming candle is GREEN,
    /// and a bearish leg (Top Loser / PE side) ONLY while it is RED. Applies when
    /// the run chart is the spot chart (strategy mode or Indicator-filters mode);
    /// premium-only / futures-only runs are unaffected. A doji (close == open) is
    /// neither green nor red, so the entry is held until a coloured candle forms.
    /// The colour is re-checked at the actual execution instant as well, so a
    /// candle that flips between the scan and the fill still blocks the trade.
    pub spot_candle_gate: bool,
    /// Multi-position mode A: when a position is already open, allow a new
    /// position on the next fresh signal (the entry gate turning from not-met to
    /// met). Concurrent positions are unlimited, so a signal that keeps
    /// re-triggering stacks entries instead of waiting for the old one to close.
    pub multi_fresh_on: bool,
    /// Multi-position mode B: while the entry gate keeps holding, open a new
    /// position on every scan cycle (no wait for a reset). Unlimited concurrent
    /// positions; the orders/sec cap still bounds the fill rate.
    pub multi_always_on: bool,
    pub brain_mode: String,
    pub brain_threshold: i64,
    /// Opposite-side filter veto mode:
    /// * `relative` (default) - veto the entry only when the OPPOSITE side's
    ///   enabled filters agree STRICTLY more than the trade side's own filters
    ///   (i.e. the opposite read is stronger). A tie, or the trade side leading,
    ///   never vetoes, so a symmetric two-sided filter set can no longer deadlock
    ///   into "no trade at all" on either side.
    /// * `off` - disable the opposite-side veto entirely; the own-side strict /
    ///   majority / Brain gate is the only entry rule.
    pub opposite_veto: String,
    /// Option side for index/underlying strategies: `both` | `CE` | `PE`.
    pub option_side: String,
    /// NIFTY ensemble trend timeframe: `1min` | `5min` | `15min` | `both`.
    pub nifty_tf: String,
    /// Indicator-filter toggles keyed by the old AST checkbox id suffix, e.g.
    /// `IncUp`, `CrossDown`, `BullEmaTrend9`, `BearMeetCloseVwap`, ... Each maps
    /// to a boolean so the engine can rebuild the entry gate set.
    pub filters: BTreeMap<String, bool>,
    // --- Straight Line Consensus indicator-filter settings. The filter rows
    // read these (default 0 = flip the instant the majority vote flips; no
    // confirm wait and no structural gate). Colours / line width are kept for
    // parity with the chart indicator's settings panel.
    pub sc_min_agree: f64,
    pub sc_confirm: f64,
    pub sc_strength: f64,
    pub sc_up_color: String,
    pub sc_down_color: String,
    pub sc_flat_color: String,
    pub sc_line_width: f64,
    // --- Support / Resistance Trendline indicator-filter settings. The filter
    // rows read these (Pivot strength / ATR period / Min tol % / Tol ATR mult /
    // Pivots to scan / Forward bars / Full span) so the fitted line the gate
    // reads matches the panel; colours / line width are kept for parity with the
    // chart indicator's settings panel.
    pub sup_strength: f64,
    pub sup_atr_period: f64,
    pub sup_min_pct: f64,
    pub sup_tol_mult: f64,
    pub sup_look: f64,
    pub sup_fwd: f64,
    pub sup_full_span: bool,
    pub sup_up_color: String,
    pub sup_down_color: String,
    pub sup_line_width: f64,
    pub res_strength: f64,
    pub res_atr_period: f64,
    pub res_min_pct: f64,
    pub res_tol_mult: f64,
    pub res_look: f64,
    pub res_fwd: f64,
    pub res_full_span: bool,
    pub res_up_color: String,
    pub res_down_color: String,
    pub res_line_width: f64,
    // --- AST template assignment (Top Movers / NIFTY Trend directions) ---
    /// Default bullish/bearish template chosen in the Template section.
    pub bull_template: String,
    pub bear_template: String,
    /// Direction-specific template assignments. When a scanner decides the
    /// direction, the assigned template's filters/brain override the manual set.
    pub mover_bull_template: String,
    pub mover_bear_template: String,
    pub nifty_bull_template: String,
    pub nifty_bear_template: String,
    // --- Strategy selection (old AST parity) ---
    /// Selection source flags: run the manually ticked strategies, and/or let
    /// the AI trader auto-pick the top-N scoring strategies per side.
    pub call_manual: bool,
    pub ai_pick: bool,
    pub ai_pick_n: i64,
    pub ai_pick_bull: bool,
    pub ai_pick_bear: bool,
    /// "Run Strategy In" global override: force CE/PE for every entry.
    pub run_in_enabled: bool,
    pub run_in_side: String,
    pub run_in_auto: bool,
    /// Paper-trading starting capital (virtual wallet). Realized paper P&L is
    /// added/subtracted from this to derive the available paper balance.
    pub paper_capital: f64,
    /// Paper fills: adverse slippage applied to every simulated fill, in basis
    /// points of the traded price (e.g. 5 = 0.05%). 0 disables slippage.
    pub paper_slippage_bps: f64,
    /// Paper fills: probability (0-100) that a simulated entry order is rejected
    /// instead of filled. 0 disables simulated rejections.
    pub paper_reject_pct: f64,
    /// Paper-only execution-latency simulation. When on, a qualifying entry is
    /// queued and placed `paper_exec_delay_ms` milliseconds after the entry
    /// condition first held, mimicking real order latency, instead of filling on
    /// the very next tick. Off = the entry fires inline exactly as before.
    pub paper_exec_delay_on: bool,
    /// Paper-only entry latency in milliseconds (see `paper_exec_delay_on`).
    pub paper_exec_delay_ms: f64,
    /// Paper-only exit-latency simulation. When on, a stop / trail-stop /
    /// target exit is not booked on the triggering tick; it waits
    /// `paper_exit_delay_ms` (mimicking a real Dhan round-trip) and then books
    /// at the live mark, so the slippage the delay causes shows in the paper
    /// P&L. Off = exits book inline exactly as before.
    pub paper_exit_delay_on: bool,
    /// Paper-only exit latency in milliseconds (see `paper_exit_delay_on`).
    pub paper_exit_delay_ms: f64,
    /// Paper-only: place the entry the way Dhan places an F&O order - as a
    /// LIMIT order on the aggressive side of the market (above the price for a
    /// buy, below for a sell) so it is always marketable and fills immediately,
    /// exactly like a market order. The limit is never placed behind/below the
    /// entry, and it is used only in the paper engine.
    pub fno_limit_order: bool,
    /// Deduct Dhan broker charges (STT / brokerage / NSE txn / SEBI / stamp /
    /// IPFT / GST) from paper P&L. Mirrors the old app's shared "Broker charges"
    /// toggle; when off the paper book is settled and shown gross of charges.
    pub broker_charges: bool,
    /// Auto-Lot: max percent of the traded contract's live traded volume a single
    /// entry may take. Independent from `auto_lot_oi_pct`.
    pub auto_lot_volume_pct: f64,
    /// Auto-Lot: max percent of the traded contract's live open interest a single
    /// entry may take. The engine buys `min(volume% lots, oi% lots, margin lots)`,
    /// so whichever of the two caps asks for the *fewer* lots wins.
    pub auto_lot_oi_pct: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            lot_size: 0.0,
            lots: 1.0,
            margin_pct: 100.0,
            margin_amount: 0.0,
            skip_on_margin_block: false,
            sl_auto: true,
            tf_1min: false,
            tf_5min: true,
            mtf: false,
            use_own_settings: false,
            ai_sl: false,
            ai_trail_tp: false,
            ai_tp_pct: false,
            manual_sl: false,
            manual_sl_pct: 0.0,
            manual_trail_sl: false,
            manual_trail_sl_pct: 0.0,
            manual_point_trail_sl: false,
            manual_point_trail_sl_points: 0.0,
            manual_trail_tp: false,
            manual_trail_tp_pct: 0.0,
            rr_enabled: false,
            rr_value: 2.0,
            trade_limit: false,
            trade_limit_count: 5,
            ai_trades: false,
            order_per_sec: 6,
            scan_interval_on: false,
            scan_interval_ms: 100,
            scan_bar_close: false,
            scan_bar_close_tf: "1min".into(),
            start_after_enabled: false,
            start_after: "09:15".into(),
            no_trade_after_enabled: false,
            no_trade_after: "15:30".into(),
            auto_square_off_enabled: false,
            auto_square_off_time: "15:20".into(),
            trade_sessions: Vec::new(),
            option_type: "ATM".into(),
            strike_mode: "both_atm".into(),
            strike_count: 3,
            only_positive: true,
            fallback_opposite_side: true,
            fastest_rising: false,
            fastest_count: 3,
            manual_strikes_enabled: false,
            manual_strikes: Vec::new(),
            premium_only: false,
            run_index: "both".into(),
            run_fno: "spot".into(),
            run_comm: "spot".into(),
            data_pool: false,
            trade_in_index: "premium".into(),
            trade_in_fno: "premium".into(),
            trade_in_comm: "premium".into(),
            trade_in_default: false,
            run_in_default: false,
            movers_on: false,
            movers_gainers: 5,
            movers_losers: 5,
            movers_indices: Vec::new(),
            movers_index_legs: Vec::new(),
            nifty_trend_on: false,
            nifty_trend_pct_on: true,
            nifty_trend_pct: 2.5,
            nifty_trend_conf_inds: Vec::new(),
            nifty_trend_slope_len: 5,
            nifty_trend_color: false,
            nifty_net_strict: false,
            nifty_all_agree_strict: false,
            commodity_on: false,
            commodity_list: Vec::new(),
            commodity_legs: Vec::new(),
            scanner_exclude: Vec::new(),
            filter_mode: false,
            all_in_one: false,
            dir_guard: false,
            overall_dir: true,
            filter_side_route: false,
            strict_line_color: false,
            once_per_signal: false,
            spot_candle_gate: false,
            multi_fresh_on: true,
            multi_always_on: false,
            brain_mode: "off".into(),
            brain_threshold: 65,
            opposite_veto: "relative".into(),
            option_side: "both".into(),
            nifty_tf: "5min".into(),
            filters: BTreeMap::new(),
            sc_min_agree: 0.0,
            sc_confirm: 0.0,
            sc_strength: 0.0,
            sc_up_color: "#00e676".into(),
            sc_down_color: "#ff5252".into(),
            sc_flat_color: "#6b6b88".into(),
            sc_line_width: 2.0,
            sup_strength: 5.0,
            sup_atr_period: 14.0,
            sup_min_pct: 0.05,
            sup_tol_mult: 0.5,
            sup_look: 12.0,
            sup_fwd: 10.0,
            sup_full_span: false,
            sup_up_color: "#26a69a".into(),
            sup_down_color: "#ef5350".into(),
            sup_line_width: 2.0,
            res_strength: 5.0,
            res_atr_period: 14.0,
            res_min_pct: 0.05,
            res_tol_mult: 0.5,
            res_look: 12.0,
            res_fwd: 10.0,
            res_full_span: false,
            res_up_color: "#26a69a".into(),
            res_down_color: "#ef5350".into(),
            res_line_width: 2.0,
            bull_template: String::new(),
            bear_template: String::new(),
            mover_bull_template: String::new(),
            mover_bear_template: String::new(),
            nifty_bull_template: String::new(),
            nifty_bear_template: String::new(),
            call_manual: true,
            ai_pick: false,
            ai_pick_n: 5,
            ai_pick_bull: true,
            ai_pick_bear: true,
            run_in_enabled: false,
            run_in_side: "CE".into(),
            run_in_auto: false,
            paper_capital: 200_000.0,
            paper_slippage_bps: 5.0,
            paper_reject_pct: 0.0,
            paper_exec_delay_on: false,
            paper_exec_delay_ms: 500.0,
            paper_exit_delay_on: false,
            paper_exit_delay_ms: 600.0,
            fno_limit_order: false,
            broker_charges: true,
            auto_lot_volume_pct: 1.0,
            auto_lot_oi_pct: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Strategies + conditions
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Condition {
    pub id: String,
    pub indicator: String,
    pub source: String,
    pub op: String,
    pub value: f64,
    #[serde(default)]
    pub settings: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Strategy {
    pub id: String,
    pub name: String,
    pub category: String,
    pub timeframe: String,
    pub security_id: i64,
    pub exchange_segment: String,
    pub instrument: String,
    pub trading_symbol: String,
    pub side: String,
    pub enabled: bool,
    pub conditions: Vec<Condition>,
    /// Last time (ms) an entry signal fired; debounces re-entry.
    pub last_signal: i64,
    pub last_error: String,
    /// Scanner-generated (Top Movers / MCX commodity) instrument.
    /// Synthetic rows carry no conditions - the indicator-filter gate is the
    /// entry rule - so the engine does not require a saved strategy for them.
    #[serde(default)]
    pub synthetic: bool,
    /// Authoritative contract lot size for a synthetic scanner instrument (0 =
    /// resolve from the scrip master as usual).
    #[serde(default)]
    pub lot: f64,
    /// Research group this strategy belongs to (old AST `groupOf(method)`):
    /// "candlestick" | "elliott" | "indicator" | "pane" | "symmetry" |
    /// "structure" | "atr" | "other". Drives the "Research stream" scoping
    /// checkboxes: when a side has stream groups ticked, only its strategies
    /// whose group is in that list run ("other"/empty always runs).
    #[serde(default)]
    pub group: String,
    /// Manual one-off order placed from an Order Placement card. Manual entries
    /// derive the option leg from the order side when the chart is an index
    /// (BUY = CE, SELL = PE) instead of reading the global Option Type setting.
    #[serde(default)]
    pub manual: bool,
}

// ---------------------------------------------------------------------------
// Persisted document
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RtDoc {
    pub settings: Settings,
    pub method: String,
    pub order_cfg: HashMap<String, Value>,
    pub strategies: Vec<Strategy>,
    pub positions: Vec<Value>,
    pub closed: Vec<Value>,
    pub armed: bool,
    pub engine_on: bool,
    pub auto_lots: bool,
    pub logs: Vec<Value>,
    pub last_square_off_day: String,
    /// Saved AST templates: name -> snapshot `{ side, savedAt, settings }`.
    pub templates: BTreeMap<String, Value>,
    /// Per-strategy selection map (old AST `state.selected`); the engine runs the
    /// ticked strategies in addition to the AI-picked ones.
    pub selected: BTreeMap<String, bool>,
    /// Strategy staging list: Auto Experiment / saved strategies waiting to be
    /// sent to (or already sent to) the engine.
    pub staging: Vec<Value>,
    pub staging_auto: bool,
    /// Entry-timing diagnostics: one row per filter-alignment episode, newest first.
    pub entry_timing: Vec<Value>,
    /// Best-performing strategies auto-saved for one-click re-run.
    pub final_strategies: Vec<Value>,
    /// Keys the operator removed from the Final list (never re-added by the scan).
    pub final_excluded: BTreeMap<String, bool>,
}

impl Default for RtDoc {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            method: "normal".into(),
            order_cfg: HashMap::new(),
            strategies: Vec::new(),
            positions: Vec::new(),
            closed: Vec::new(),
            armed: false,
            engine_on: false,
            auto_lots: false,
            logs: Vec::new(),
            last_square_off_day: String::new(),
            templates: BTreeMap::new(),
            selected: BTreeMap::new(),
            staging: Vec::new(),
            staging_auto: false,
            entry_timing: Vec::new(),
            final_strategies: Vec::new(),
            final_excluded: BTreeMap::new(),
        }
    }
}

/// Dhan-style broker charge simulation, ported 1:1 from the Python paper-trade
/// engine (`static/papertrade.js`). All rates are Dhan's published retail
/// tariffs (dhan.co/pricing):
///   Delivery: brokerage Rs 0, STT 0.1% (buy+sell), NSE txn 0.0030699%,
///             SEBI 0.0001%, stamp 0.015% (buy), IPFT 0.0000001%, GST 18%
///   Intraday: brokerage Rs 20 or 0.03% (lower of the two), STT 0.025% (sell),
///             NSE txn 0.0030699%, SEBI 0.0001%, stamp 0.003% (buy),
///             IPFT 0.0000001%, GST 18%
///   Options:  brokerage Rs 20 / executed order, STT 0.0625% of premium (sell),
///             NSE txn 0.03503% of premium, SEBI 0.0001%, stamp 0.003% (buy),
///             IPFT 0.0000001%, GST 18%
///   Futures:  brokerage Rs 20 / executed order, STT 0.02% of turnover (sell),
///             NSE txn 0.00173% of turnover, SEBI 0.0001%, stamp 0.003% (buy),
///             IPFT 0.0000001%, GST 18%
/// Rounding follows the Dhan contract-note rule: STT + stamp duty to the nearest
/// rupee, every other component to 2 dp. Mirrored by the frontend `legCharges()`
/// so the paper wallet, Closed Trades table and Paper Stats all agree.
#[derive(Clone, Copy)]
struct ChargeRates {
    brokerage_flat: f64,
    brokerage_pct: f64,
    txn_pct: f64,
    stt_buy_pct: f64,
    stt_sell_pct: f64,
    sebi_pct: f64,
    stamp_buy_pct: f64,
    stamp_sell_pct: f64,
    gst_pct: f64,
    ipft_pct: f64,
}

const CHARGES_DELIVERY: ChargeRates = ChargeRates {
    brokerage_flat: 0.0,
    brokerage_pct: 0.0,
    txn_pct: 0.0030699,
    stt_buy_pct: 0.1,
    stt_sell_pct: 0.1,
    sebi_pct: 0.0001,
    stamp_buy_pct: 0.015,
    stamp_sell_pct: 0.0,
    gst_pct: 18.0,
    ipft_pct: 0.0000001,
};
const CHARGES_INTRADAY: ChargeRates = ChargeRates {
    brokerage_flat: 20.0,
    brokerage_pct: 0.03,
    txn_pct: 0.0030699,
    stt_buy_pct: 0.0,
    stt_sell_pct: 0.025,
    sebi_pct: 0.0001,
    stamp_buy_pct: 0.003,
    stamp_sell_pct: 0.0,
    gst_pct: 18.0,
    ipft_pct: 0.0000001,
};
const CHARGES_OPTIONS: ChargeRates = ChargeRates {
    brokerage_flat: 20.0,
    brokerage_pct: 0.0,
    txn_pct: 0.03503,
    stt_buy_pct: 0.0,
    stt_sell_pct: 0.0625,
    sebi_pct: 0.0001,
    stamp_buy_pct: 0.003,
    stamp_sell_pct: 0.0,
    gst_pct: 18.0,
    ipft_pct: 0.0000001,
};
const CHARGES_FUTURES: ChargeRates = ChargeRates {
    brokerage_flat: 20.0,
    brokerage_pct: 0.0,
    txn_pct: 0.00173,
    stt_buy_pct: 0.0,
    stt_sell_pct: 0.02,
    sebi_pct: 0.0001,
    stamp_buy_pct: 0.003,
    stamp_sell_pct: 0.0,
    gst_pct: 18.0,
    ipft_pct: 0.0000001,
};

/// Charge segment for a position / symbol: OPT* -> options, FUT* -> futures,
/// otherwise delivery (equity / index spot). Mirrors Python `segmentFor()`.
fn charges_segment(instrument: &str, trading_symbol: &str) -> &'static str {
    let inst = instrument.trim().to_ascii_uppercase();
    if matches!(inst.as_str(), "OPTIDX" | "OPTSTK" | "OPTFUT" | "OPT") {
        return "options";
    }
    if matches!(inst.as_str(), "FUTIDX" | "FUTSTK" | "FUTCOM" | "FUT") {
        return "futures";
    }
    let nm = trading_symbol.trim().to_ascii_uppercase();
    let has_ce_pe = nm
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == "CE" || w == "PE");
    if has_ce_pe {
        return "options";
    }
    "delivery"
}

fn charges_rates(segment: &str) -> ChargeRates {
    match segment {
        "options" => CHARGES_OPTIONS,
        "futures" => CHARGES_FUTURES,
        "intraday" => CHARGES_INTRADAY,
        _ => CHARGES_DELIVERY,
    }
}

/// Per-component charge line for one order side (turnover = qty x price).
#[derive(Clone, Copy, Default)]
struct SideCharges {
    brokerage: f64,
    txn: f64,
    stt: f64,
    sebi: f64,
    stamp: f64,
    ipft: f64,
    gst: f64,
    total: f64,
}

impl SideCharges {
    fn to_json(&self) -> serde_json::Value {
        json!({
            "brokerage": self.brokerage,
            "txn": self.txn,
            "stt": self.stt,
            "sebi": self.sebi,
            "stamp": self.stamp,
            "ipft": self.ipft,
            "gst": self.gst,
            "total": self.total,
        })
    }
}

/// One side's charges for `segment`. STT + stamp round to the nearest rupee,
/// every other component to 2 dp (Dhan contract-note rule).
fn side_charges(segment: &str, is_buy: bool, turnover: f64) -> SideCharges {
    let c = charges_rates(segment);
    let t = turnover.abs();
    if t <= 0.0 {
        return SideCharges::default();
    }
    let mut brokerage = c.brokerage_flat;
    if c.brokerage_pct > 0.0 {
        let pct = (t * c.brokerage_pct) / 100.0;
        brokerage = if c.brokerage_flat > 0.0 {
            c.brokerage_flat.min(pct)
        } else {
            pct
        };
    }
    let txn = t * c.txn_pct / 100.0;
    let stt = ((if is_buy { c.stt_buy_pct } else { c.stt_sell_pct }) * t / 100.0).round();
    let sebi = t * c.sebi_pct / 100.0;
    let stamp = ((if is_buy { c.stamp_buy_pct } else { c.stamp_sell_pct }) * t / 100.0).round();
    let ipft = t * c.ipft_pct / 100.0;
    let gst = (brokerage + txn + sebi + ipft) * c.gst_pct / 100.0;
    SideCharges {
        brokerage: round2(brokerage),
        txn: round2(txn),
        stt,
        sebi: round2(sebi),
        stamp,
        ipft,
        gst: round2(gst),
        total: round2(brokerage + txn + stt + sebi + stamp + ipft + gst),
    }
}

/// Full round-trip charge estimate for a position closed at `exit`. Mirrors
/// Python `computeChargesForTrade()`; the entry leg used `pos.side` and the exit
/// leg the opposite side, so STT lands on the sell leg and stamp on the buy leg.
#[allow(dead_code)]
struct TradeCharges {
    segment: &'static str,
    entry: SideCharges,
    exit: SideCharges,
    total: f64,
    gross: f64,
    net: f64,
}

fn compute_charges_for_trade(
    entry: f64,
    exit: f64,
    qty: f64,
    side: &str,
    instrument: &str,
    trading_symbol: &str,
    gross: f64,
) -> Option<TradeCharges> {
    let q = qty.abs();
    if q <= 0.0 || entry.abs() <= 0.0 || exit.abs() <= 0.0 {
        return None;
    }
    let segment = charges_segment(instrument, trading_symbol);
    let is_long = !side.eq_ignore_ascii_case("SELL");
    let entry_c = side_charges(segment, is_long, entry.abs() * q);
    let exit_c = side_charges(segment, !is_long, exit.abs() * q);
    let total = round2(entry_c.total + exit_c.total);
    Some(TradeCharges {
        segment,
        entry: entry_c,
        exit: exit_c,
        total,
        gross: round2(gross),
        net: round2(gross - total),
    })
}

/// Deterministic-per-seed PRNG (xorshift64*) for simulated paper outcomes, so a
/// rejection decision needs no external RNG dependency.
fn paper_rand01(seed: u64) -> f64 {
    let mut x = seed.wrapping_mul(0x2545F4914F6CDD1D).wrapping_add(0x9E3779B97F4A7C15);
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let v = x.wrapping_mul(0x2545F4914F6CDD1D);
    (v >> 11) as f64 / (1u64 << 53) as f64
}

fn paper_seed(extra: i64) -> u64 {
    (now_ms() as u64) ^ (extra as u64).rotate_left(17)
}

/// Apply adverse slippage to a simulated fill: a BUY fills above the mark, a
/// SELL fills below it, by `bps` basis points of the mark.
fn paper_fill_price(mark: f64, is_buy: bool, bps: f64) -> f64 {
    let slip = (mark * bps / 10_000.0).abs();
    if is_buy {
        mark + slip
    } else {
        (mark - slip).max(0.05)
    }
}

/// Paper-only F&O limit price for a BUY: a marketable price placed above the
/// market (never below it) so the order is certain to fill, exactly like a Dhan
/// market order. Sized generously enough (0.5% or one tick, whichever is
/// larger) that it is always marketable.
fn fno_limit_buy_price(ltp: f64) -> f64 {
    let protect = (ltp * 0.005).max(0.05);
    round2(ltp + protect)
}

/// Realized P&L of a closed trade: the stored net (after charges) when the
/// "Deduct Dhan charges" toggle is on, else the gross `pnl`. Real Dhan trades
/// settle charges broker-side (net == gross) and never turn the toggle on.
fn closed_pnl(c: &Value, charges_on: bool) -> f64 {
    if charges_on && c.get("netPnl").is_some() {
        jf(c, "netPnl")
    } else {
        jf(c, "pnl")
    }
}

/// Sum of realized P&L across closed trades.
fn realized_pnl(closed: &[Value], charges_on: bool) -> f64 {
    closed.iter().map(|c| closed_pnl(c, charges_on)).sum()
}

/// Smart P&L summary strip (old app `renderSummary`): "Smart Live P&L (gross)" is
/// `realized + unrealized`, "Smart Realized P&L" is the closed book's net,
/// "Smart Win Rate" and "Smart Trades (W/L)" come from the same closed trades
/// (L = total - wins, exactly like the old app) and "Smart Charges" sums every
/// banked charge. Computed from the FULL closed ledger, not the snapshot page,
/// so the strip never shrinks as trades age out.
fn smart_stats(closed: &[Value], positions: &[Value], charges_on: bool) -> Value {
    let realized = realized_pnl(closed, charges_on);
    // Live P&L is gross: running trades never deduct broker charges, which are
    // applied once at close and surface in the realized total.
    let unrealized: f64 = positions.iter().map(|p| jf(p, "pnl")).sum();
    let wins = closed.iter().filter(|c| closed_pnl(c, charges_on) > 0.0).count();
    let losses = closed.iter().filter(|c| closed_pnl(c, charges_on) < 0.0).count();
    let charges_total: f64 = closed.iter().map(|c| jf(c, "charges")).sum();
    let win_rate = if closed.is_empty() { 0.0 } else { wins as f64 / closed.len() as f64 * 100.0 };
    // Normalize negative zero so an empty book serializes as `0`, not `-0`.
    let z = |x: f64| if x == 0.0 { 0.0 } else { round2(x) };
    json!({
        "realized": z(realized),
        "unrealized": z(unrealized),
        "net": z(realized + unrealized),
        "wins": wins,
        "losses": losses,
        "total": closed.len(),
        "running": positions.len(),
        "charges": z(charges_total),
        "chargesOn": charges_on,
        "winRate": z(win_rate),
    })
}

/// Cost locked by open positions (qty x entry, falling back to fill/ltp), plus
/// the position count. Lock-free so callers already holding the doc lock can use
/// it without re-entering the (non-reentrant) mutex.
fn locked_margin_of(positions: &[Value]) -> (f64, i64) {
    let locked = positions
        .iter()
        .map(|p| {
            let qty = jf(p, "qty").abs();
            let entry = {
                let e = jf(p, "entry");
                if e > 0.0 {
                    e
                } else {
                    let f = jf(p, "fillPrice");
                    if f > 0.0 {
                        f
                    } else {
                        jf(p, "ltp")
                    }
                }
            };
            qty * entry
        })
        .sum();
    (locked, positions.len() as i64)
}

/// Margin budget: a manual rupee amount wins when set (`> 0`), capped by the live
/// available balance; otherwise the operator-set percentage of the balance is
/// used. `0%` / `0` means no budget configured (the margin gate is disabled).
fn margin_budget_of(margin_amount: f64, margin_pct: f64, available: f64) -> f64 {
    let balance = available.max(0.0);
    if margin_amount > 0.0 {
        balance.min(margin_amount)
    } else {
        balance * margin_pct.clamp(0.0, 100.0) / 100.0
    }
}

/// Pure margin gate: the block reason when a leg's required margin exceeds the
/// budget still free after the running trades, else `None`. In-memory arithmetic
/// only (microseconds), so it can be called both as an early pre-check and at the
/// actual fill without changing behaviour.
fn margin_block_reason(required: f64, budget: f64, locked: f64, running: i64) -> Option<String> {
    let available = (budget - locked).max(0.0);
    if required > available {
        Some(format!(
            "trade blocked: required margin {} > available {} (Margin {} - locked {} by {} running trade{})",
            round2(required),
            round2(available),
            round2(budget),
            round2(locked),
            running,
            if running == 1 { "" } else { "s" }
        ))
    } else {
        None
    }
}

/// Paper wallet balance BEFORE the running trades' margin is committed:
/// starting capital + realized P&L. The margin budget is sized against this so
/// the locked margin of open positions is subtracted exactly once (by the entry
/// gate / margin bar) instead of twice.
fn paper_wallet_of(cap: f64, closed: &[Value], charges_on: bool) -> f64 {
    let cap = if cap > 0.0 { cap } else { 200_000.0 };
    (cap + realized_pnl(closed, charges_on)).max(0.0)
}

/// Paper wallet free cash: balance - margin locked by the open paper positions.
/// Derived (never stored) so it can never drift. This is the amount shown as the
/// account's available balance; the margin budget is sized against
/// [`paper_wallet_of`] instead, so the lock is never counted twice.
fn paper_available_of(cap: f64, closed: &[Value], positions: &[Value], charges_on: bool) -> f64 {
    let (locked, _) = locked_margin_of(positions);
    (paper_wallet_of(cap, closed, charges_on) - locked).max(0.0)
}

/// Durable state file names, one per independent engine. The real engine's
/// `ALGODHAN_STATE_PATH` override never applies to a paper engine, and each
/// paper engine keeps its own file so their books can never share state.
const STATE_FILE_REAL: &str = "algodhan_realtime_state.json";
const STATE_FILE_PAPER: &str = "algodhan_paper_state.json";
const STATE_FILE_PAPER2: &str = "algodhan_paper2_state.json";

fn state_path(name: &str) -> PathBuf {
    // Durable location first: an explicit override, else a `data/` directory in
    // the process working dir. /tmp is only a last resort (wiped on reboot), so
    // the saved strategies/settings survive a restart.
    if name == STATE_FILE_REAL {
        if let Ok(p) = std::env::var("ALGODHAN_STATE_PATH") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
    }
    let dir = std::env::var("ALGODHAN_DATA_DIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));
    if std::fs::create_dir_all(&dir).is_ok() {
        return dir.join(name);
    }
    std::env::temp_dir().join(name)
}

/// Atomic JSON write: serialize to a sibling temp file then rename over the
/// target, so a crash mid-write cannot leave a truncated state file.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Broker-side account cache
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RealtimeState {
    pub dhan: DhanState,
    /// Paper-trading mode: the exact same engine/signals/risk logic, but every
    /// execution touchpoint (entry fill, exit fill, broker reconciliation,
    /// account/margin wallet) is simulated locally instead of sent to Dhan.
    pub paper: bool,
    /// Durable state-file name for THIS engine (see the `STATE_FILE_*` consts).
    /// Stored so `save()` persists each independent engine to its own file.
    state_file: &'static str,
    doc: Arc<Mutex<RtDoc>>,
    ltp: Arc<Mutex<HashMap<i64, f64>>>,
    broker_positions: Arc<Mutex<Vec<Value>>>,
    broker_holdings: Arc<Mutex<Vec<Value>>>,
    funds: Arc<Mutex<Value>>,
    last_acct: Arc<AtomicI64>,
    last_funds: Arc<AtomicI64>,
    last_ltp: Arc<AtomicI64>,
    /// Last broker fill-reconciliation poll (ms), so the order book is not
    /// fetched every 100ms loop tick.
    last_fills: Arc<AtomicI64>,
    /// Last post-reconnect broker resync (ms), so a websocket that flaps does
    /// not re-issue the order-book / positions fetch on every blip.
    last_reconnect: Arc<AtomicI64>,
    /// Previous Dhan connection / feed-up flag, used to fire `reconcile_reconnect`
    /// on the false -> true edge (a fresh Connect, or a feed that came back).
    was_conn: Arc<AtomicBool>,
    was_feed: Arc<AtomicBool>,
    last_sig: Arc<Mutex<HashMap<String, i64>>>,
    /// Entry-gate edge detector for the multi-position "fresh signal" mode:
    /// strategy id -> whether the entry gate held on the previous scan. A new
    /// position is allowed only on the false -> true edge, so a gate that stays
    /// true opens exactly one position instead of one every scan cycle.
    sig_state: Arc<Mutex<HashMap<String, bool>>>,
    /// "One trade per strike until the next fresh filter signal": strategy id ->
    /// the option contracts already traded during the CURRENT signal pulse. A
    /// contract is cleared the moment the leg's gate stops holding (the pulse
    /// ends), so the next fresh signal may trade it again.
    pulse_traded: Arc<Mutex<HashMap<String, std::collections::HashSet<i64>>>>,
    /// Signal-end debounce: strategy id -> millisecond timestamp when its leg's
    /// gate was FIRST seen failing on the current run of failures. A single-scan
    /// flicker must not end the signal, so the pulse is only reset once the gate
    /// has stayed false continuously for `SIGNAL_END_DEBOUNCE_MS`. The entry
    /// clears the marker, so a brief dip inside one signal keeps it running.
    sig_fail_since: Arc<Mutex<HashMap<String, i64>>>,
    /// Cached ATR per `security:segment:instrument:timeframe` `(at_ms, atr)`.
    /// AI SL/TP/trail reuse a recent ATR instead of paying a throttled Dhan
    /// candle round-trip on every entry (that fetch was a 3-10s entry lag).
    atr_cache: Arc<Mutex<HashMap<String, (i64, f64)>>>,
    /// Cached Live Data Pool readout `(at_ms, payload)`; short TTL so the 2s UI
    /// poll never hammers Dhan and starves the chart's candle fetches.
    pool_cache: Arc<Mutex<(i64, Value)>>,
    /// Guards the background Data Pool refresh so the endpoint never blocks the
    /// caller on the ~12 throttled Dhan candle fetches a compute needs.
    pool_busy: Arc<AtomicBool>,
    /// Latest movers bias: 0 balanced/unknown, +1 gainers lead, -1 losers lead.
    mover_bias: Arc<AtomicI64>,
    /// Last auto-computed Run-in side (0 unknown, +1 CE, -1 PE). When the live
    /// scanners flip it, the picked strikes / strategy legs are re-resolved on
    /// the same pass instead of waiting for the next scheduled scan.
    last_auto_side: Arc<AtomicI64>,
    last_movers: Arc<AtomicI64>,
    /// Cached movers rows for the UI `(at_ms, payload)`.
    movers_cache: Arc<Mutex<(i64, Value)>>,
    /// NIFTY trend-following direction derived from the selected straight-line
    /// indicators: 0 neutral, +1 bullish (trade Top Gainers), -1 bearish (trade
    /// Top Losers).
    nifty_dir: Arc<AtomicI64>,
    /// Straight-line indicators currently reading bullish. A bullish line is
    /// assigned to the Top Gainer side, so only it gates the gainer legs.
    nifty_bull_filters: Arc<Mutex<Vec<String>>>,
    /// Straight-line indicators currently reading bearish - assigned to the Top
    /// Loser side.
    nifty_bear_filters: Arc<Mutex<Vec<String>>>,
    /// Net direction of the COMMITTED straight-line read (0 = the committed side
    /// read a tie / none). Refreshed from each confirmed read rather than the raw
    /// per-tick flicker, so the strict net-direction gate blocks on a real change
    /// of side - not on a millisecond wiggle of the live forming bar.
    nifty_raw_net: Arc<AtomicI64>,
    /// All-lines-agree state of the assigned straight-line confirmation
    /// indicators: +1 when every line reads bullish (bull set non-empty, bear set
    /// empty), -1 when every line reads bearish, 0 when the read is mixed / tied
    /// / unavailable. Drives the strict all-lines-agree gate.
    nifty_agree: Arc<AtomicI64>,
    last_trend: Arc<AtomicI64>,
    last_nifty_scan: Arc<AtomicI64>,
    /// Cached NIFTY-trend pick rows `(at_ms, payload)` for the UI + pick tagging.
    nifty_picks: Arc<Mutex<(i64, Value)>>,
    /// Last NIFTY trend-change signal: `{from,to,at,seq}` where `from`/`to` are
    /// 0 / +1 / -1. Bumped the instant the assigned straight-line indicators flip,
    /// so running strategies can switch leg + strikes without waiting for a bar.
    nifty_flip: Arc<Mutex<Value>>,
    /// Monotonic counter of NIFTY trend flips (never resets while the engine runs).
    nifty_seq: Arc<AtomicI64>,
    /// Throttle for the tick-native NIFTY flip watchdog (millisecond clock).
    last_nifty_flip: Arc<AtomicI64>,
    /// Throttle for the colour-based NIFTY confirmation scan. When colour mode is
    /// ON the assigned straight-line indicators' colour is re-read on this fixed
    /// cadence (see `COLOR_SCAN_MS`) instead of on every tick.
    last_color_scan: Arc<AtomicI64>,
    /// Candidate next NIFTY direction awaiting confirmation (0 = none pending).
    /// Set the first time a read disagrees with the committed side; only becomes
    /// the committed side once it has persisted `NIFTY_FLIP_CONFIRM_MS`.
    nifty_pending_dir: Arc<AtomicI64>,
    /// Millisecond clock when `nifty_pending_dir` was first read.
    nifty_pending_since: Arc<AtomicI64>,
    /// Order timestamps (ms) in the last second, powering the orders/sec cap.
    order_times: Arc<Mutex<Vec<i64>>>,
    /// Paper-only simulated entry latency: strategy id -> due ms for entries
    /// currently queued by `queue_delayed_entry`. The map doubles as the
    /// in-flight guard so the 100ms scanner never queues the same strategy
    /// twice while its delayed order is still pending.
    paper_pending: Arc<Mutex<HashMap<String, i64>>>,
    /// Paper-only simulated exit latency: position id -> due ms for exits
    /// currently queued by `queue_delayed_exit`. Doubles as the in-flight guard
    /// so the position manager cannot queue the same stop exit twice while its
    /// delayed order is still pending.
    paper_exit_pending: Arc<Mutex<HashMap<String, i64>>>,
    /// Cached broker margin-calculator responses `(at_ms, price, Option<resp>)`.
    /// Dhan's margin API is throttled to ~1 call/second, so without this every
    /// option-chain / instrument switch paid that round-trip and the chart load
    /// queued behind it. The requirement moves slowly, so a short TTL is safe;
    /// `None` is cached too so a rejected contract (e.g. an index) does not
    /// re-throttle on every switch.
    margin_cache: Arc<Mutex<HashMap<String, (i64, f64, Option<MarginResponse>)>>>,
    /// Cached scanner/strategy option legs `(at_ms, rows)` powering the "Picked
    /// Strikes" readout - every resolved CE/PE contract the engine will execute.
    picked_strikes: Arc<Mutex<(i64, Vec<Value>)>>,
    /// Last option-premium leg the engine resolved per running strategy, keyed
    /// `strategyId -> { run, trade }`. Powers the Running Strategies view so each
    /// row can name (and open) the exact chart it evaluates the entry on and
    /// executes the order on - spot strategies fall back to the strategy's own
    /// instrument, so only the resolved premium contracts need caching here.
    strat_legs: Arc<Mutex<HashMap<String, Value>>>,
    /// Entry-timing diagnostics: strategy id -> `(metAt_ms, orders_at_meet)` for
    /// the currently-armed filter-alignment episode.
    et_pending: Arc<Mutex<HashMap<String, (i64, i64)>>>,
    /// Whole-engine counter of AST orders placed, used for the entry-timing
    /// "orders placed in this window" column.
    et_placed: Arc<AtomicI64>,
    /// Throttle map for repeated skip/diagnostic log lines (key -> last ms).
    log_throttle: Arc<Mutex<HashMap<String, i64>>>,
    /// Set by `POST /api/rt/tick` ("Run / Tick Now"): the engine loop consumes
    /// it to perform one immediate evaluation pass instead of waiting for the
    /// next scheduled tick.
    force_tick: Arc<AtomicBool>,
}

impl RealtimeState {
    pub fn new(dhan: DhanState) -> Self {
        Self::new_mode(dhan, false, STATE_FILE_REAL)
    }

    /// Paper-trade engine: identical logic, simulated execution + wallet, and a
    /// completely separate durable state file so paper strategies/settings never
    /// touch the real engine's book.
    pub fn new_paper(dhan: DhanState) -> Self {
        Self::new_mode(dhan, true, STATE_FILE_PAPER)
    }

    /// Second, fully independent paper-trade engine (`/api/paper2/*`): the same
    /// logic and simulated execution as `new_paper`, but its own durable state
    /// file and in-memory book, so it shares NO data with the first paper
    /// engine.
    pub fn new_paper2(dhan: DhanState) -> Self {
        Self::new_mode(dhan, true, STATE_FILE_PAPER2)
    }

    fn new_mode(dhan: DhanState, paper: bool, state_file: &'static str) -> Self {
        let path = state_path(state_file);
        // Read the durable file, else migrate a legacy /tmp state file if the
        // durable one does not exist yet.
        let mut doc = std::fs::read_to_string(&path)
            .ok()
            .or_else(|| {
                // Legacy /tmp migration applies to the real engine only; the
                // paper book must never inherit the real one.
                if paper {
                    return None;
                }
                let legacy = std::env::temp_dir().join("algodhan_realtime_state.json");
                if legacy != path {
                    std::fs::read_to_string(legacy).ok()
                } else {
                    None
                }
            })
            .and_then(|s| serde_json::from_str::<RtDoc>(&s).ok())
            .unwrap_or_default();
        // Old-app migration: a missing/non-positive "Max trades" count defaults
        // to 5 so ticking the box never silently means "no cap".
        if doc.settings.trade_limit_count <= 0 {
            doc.settings.trade_limit_count = 5;
        }
        // Orders/sec: 0 (or nonsense) means "unset" -> default 6; anything above
        // the Dhan ceiling is clamped to 30.
        if doc.settings.order_per_sec <= 0 {
            doc.settings.order_per_sec = 6;
        }
        doc.settings.order_per_sec = order_per_sec_budget(doc.settings.order_per_sec);
        // Engine Scan interval: keep it inside the floor/ceiling so a saved 0 or a
        // nonsense value can never make the engine spin or misbehave.
        doc.settings.scan_interval_ms = scan_interval_budget(doc.settings.scan_interval_ms);
        doc.settings.scan_bar_close_tf = scan_bar_close_tf(&doc.settings.scan_bar_close_tf).to_string();
        let st = Self {
            dhan,
            paper,
            state_file,
            doc: Arc::new(Mutex::new(doc)),
            ltp: Arc::new(Mutex::new(HashMap::new())),
            broker_positions: Arc::new(Mutex::new(Vec::new())),
            broker_holdings: Arc::new(Mutex::new(Vec::new())),
            funds: Arc::new(Mutex::new(json!({}))),
            last_acct: Arc::new(AtomicI64::new(0)),
            last_funds: Arc::new(AtomicI64::new(0)),
            last_ltp: Arc::new(AtomicI64::new(0)),
            last_fills: Arc::new(AtomicI64::new(0)),
            last_reconnect: Arc::new(AtomicI64::new(0)),
            was_conn: Arc::new(AtomicBool::new(false)),
            was_feed: Arc::new(AtomicBool::new(false)),
            last_sig: Arc::new(Mutex::new(HashMap::new())),
            sig_state: Arc::new(Mutex::new(HashMap::new())),
            pulse_traded: Arc::new(Mutex::new(HashMap::new())),
            sig_fail_since: Arc::new(Mutex::new(HashMap::new())),
            atr_cache: Arc::new(Mutex::new(HashMap::new())),
            pool_cache: Arc::new(Mutex::new((0, Value::Null))),
            pool_busy: Arc::new(AtomicBool::new(false)),
            mover_bias: Arc::new(AtomicI64::new(0)),
            last_auto_side: Arc::new(AtomicI64::new(0)),
            last_movers: Arc::new(AtomicI64::new(0)),
            movers_cache: Arc::new(Mutex::new((0, Value::Null))),
            nifty_dir: Arc::new(AtomicI64::new(0)),
            nifty_bull_filters: Arc::new(Mutex::new(Vec::new())),
            nifty_bear_filters: Arc::new(Mutex::new(Vec::new())),
            nifty_raw_net: Arc::new(AtomicI64::new(0)),
            nifty_agree: Arc::new(AtomicI64::new(0)),
            last_trend: Arc::new(AtomicI64::new(0)),
            last_nifty_scan: Arc::new(AtomicI64::new(0)),
            nifty_picks: Arc::new(Mutex::new((0, Value::Null))),
            nifty_flip: Arc::new(Mutex::new(Value::Null)),
            nifty_seq: Arc::new(AtomicI64::new(0)),
            last_nifty_flip: Arc::new(AtomicI64::new(0)),
            last_color_scan: Arc::new(AtomicI64::new(0)),
            nifty_pending_dir: Arc::new(AtomicI64::new(0)),
            nifty_pending_since: Arc::new(AtomicI64::new(0)),
            order_times: Arc::new(Mutex::new(Vec::new())),
            paper_pending: Arc::new(Mutex::new(HashMap::new())),
            paper_exit_pending: Arc::new(Mutex::new(HashMap::new())),
            margin_cache: Arc::new(Mutex::new(HashMap::new())),
            picked_strikes: Arc::new(Mutex::new((0, Vec::new()))),
            strat_legs: Arc::new(Mutex::new(HashMap::new())),
            et_pending: Arc::new(Mutex::new(HashMap::new())),
            et_placed: Arc::new(AtomicI64::new(0)),
            log_throttle: Arc::new(Mutex::new(HashMap::new())),
            force_tick: Arc::new(AtomicBool::new(false)),
        };
        st.clone().spawn_engine();
        st.clone().spawn_guard();
        st.clone().spawn_nifty_watch();
        st
    }

    fn save(&self) {
        // Serialize under the lock (cheap-ish) but NEVER do the disk write while
        // holding it: the paper state carries the whole closed ledger (multi-MB),
        // so a slow write under `self.doc` used to stall every snapshot/scan and
        // made the whole pane feel frozen. Clone the bytes, drop the guard, then
        // write outside the lock.
        let bytes = {
            let d = match self.doc.lock() {
                Ok(d) => d,
                Err(_) => return,
            };
            match serde_json::to_string(&*d) {
                Ok(s) => s,
                Err(_) => return,
            }
        };
        let _ = write_atomic(&state_path(self.state_file), bytes.as_bytes());
    }

    fn log(&self, level: &str, msg: &str) {
        let entry = json!({ "t": now_ms(), "level": level, "msg": msg });
        if let Ok(mut d) = self.doc.lock() {
            d.logs.push(entry);
            let n = d.logs.len();
            if n > 600 {
                d.logs.drain(0..n - 600);
            }
        }
    }

    /// Emit a log line at most once per `ttl_ms` for a given `key`, so rapidly
    /// re-evaluated skips (no option contracts, empty scanner, ...) do not spam
    /// the Condition Log every engine tick.
    fn log_throttled(&self, key: &str, ttl_ms: i64, level: &str, msg: &str) {
        let now = now_ms();
        {
            let mut m = match self.log_throttle.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            if let Some(last) = m.get(key) {
                if now - *last < ttl_ms {
                    return;
                }
            }
            m.insert(key.to_string(), now);
            if m.len() > 2000 {
                m.retain(|_, v| now - *v < 3_600_000);
            }
        }
        self.log(level, msg);
    }

    fn doc(&self) -> Option<std::sync::MutexGuard<'_, RtDoc>> {
        self.doc.lock().ok()
    }

    /// Full durable engine state (`RtDoc`) as JSON for the Backup tab. This is
    /// the *entire* engine: settings, indicator filters, strategies, saved
    /// templates, staging/final lists, open positions and the whole closed
    /// ledger (trade statistics) - not just the derived UI snapshot.
    pub fn export_state(&self) -> Value {
        self.doc()
            .and_then(|d| serde_json::to_value(&*d).ok())
            .unwrap_or(Value::Null)
    }

    /// Replace the whole durable engine state from a backup, then persist it to
    /// disk. Only a syntactically valid `RtDoc` is accepted. Returns the number
    /// of strategies + positions + closed trades restored for the status line.
    pub fn import_state(&self, v: &Value) -> Result<usize, String> {
        let mut parsed: RtDoc = serde_json::from_value(v.clone())
            .map_err(|e| format!("invalid engine state: {e}"))?;
        // Apply the same normalisation the startup loader does, so a restored
        // state can never carry a nonsense order-rate value.
        if parsed.settings.trade_limit_count <= 0 {
            parsed.settings.trade_limit_count = 5;
        }
        if parsed.settings.order_per_sec <= 0 {
            parsed.settings.order_per_sec = 6;
        }
        parsed.settings.order_per_sec = order_per_sec_budget(parsed.settings.order_per_sec);
        parsed.settings.scan_interval_ms = scan_interval_budget(parsed.settings.scan_interval_ms);
        parsed.settings.scan_bar_close_tf = scan_bar_close_tf(&parsed.settings.scan_bar_close_tf).to_string();
        let count = parsed.strategies.len() + parsed.positions.len() + parsed.closed.len();
        {
            let mut d = self.doc.lock().map_err(|_| "state lock poisoned".to_string())?;
            *d = parsed;
        }
        self.save();
        // Force the next scanner passes to rebuild against the restored state.
        self.last_movers.store(0, Ordering::Relaxed);
        self.last_nifty_scan.store(0, Ordering::Relaxed);
        self.last_trend.store(0, Ordering::Relaxed);
        Ok(count)
    }

    /// Closed-trade ledger + armed flag for the Trade Stats report. Uses the
    /// full ledger (not the snapshot's 500-row cap) so period windows and the
    /// time-of-day analysis see the entire executed history.
    pub fn stats_input(&self) -> (Vec<Value>, bool) {
        match self.doc() {
            Some(d) => (d.closed.clone(), d.armed),
            None => (Vec::new(), false),
        }
    }

    /// Cheap stable hash of the closed ledger + armed flag, seeded by `seed`, so
    /// the stats endpoint can detect an unchanged ledger and answer from its cache
    /// WITHOUT cloning the whole (multi-MB) ledger first.
    pub fn closed_signature(&self, seed: u64) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        seed.hash(&mut h);
        if let Some(d) = self.doc() {
            d.armed.hash(&mut h);
            d.closed.len().hash(&mut h);
            for c in &d.closed {
                c.hash(&mut h);
            }
        }
        h.finish()
    }

    fn ltp_of(&self, sec_id: i64, exch: &str) -> f64 {
        if let Some(v) = self.ltp.lock().ok().and_then(|m| m.get(&sec_id).copied()) {
            if v > 0.0 {
                return v;
            }
        }
        if let Ok(q) = self.dhan.market.quotes.lock() {
            if let Some(v) = q.get(&crate::market::quote_key(sec_id, exch)) {
                if let Some(p) = v.get("ltp").and_then(|x| x.as_f64()) {
                    return p;
                }
            }
        }
        0.0
    }

    /// Refresh the engine LTP cache from a set of scanner quote rows. Called on
    /// every `quote_rows` return path so the entry path resolves option strikes
    /// against the current underlying price. Skipping this on the all-from-feed
    /// fast path left `ltp` pinned to its startup/pre-open value, which resolved
    /// far-from-ATM strikes (e.g. a -20% mover's deep-ITM 1900-PE instead of the
    /// near-ATM leg the readout showed).
    fn seed_ltp_from_rows(&self, rows: &[Value]) {
        if let Ok(mut m) = self.ltp.lock() {
            for r in rows {
                let id = r.get("securityId").and_then(|x| x.as_i64()).unwrap_or(0);
                let l = r.get("last").and_then(|x| x.as_f64()).unwrap_or(0.0);
                if id > 0 && l > 0.0 {
                    m.insert(id, l);
                }
            }
        }
    }

    /// Paper entries are REST-free, so a freshly subscribed leg's first tick can
    /// lag the subscription by a few hundred ms. Poll the live feed over a short
    /// bounded window instead of failing the entry, so a signal is not lost
    /// merely because the quote had not landed yet.
    async fn wait_feed_ltp(&self, sec_id: i64, exch: &str, timeout_ms: u64) -> f64 {
        let first = self.ltp_of(sec_id, exch);
        if first > 0.0 {
            return first;
        }
        let mut waited = 0u64;
        while waited < timeout_ms {
            tokio::time::sleep(Duration::from_millis(60)).await;
            waited += 60;
            let ltp = self.ltp_of(sec_id, exch);
            if ltp > 0.0 {
                return ltp;
            }
        }
        0.0
    }

    /// Tick-native candle series.
    ///
    /// First use seeds the series from one REST fetch; every later call is
    /// served from the in-memory series the live websocket feed keeps patching,
    /// so the strategy scan never waits on (or goes stale behind) a REST
    /// round-trip. If the feed goes quiet the series ages out after
    /// [`LIVE_BAR_TTL`] and the next call re-seeds from REST.
    async fn live_candles(
        &self,
        sec_id: i64,
        exch: &str,
        inst: &str,
        tf: &str,
    ) -> Result<Vec<Candle>, dhan_hq::DhanError> {
        if let Some(c) = self.dhan.market.live_bars_for(sec_id, tf, LIVE_BAR_TTL) {
            return Ok(c);
        }
        match self.dhan.fetch_candles(sec_id, exch, inst, tf).await {
            Ok(c) => {
                self.dhan.market.seed_bars(sec_id, tf, c.clone());
                Ok(c)
            }
            Err(e) => {
                // Never blank a running strategy just because one REST refresh
                // failed - serve the last live series (bounded) instead.
                if let Some(stale) = self
                    .dhan
                    .market
                    .live_bars_for(sec_id, tf, Duration::from_secs(900))
                {
                    Ok(stale)
                } else {
                    Err(e)
                }
            }
        }
    }

    /// `(ltp, oi, volume)` for a security from the shared live-quote cache. The
    /// Order Placement cards and the Auto-Lots liquidity gate read the exact same
    /// live market data the engine trades on.
    fn quote_fields(&self, sec_id: i64, exch: &str) -> (f64, f64, f64) {
        let key = crate::market::quote_key(sec_id, exch);
        if let Ok(q) = self.dhan.market.quotes.lock() {
            if let Some(v) = q.get(&key) {
                let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                return (f("ltp"), f("oi"), f("volume"));
            }
        }
        (self.ltp_of(sec_id, exch), 0.0, 0.0)
    }

    /// `(change, change_pct)` for a security from the shared live-quote cache, with
    /// a fallback to `ltp - prev_ltp` when the feed only published prices. `(0, 0)`
    /// when unquoted. Surfaced on the trade lists so the option premium's
    /// plus/minus sign - the whole point of the "+green premium" pick - is visible
    /// next to the trade.
    fn quote_change(&self, sec_id: i64, exch: &str) -> (f64, f64) {
        let key = crate::market::quote_key(sec_id, exch);
        if let Ok(q) = self.dhan.market.quotes.lock() {
            if let Some(v) = q.get(&key) {
                let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                let ltp = f("ltp");
                let prev = f("prev_ltp");
                let mut chg = f("change");
                let mut pct = f("change_pct");
                if chg == 0.0 && prev > 0.0 && ltp > 0.0 {
                    chg = ltp - prev;
                    pct = (ltp - prev) / prev * 100.0;
                }
                return (round2(chg), round2(pct));
            }
        }
        (0.0, 0.0)
    }

    /// Whether this engine books Dhan broker charges against realized P&L.
    /// Real Dhan trades settle charges broker-side, so only the paper engine
    /// honours the "Deduct Dhan charges" toggle.
    pub fn charges_on(&self) -> bool {
        self.paper && self.doc().map(|d| d.settings.broker_charges).unwrap_or(true)
    }

    /// Realized paper P&L (sum of every closed paper trade).
    fn paper_realized(&self) -> f64 {
        self.doc()
            .map(|d| realized_pnl(&d.closed, d.settings.broker_charges))
            .unwrap_or(0.0)
    }

    /// Paper wallet: starting capital + realized P&L - margin locked by the open
    /// paper positions. Derived (never stored) so it can never drift.
    fn paper_available(&self) -> f64 {
        self.doc()
            .map(|d| {
                paper_available_of(
                    d.settings.paper_capital,
                    &d.closed,
                    &d.positions,
                    d.settings.broker_charges,
                )
            })
            .unwrap_or(200_000.0)
    }

    /// Real Dhan funds available (falls back to None when unknown/zero).
    fn available_funds(&self) -> Option<f64> {
        if self.paper {
            return Some(self.paper_available());
        }
        let f = self.funds.lock().ok()?;
        f.get("availabelBalance")
            .or_else(|| f.get("availableBalance"))
            .and_then(|x| x.as_f64())
            .filter(|v| *v > 0.0)
    }

    /// AI Smart margin budget: a manual rupee amount when set, else the
    /// operator-set share (percentage) of the live available balance. `0` / `0%`
    /// (or unknown funds) means no budget configured, so the margin gate is
    /// disabled; the default `100%` uses the whole wallet.
    fn margin_budget(&self) -> f64 {
        if self.paper {
            // Size the paper budget against the wallet BEFORE the running
            // trades' margin is locked, so `locked` is only ever subtracted
            // once (in the entry gate / margin bar). Using the post-lock
            // available here shrank the budget with every open trade and made
            // the bar report 0 available while capital was still free.
            let (amount, pct, wallet) = self
                .doc()
                .map(|d| {
                    (
                        d.settings.margin_amount,
                        d.settings.margin_pct,
                        paper_wallet_of(d.settings.paper_capital, &d.closed, d.settings.broker_charges),
                    )
                })
                .unwrap_or((0.0, 100.0, 200_000.0));
            return margin_budget_of(amount, pct, wallet);
        }
        let (amount, pct) = self
            .doc()
            .map(|d| (d.settings.margin_amount, d.settings.margin_pct))
            .unwrap_or((0.0, 100.0));
        margin_budget_of(amount, pct, self.available_funds().unwrap_or(0.0))
    }

    /// Total cost locked by open engine positions (qty x entry), plus the count.
    fn locked_margin(&self) -> (f64, i64) {
        let positions = self.doc().map(|d| d.positions.clone()).unwrap_or_default();
        locked_margin_of(&positions)
    }

    /// Margin gate for an already-sized entry: `Some(reason)` when the required
    /// margin (qty x price) exceeds the budget still free after the running
    /// trades. Used by the "skip when margin unavailable" guard and by the live
    /// entry gate, so both make the identical, deterministic decision.
    fn margin_block_for_qty(&self, qty: i64, ltp: f64) -> Option<String> {
        let budget = self.margin_budget();
        if budget <= 0.0 || ltp <= 0.0 || qty <= 0 {
            return None;
        }
        let (locked, running) = self.locked_margin();
        margin_block_reason(qty as f64 * ltp, budget, locked, running)
    }

    /// Early "skip when margin unavailable" pre-check used before a signal is
    /// queued. Mirrors the entry gate's qty sizing for the manual-lots case; when
    /// Auto-Lots is on the qty is sized to fit the budget, so this returns `None`
    /// and the exact gate inside `open_entry` still governs. Returns the block
    /// reason when the signal would be rejected for margin, so the caller can
    /// consume the signal instead of retrying it into a late fill.
    fn early_margin_skip_reason(&self, strat: &Strategy, ltp: f64) -> Option<String> {
        let (auto_lots, lots, lot_size) = self
            .doc()
            .map(|d| (d.auto_lots, d.settings.lots, d.settings.lot_size))
            .unwrap_or((false, 1.0, 0.0));
        if auto_lots {
            return None;
        }
        let lot = if strat.lot > 0.0 {
            strat.lot
        } else if lot_size > 0.0 {
            lot_size
        } else {
            scrip::get()
                .and_then(|s| s.lot_for(&strat.trading_symbol, &strat.exchange_segment).map(|(l, _)| l))
                .unwrap_or(1.0)
        };
        let qty = (lots.max(1.0) * lot).round() as i64;
        self.margin_block_for_qty(qty, ltp)
    }

    /// Whether the operator enabled "skip trade when margin unavailable".
    fn skip_margin_block_enabled(&self) -> bool {
        self.doc().map(|d| d.settings.skip_on_margin_block).unwrap_or(false)
    }

    /// Consume a signal because margin blocked it: latch the fresh edge and mark
    /// the strike traded for this pulse so the SAME signal can never re-queue and
    /// fill late. A genuinely new signal (after the pulse ends) is still allowed.
    fn consume_signal_for_margin(&self, strategy_id: &str, security_id: i64) {
        self.set_sig_state(strategy_id, true);
        self.pulse_mark(strategy_id, security_id);
    }

    /// Entry-gate edge state for the multi-position "fresh signal" mode: whether
    /// the gate held on the previous scan for this strategy.
    fn sig_state_is_set(&self, strategy_id: &str) -> bool {
        self.sig_state
            .lock()
            .ok()
            .and_then(|m| m.get(strategy_id).copied())
            .unwrap_or(false)
    }

    /// Record the entry-gate result for the multi-position "fresh signal" edge
    /// detector. `true` as soon as an entry is placed so a gate that stays true
    /// does not stack another position until it resets and re-triggers.
    fn set_sig_state(&self, strategy_id: &str, val: bool) {
        if let Ok(mut m) = self.sig_state.lock() {
            if m.len() > 5000 {
                m.clear();
            }
            m.insert(strategy_id.to_string(), val);
        }
    }

    /// Release the fresh-signal edge for a queued entry that resolved without
    /// actually filling (margin block, rejection, engine stopped/disarmed). In
    /// "fresh signal" mode the edge is consumed at queue time, so a fill that
    /// never lands must hand it back; otherwise the strategy latches silent and
    /// stops trading until the gate happens to reset and print a brand-new
    /// signal.
    fn release_fresh_edge(&self, strategy_id: &str) {
        self.clear_signal_edge(strategy_id);
    }

    /// End the current signal pulse for a strategy: the gate stopped holding, so
    /// the next time it holds is a brand-new fresh signal. Clears both the
    /// fresh-edge flag and the per-strike "already traded this pulse" memory.
    fn clear_signal_edge(&self, strategy_id: &str) {
        self.set_sig_state(strategy_id, false);
        self.pulse_reset(strategy_id);
    }

    /// Forget the contracts a strategy traded during its current signal pulse, so
    /// the next fresh signal may trade them again.
    fn pulse_reset(&self, strategy_id: &str) {
        if let Ok(mut m) = self.pulse_traded.lock() {
            m.remove(strategy_id);
        }
    }

    /// True when this strategy has already traded this exact contract during the
    /// CURRENT signal pulse (blocked until the pulse ends and a fresh signal
    /// begins).
    fn pulse_blocked(&self, strategy_id: &str, security_id: i64) -> bool {
        self.pulse_traded
            .lock()
            .ok()
            .and_then(|m| m.get(strategy_id).map(|s| s.contains(&security_id)))
            .unwrap_or(false)
    }

    /// Record that this strategy traded this contract during the current pulse.
    fn pulse_mark(&self, strategy_id: &str, security_id: i64) {
        if security_id <= 0 {
            return;
        }
        if let Ok(mut m) = self.pulse_traded.lock() {
            if m.len() > 5000 {
                m.clear();
            }
            m.entry(strategy_id.to_string()).or_default().insert(security_id);
        }
    }

    /// The leg's gate held on this scan: cancel any pending signal-end debounce so
    /// a brief dip inside one signal never ends it.
    fn note_signal_pass(&self, strategy_id: &str) {
        if let Ok(mut m) = self.sig_fail_since.lock() {
            m.remove(strategy_id);
        }
    }

    /// The leg's gate failed on this scan. Returns `true` only once the failure
    /// has been continuous for [`SIGNAL_END_DEBOUNCE_MS`], i.e. a genuine signal
    /// end (reset the pulse) rather than a single-scan flicker.
    fn end_signal_if_due(&self, strategy_id: &str) -> bool {
        let now = now_ms();
        let Ok(mut m) = self.sig_fail_since.lock() else { return false };
        if m.len() > 5000 {
            m.clear();
        }
        let since = *m.entry(strategy_id.to_string()).or_insert(now);
        now - since >= SIGNAL_END_DEBOUNCE_MS
    }

    /// Trades already taken for a strategy: open positions plus closed trades.
    /// The "Trades per strategy / Max trades" cap is counted against this.
    fn trades_done(&self, strategy_id: &str) -> i64 {
        self.doc()
            .map(|d| {
                d.positions.iter().filter(|p| js(p, "strategyId") == strategy_id).count()
                    + d.closed.iter().filter(|c| js(c, "strategyId") == strategy_id).count()
            })
            .unwrap_or(0) as i64
    }

    /// True when a running position already holds this exact option contract
    /// (same `securityId`). Used to stop the engine from opening a second entry
    /// on a strike that is already being traded - a fresh-signal re-trigger, a
    /// second strategy resolving to the same strike, or a scan after a restart
    /// (which clears the in-memory signal edge) must not double the position.
    fn contract_running(&self, security_id: i64) -> bool {
        if security_id <= 0 {
            return false;
        }
        self.doc()
            .map(|d| d.positions.iter().any(|p| ji(p, "securityId") == security_id))
            .unwrap_or(false)
    }

    /// Whether a new entry on `security_id` must be blocked as a duplicate of an
    /// already-running position on the same contract. `guard` is false for the
    /// operator-driven Manual Order Placement path; the explicit "repeat while
    /// condition holds" stacking mode (multi-position B) is always exempt so it
    /// can keep stacking as designed.
    fn duplicate_entry_blocked(&self, settings: &Settings, security_id: i64, guard: bool) -> bool {
        guard && !settings.multi_always_on && self.contract_running(security_id)
    }

    /// "One trade per signal" guard. Returns a human-readable reason when the
    /// scanner leg must be blocked, else `None`. The rule is purely signal-scoped:
    /// once a leg has traded a contract during the CURRENT signal pulse it is
    /// held until that pulse ends, no matter whether the position is still open.
    /// A genuinely new signal clears the pulse (see `note_signal_pass` /
    /// `end_signal_if_due`), so the very next signal may trade the same strike
    /// again. A running position therefore never blocks a fresh signal - only a
    /// repeat inside the same pulse is blocked.
    fn once_per_signal_block_reason(
        &self,
        settings: &Settings,
        strat: &Strategy,
        security_id: i64,
    ) -> Option<&'static str> {
        if !settings.once_per_signal || !strat.synthetic {
            return None;
        }
        if self.pulse_blocked(&strat.id, security_id) {
            return Some("already traded this strike on the current signal");
        }
        None
    }

    /// Margin bar readout for the Running Trades section. Takes the already
    /// cloned settings/positions plus the caller-resolved available funds so a
    /// caller holding the doc lock never re-locks it (the paper wallet is derived
    /// from the very same doc snapshot).
    fn margin_info_from(&self, margin_amount: f64, margin_pct: f64, positions: &[Value], available: f64) -> Value {
        let pct = margin_pct.clamp(0.0, 100.0);
        let balance = available.max(0.0);
        let budget = margin_budget_of(margin_amount, margin_pct, balance);
        let (locked, count) = locked_margin_of(positions);
        json!({
            "budget": round2(budget),
            "locked": round2(locked.max(0.0)),
            "available": round2((budget - locked.max(0.0)).max(0.0)),
            "count": count,
            "configured": budget > 0.0,
            "pct": round2(pct),
            "amount": round2(margin_amount.max(0.0)),
            "balance": round2(balance),
        })
    }

    /// Dhan `/margincalculator` response for a hypothetical order (margin
    /// required + broker-reported available balance).
    async fn margin_required(
        &self,
        sec_id: i64,
        exch: &str,
        side: &str,
        qty: i64,
        price: f64,
        product: &str,
    ) -> Option<MarginResponse> {
        if qty <= 0 || price <= 0.0 {
            return None;
        }
        // Paper mode never calls the broker margin calculator.
        if self.paper {
            return None;
        }
        // Indices are not tradable instruments, so Dhan rejects a margin request
        // for them (IDX_I). Skip the round-trip entirely - this is the selection
        // present on most option-chain / chart switches, and it used to stall the
        // switch for a throttle plus a rejected call.
        let exch_up = exch.to_uppercase();
        if exch_up.starts_with("IDX") || exch_up == "INDEX" {
            return None;
        }
        // Serve a recent answer for the same contract/side/size when the price has
        // barely moved. The broker margin API is rate-limited to ~1 call/second, so
        // without this cache a strike switch waited ~1.5-2s on it (and the chart's
        // own Dhan candle fetch then queued behind the same throttle).
        let key = format!(
            "{}:{}:{}:{}:{}",
            sec_id,
            exch_up,
            side.to_uppercase(),
            qty,
            product.to_uppercase()
        );
        const MARGIN_TTL_MS: i64 = 20_000;
        if let Ok(g) = self.margin_cache.lock() {
            if let Some((at, cached_price, resp)) = g.get(&key) {
                if now_ms() - *at < MARGIN_TTL_MS
                    && *cached_price > 0.0
                    && (price - *cached_price).abs() / *cached_price < 0.005
                {
                    return resp.clone();
                }
            }
        }
        let client = self.dhan.session_client().await?;
        let client_id = self.dhan.session_client_id().await?;
        let req = MarginRequest {
            dhan_client_id: client_id,
            exchange_segment: seg_from(exch),
            transaction_type: txn(side),
            quantity: qty,
            product_type: product_from(product),
            security_id: sec_id.to_string(),
            price,
            trigger_price: None,
        };
        self.dhan.dhan_acct_throttle().await;
        // A rejected request (bad contract / rate limit) is cached as `None` so the
        // UI answers instantly instead of re-paying the throttle on every switch.
        let resp = client.margin_calculator(&req).await.ok();
        if let Ok(mut g) = self.margin_cache.lock() {
            if g.len() > 512 {
                g.retain(|_, (at, _, _)| now_ms() - *at < MARGIN_TTL_MS);
            }
            g.insert(key, (now_ms(), price, resp.clone()));
        }
        resp
    }

    /// Orders/sec budget: true when fewer than `max_per_sec` orders were
    /// placed in the trailing second.
    fn order_allow(&self, max_per_sec: i64) -> bool {
        if max_per_sec <= 0 {
            return false;
        }
        let now = now_ms();
        let mut g = match self.order_times.lock() {
            Ok(g) => g,
            Err(_) => return true,
        };
        g.retain(|t| now - *t < 1000);
        (g.len() as i64) < max_per_sec
    }

    fn order_record(&self) {
        let now = now_ms();
        if let Ok(mut g) = self.order_times.lock() {
            g.push(now);
        }
    }

    // -----------------------------------------------------------------------
    // Engine loop
    // -----------------------------------------------------------------------

    fn spawn_engine(self) {
        tokio::spawn(async move {
            // Engine loop. Housekeeping runs on a base ~100ms cadence; the entry
            // scan is additionally throttled by the "Engine Scan" setting when it
            // is enabled (see `scan_interval_on` / `scan_interval_ms`). Exits are
            // never throttled - they live in the dedicated 50ms guardian task.
            let mut last_scan = 0i64;
            // Last bar bucket seen in "Countdown to Bar Close" scan mode; a change
            // means a new bar just opened, which is exactly when we scan.
            let mut last_bar_bucket = 0i64;
            loop {
                // Scanners run whenever their own toggle is ON, independent of the
                // engine run/arm state, so the Top Movers / commodity readouts
                // always fetch (old app polled them outside the run gate).
                // Both the real and paper engines behave identically here; each
                // engine owns its own settings/toggles, so an idle tab only fetches
                // for scanners the operator enabled on that tab.
                let manual_mode = self
                    .doc()
                    .map(|d| d.settings.manual_strikes_enabled)
                    .unwrap_or(false);
                if manual_mode {
                    // Manual Strike Select (testing): the scanner universes are
                    // inactive, and the Picked Strikes readout is the manual list.
                    self.refresh_manual_picked();
                } else {
                    // Leaving manual mode: drop any leftover manual rows so the
                    // scanner readouts own the list again.
                    self.update_picked("Manual", Vec::new());
                    self.refresh_movers().await;
                    self.refresh_nifty_trend().await;
                    self.refresh_nifty_scan().await;
                }
                self.sync_auto_side();
                // Broker account (funds/positions/holdings) and open-position LTP
                // refresh regardless of run/arm, so the Account view is live
                // whenever Dhan is connected (the old app fetched account data on
                // demand, independent of the engine run state).
                self.refresh().await;
                self.reconcile_fills().await;
                // Resync the broker book on the reconnect edge so a stop leg that
                // fired (or a manual fill) while the feed was down is never lost.
                self.reconcile_reconnect().await;
                let force = self.force_tick.swap(false, Ordering::Relaxed);
                if force {
                    if let Some(mut d) = self.doc() {
                        d.engine_on = true;
                    }
                    self.log("info", "Run / Tick Now: immediate evaluation pass");
                }
                // Read the run state plus the Engine Scan config in a single lock.
                let (on, armed, scan_on, scan_ms, scan_bc, scan_bc_tf) = self
                    .doc()
                    .map(|d| {
                        (
                            d.engine_on,
                            d.armed,
                            d.settings.scan_interval_on,
                            d.settings.scan_interval_ms,
                            d.settings.scan_bar_close,
                            d.settings.scan_bar_close_tf.clone(),
                        )
                    })
                    .unwrap_or((false, false, false, 100, false, "1min".into()));
                if on {
                    self.check_square_off().await;
                    if !armed {
                        // Running-but-disarmed places no orders. Surface it on a
                        // throttle so the Condition Log always explains why the
                        // engine looks active yet nothing is being traded.
                        self.log_throttled(
                            "engine-disarmed",
                            15_000,
                            "warn",
                            "engine RUNNING but DISARMED - no orders will be placed (click 'AI Smart Trading: ON' or 'Run / Tick Now' to arm)",
                        );
                    }
                    if armed {
                        // "Engine Scan" throttle. Three cases:
                        //   OFF            -> scan every loop (historical ~100ms).
                        //   ON + bar-close -> scan the instant the selected
                        //                     timeframe rolls to a new bar.
                        //   ON + interval  -> scan every `scan_ms`.
                        // A manual Run / Tick Now always bypasses the wait.
                        let due = if force || !scan_on {
                            true
                        } else if scan_bc {
                            let step = scan_bar_close_step_ms(&scan_bc_tf);
                            now_ms() / step != last_bar_bucket
                        } else {
                            now_ms().saturating_sub(last_scan) >= scan_ms
                        };
                        if due {
                            self.scan_signals().await;
                            last_scan = now_ms();
                            if scan_bc {
                                last_bar_bucket = now_ms() / scan_bar_close_step_ms(&scan_bc_tf);
                            }
                        }
                    }
                    // Position protection (trail / SL / TP) does NOT run here: it
                    // lives in the dedicated 50ms guardian task so a slow scanner
                    // REST call can never delay a trailing-stop exit.
                    self.reconcile_super().await;
                } else {
                    // Idle: forget the last bar so re-arming in bar-close mode
                    // fires an immediate scan instead of waiting for the next bar.
                    last_bar_bucket = 0;
                }
                // Base cadence: wake at most every 100ms for housekeeping, but
                // honour a sub-100ms Engine Scan value (e.g. 50ms) instead of
                // rounding it up. A larger interval (or bar-close mode) simply
                // leaves the scan gated - 100ms is fast enough to catch a bar
                // boundary the moment it happens.
                let base_ms = if on && scan_on && !scan_bc {
                    scan_ms.clamp(SCAN_INTERVAL_MIN_MS, 100)
                } else {
                    100
                };
                tokio::time::sleep(Duration::from_millis(base_ms.max(SCAN_INTERVAL_MIN_MS) as u64)).await;
            }
        });
    }

    /// Dedicated ultrafast position guardian.
    ///
    /// Runs on its own 50ms timer, independent of the network-bound scanner /
    /// entry loop, so an open trade's trailing stop (and SL / TP) is evaluated
    /// against the freshest tick and booked the instant it is hit. This is what
    /// makes a reversing trade cut immediately instead of after the scanner's
    /// REST round-trips. It only ever exits - new entries stay gated by the main
    /// engine loop's arm state.
    fn spawn_guard(self) {
        tokio::spawn(async move {
            // Wake on every applied market tick so a reversing trade is cut on
            // the very tick that breaches its stop, not on the next poll. The
            // 50ms interval is only a fallback for when the feed is quiet.
            let notify = self.dhan.market.tick_notify();
            let mut tick = tokio::time::interval(Duration::from_millis(50));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let on = self.doc.lock().map(|d| d.engine_on).unwrap_or(false);
                if on {
                    tokio::select! {
                        _ = notify.notified() => {}
                        _ = tick.tick() => {}
                    }
                } else {
                    // Idle: don't wake (and take the state lock) on every
                    // market tick; the 50ms poll is enough to notice arming.
                    tick.tick().await;
                }
                // Exits (SL / TP / trail) are managed only while the live feed is
                // streaming, so a stale mark can never book a fake fill.
                if self.doc.lock().map(|d| d.engine_on).unwrap_or(false)
                    && self.dhan.feed_live()
                {
                    self.manage_positions().await;
                }
            }
        });
    }

    /// Dedicated tick-native NIFTY trend watchdog.
    ///
    /// Wakes on every applied market tick and re-reads the live forming candle
    /// series, so a bullish<->bearish flip on the assigned straight-line
    /// indicators is applied within milliseconds - never on a candle close. A
    /// 200ms poll is only the fallback for when the feed is quiet.
    fn spawn_nifty_watch(self) {
        tokio::spawn(async move {
            let notify = self.dhan.market.tick_notify();
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = tick.tick() => {}
                }
                let on = self
                    .doc()
                    .map(|d| d.settings.nifty_trend_on && !d.settings.manual_strikes_enabled)
                    .unwrap_or(false);
                if on {
                    self.refresh_nifty_flip().await;
                }
            }
        });
    }

    /// Refresh broker positions, funds and option LTP (rate limited).
    async fn refresh(&self) {
        // Paper mode: publish the virtual wallet (capital + realized P&L - locked
        // margin) into the same funds cache the UI reads, and never call the Dhan
        // account/position/holding APIs. Live leg prices come from the shared
        // quote cache populated by the Dhan feed.
        if self.paper {
            let now = now_ms();
            if now - self.last_funds.load(Ordering::Relaxed) >= 1500 {
                self.last_funds.store(now, Ordering::Relaxed);
                let avail = self.paper_available();
                let cap = self
                    .doc()
                    .map(|d| d.settings.paper_capital)
                    .filter(|c| *c > 0.0)
                    .unwrap_or(200_000.0);
                let realized = self.paper_realized();
                if let Ok(mut g) = self.funds.lock() {
                    *g = json!({
                        "availabelBalance": round2(avail),
                        "availableBalance": round2(avail),
                        "openingBalance": round2(cap),
                        "realizedPnl": round2(realized),
                        "paper": true,
                    });
                }
            }
            // Mark open paper positions to the shared Dhan feed quote cache. This
            // is websocket-fed (no REST), and refreshing it here - independent of
            // the paper engine run/arm state - matches how the real tab keeps its
            // LTP cache fresh, so the paper ledger P&L is live even while stopped.
            let open: Vec<(i64, String)> = self
                .doc()
                .map(|d| {
                    d.positions
                        .iter()
                        .map(|p| (ji(p, "securityId"), js(p, "exchangeSegment")))
                        .collect()
                })
                .unwrap_or_default();
            if !open.is_empty() {
                let marks: Vec<(i64, f64)> = {
                    match self.dhan.market.quotes.lock() {
                        Ok(q) => open
                            .iter()
                            .filter_map(|(sid, exch)| {
                                if *sid <= 0 {
                                    return None;
                                }
                                let key = crate::market::quote_key(*sid, exch);
                                q.get(&key)
                                    .and_then(|v| v.get("ltp"))
                                    .and_then(|x| x.as_f64())
                                    .filter(|p| *p > 0.0)
                                    .map(|p| (*sid, p))
                            })
                            .collect(),
                        Err(_) => Vec::new(),
                    }
                };
                if !marks.is_empty() {
                    if let Ok(mut m) = self.ltp.lock() {
                        for (sid, p) in marks {
                            m.insert(sid, p);
                        }
                    }
                }
            }
            return;
        }
        if !self.dhan.is_connected().await {
            return;
        }
        // Yield Dhan's single data slot while the operator is interacting
        // (chart / option chain / quotes); the 100ms engine tick retries.
        if self.dhan.user_active() {
            return;
        }
        let now = now_ms();
        let last_ltp = self.last_ltp.load(Ordering::Relaxed);
        if now - last_ltp >= 3000 {
            self.last_ltp.store(now, Ordering::Relaxed);
            let open: Vec<(i64, String)> = {
                self.doc()
                    .map(|d| {
                        d.positions
                            .iter()
                            .map(|p| (ji(p, "securityId"), js(p, "exchangeSegment")))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            if !open.is_empty() {
                let mut req: BTreeMap<String, Vec<i64>> = BTreeMap::new();
                for (sid, seg) in open {
                    if sid > 0 {
                        req.entry(seg).or_default().push(sid);
                    }
                }
                if let Some(client) = self.dhan.session_client().await {
                    self.dhan.dhan_throttle().await;
                    match client.market_feed_ltp(&req).await {
                        Ok(resp) => {
                            if let Ok(mut m) = self.ltp.lock() {
                                for (_seg, legs) in resp {
                                    for (sid, e) in legs {
                                        if let Ok(id) = sid.parse::<i64>() {
                                            if e.last_price > 0.0 {
                                                m.insert(id, e.last_price);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = e;
                        }
                    }
                }
            }
        }

        let last_acct = self.last_acct.load(Ordering::Relaxed);
        if now - last_acct >= 8000 {
            self.last_acct.store(now, Ordering::Relaxed);
            if let Some(client) = self.dhan.session_client().await {
                self.dhan.dhan_acct_throttle().await;
                if let Ok(rows) = client.positions().await {
                    let vals: Vec<Value> = rows
                        .iter()
                        .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
                        .collect();
                    if let Ok(mut g) = self.broker_positions.lock() {
                        *g = vals;
                    }
                }
            }
        }

        let last_funds = self.last_funds.load(Ordering::Relaxed);
        if now - last_funds >= 30000 {
            self.last_funds.store(now, Ordering::Relaxed);
            if let Some(client) = self.dhan.session_client().await {
                self.dhan.dhan_acct_throttle().await;
                match client.fund_limit().await {
                    Ok(f) => {
                        if let Ok(mut g) = self.funds.lock() {
                            *g = serde_json::to_value(&f).unwrap_or(json!({}));
                        }
                    }
                    Err(e) => self.log_throttled(
                        "acct-funds",
                        30_000,
                        "warn",
                        &format!("funds fetch failed: {e}"),
                    ),
                }
                self.dhan.dhan_acct_throttle().await;
                match client.holdings().await {
                    Ok(rows) => {
                        let vals: Vec<Value> = rows
                            .iter()
                            .map(|h| serde_json::to_value(h).unwrap_or(Value::Null))
                            .collect();
                        if let Ok(mut g) = self.broker_holdings.lock() {
                            *g = vals;
                        }
                    }
                    Err(e) if e.is_empty_holdings() => {
                        // A clean account with no delivery holdings is not a
                        // failure: Dhan answers DH-1111 instead of an empty list.
                        // Cache it as empty and keep the Condition Log quiet.
                        if let Ok(mut g) = self.broker_holdings.lock() {
                            g.clear();
                        }
                    }
                    Err(e) => self.log_throttled(
                        "acct-holdings",
                        30_000,
                        "warn",
                        &format!("holdings fetch failed: {e}"),
                    ),
                }
            }
        }
    }

    async fn check_square_off(&self) {
        let (enabled, at, day) = {
            let Some(d) = self.doc() else { return };
            (d.settings.auto_square_off_enabled, d.settings.auto_square_off_time.clone(), d.last_square_off_day.clone())
        };
        if !enabled {
            return;
        }
        let Some(at_min) = hhmm_to_minutes(&at) else { return };
        let today = ist_day_key();
        if day == today || ist_minutes() < at_min {
            return;
        }
        if let Some(d) = self.doc() {
            if d.positions.is_empty() {
                return;
            }
        }
        self.log("warn", &format!("auto square-off at {at}"));
        self.square_off_all("auto_square_off").await;
        if let Some(mut d) = self.doc() {
            d.last_square_off_day = today;
            d.armed = false;
        }
        self.save();
    }

    // -----------------------------------------------------------------------
    // Top Movers scanner (auto CE/PE side)
    // -----------------------------------------------------------------------

    /// Auto option side from movers dominance.
    /// The NIFTY straight-line direction is deliberately NOT part of this global
    /// source: NIFTY Trend Following applies only to F&O stocks, so it is applied
    /// per strategy via [`Self::nifty_locked_side`] instead of leaking onto index
    /// and commodity strategies. Returns `None` when the scanner has not decided a
    /// direction, so the caller falls back to the strategy's own bullish/bearish
    /// side.
    fn auto_option_side(&self, settings: &Settings) -> Option<&'static str> {
        // Selecting BOTH Top-Mover legs (a positive Top Gainers count AND a
        // positive Top Losers count) is the operator's explicit instruction to
        // work both the bullish (CE) and bearish (PE) legs at once, so the
        // movers dominance bias must NOT collapse the scanner onto a single
        // side. The bias still picks the side when exactly one leg is selected.
        if settings.movers_on && !movers_both_legs(settings) {
            let b = self.mover_bias.load(Ordering::Relaxed);
            if b > 0 {
                return Some("CE");
            }
            if b < 0 {
                return Some("PE");
            }
        }
        None
    }

    /// NIFTY straight-line direction, scoped to F&O-stock strategies only. The
    /// NIFTY Trend Following feature must never influence index or commodity
    /// strategies, so this returns `None` unless the strategy's category is F&O.
    /// `None` too when the feature is off or the lines are tied (no committed
    /// direction).
    fn nifty_side_for(&self, strat: &Strategy, settings: &Settings) -> Option<&'static str> {
        if !settings.nifty_trend_on || strat_category(strat) != StratCat::Fno {
            return None;
        }
        match self.nifty_dir.load(Ordering::Relaxed) {
            d if d > 0 => Some("CE"),
            d if d < 0 => Some("PE"),
            _ => None,
        }
    }

    /// Strict-gate check for the NIFTY-trend universe (Top Movers Gainers/Losers
    /// + NIFTY-trend picks). `bull` = the leg being considered is a CE / Top
    /// Gainer leg. Returns `false` (hard block) when either enabled strict gate
    /// disagrees with the assigned straight-line confirmation indicators:
    ///   * net gate  (`nifty_net_strict`)       - CE needs raw net > 0, PE < 0;
    ///     a tie / dead-heat fails.
    ///   * agree gate (`nifty_all_agree_strict`) - CE needs every assigned line
    ///     bullish (+1), PE every line bearish (-1); a mixed read fails.
    /// With the feature off, no gate enabled, or NIFTY Trend Following off this
    /// is a no-op (`true`), so nothing else is affected.
    fn nifty_line_side_ok(&self, settings: &Settings, bull: bool) -> bool {
        if !settings.nifty_trend_on || (!settings.nifty_net_strict && !settings.nifty_all_agree_strict) {
            return true;
        }
        if settings.nifty_net_strict {
            let n = self.nifty_raw_net.load(Ordering::Relaxed);
            if bull && n <= 0 {
                return false;
            }
            if !bull && n >= 0 {
                return false;
            }
        }
        if settings.nifty_all_agree_strict {
            let a = self.nifty_agree.load(Ordering::Relaxed);
            if bull && a != 1 {
                return false;
            }
            if !bull && a != -1 {
                return false;
            }
        }
        true
    }

    /// Strict NIFTY straight-line direction lock, applied to F&O stocks only.
    /// While NIFTY Trend Following is on and the straight lines have committed a
    /// net direction, ONLY that side may trade for F&O strategies: a bullish NIFTY
    /// can never open a PE trade and a bearish NIFTY can never open a CE trade.
    /// Indices and commodities are never locked. `None` when the feature is off,
    /// the strategy is not an F&O stock, or the lines are tied (no committed
    /// direction), so callers keep their normal resolution.
    fn nifty_locked_side(&self, strat: &Strategy, settings: &Settings) -> Option<&'static str> {
        // Per-strategy side routing owns the direction: the strategy's own
        // stock/filter side decides the leg, so the NIFTY lock must not flip it.
        if per_strategy_side(settings) {
            return None;
        }
        self.nifty_side_for(strat, settings)
    }

    /// "Run Strategy In" override side (old AST `effectiveRunInSide`): the manual
    /// CE/PE when the override is on, or the live scanner direction in Auto
    /// Select Mode (falling back to the manual side while no auto signal is
    /// available). `None` when the override is off - callers then keep their
    /// normal option-side resolution.
    fn effective_run_in_side(&self, settings: &Settings) -> Option<&'static str> {
        // Per-strategy side routing disables the "Run Strategy In" CE/PE override.
        if per_strategy_side(settings) || !settings.run_in_enabled {
            return None;
        }
        if settings.run_in_auto {
            // Both Top-Mover legs selected: the auto side is deliberately
            // undecided so both legs can run, so do NOT fall back to the manual
            // "Run Strategy In" side - that would re-narrow the scanner to one
            // leg and defeat the operator's both-legs selection.
            if movers_both_legs(settings) {
                return None;
            }
            if let Some(side) = self.auto_option_side(settings) {
                return Some(side);
            }
        }
        match settings.run_in_side.to_uppercase().as_str() {
            "CE" => Some("CE"),
            "PE" => Some("PE"),
            _ => None,
        }
    }

    /// The one side the engine is trading right now: the explicit "Run Strategy
    /// In" override when it is enabled, else the live scanner direction. Every
    /// direction decision (which synthetic strategies to build, the Overall
    /// direction filter, the executed option leg and the readouts) must read this
    /// single source, otherwise the filter side and the traded contract can
    /// disagree and an entry fires opposite to the filter that gated it.
    fn active_side(&self, settings: &Settings) -> Option<&'static str> {
        // Per-strategy side routing is per strategy: there is no single global
        // side to narrow the scanner/Overall-direction with, so report
        // "undecided" and let each strategy's own side drive its leg.
        if per_strategy_side(settings) {
            return None;
        }
        self.effective_run_in_side(settings)
            .or_else(|| self.auto_option_side(settings))
    }

    /// Re-resolve the picked strikes / strategy legs on the same pass whenever
    /// the live auto CE/PE side flips. Without this the scanner readouts kept
    /// the previous side until their own throttle (15s movers) expired, which
    /// looked like "Auto Select Mode is stuck" even though the direction had
    /// already changed.
    fn sync_auto_side(&self) {
        let settings = self.doc().map(|d| d.settings.clone()).unwrap_or_default();
        let cur = match self.auto_option_side(&settings) {
            Some("CE") => 1,
            Some("PE") => -1,
            _ => 0,
        };
        let prev = self.last_auto_side.swap(cur, Ordering::Relaxed);
        if prev != cur {
            self.last_movers.store(0, Ordering::Relaxed);
        }
    }

    /// Option side used for the "Run Strategy In / No <side> option contracts"
    /// diagnostics: the operator's global run-in side wins, else the live scanner
    /// direction, else the strategy's own bullish/bearish side.
    fn desired_option_side(&self, settings: &Settings, strat: &Strategy) -> String {
        if let Some(side) = routed_option_side(settings, strat) {
            return side.to_string();
        }
        if let Some(side) = self.nifty_locked_side(strat, settings) {
            return side.to_string();
        }
        if let Some(side) = self.effective_run_in_side(settings) {
            return side.to_string();
        }
        self.auto_option_side(settings)
            .map(|s| s.to_string())
            .unwrap_or_else(|| if strategy_is_bull(strat) { "CE".to_string() } else { "PE".to_string() })
    }

    /// Scan universe for the Top Movers scanner: the full market
    /// catalog (dropdown symbols + market-watch companies), NIFTY itself and the
    /// operator's strategy symbols. Indices are only included when
    /// `include_indices` is set; MCX commodities only when `include_commodities`
    /// is set (they fall back to the default MCX set when no custom list exists).
    fn scan_universe(&self, include_indices: bool, include_commodities: bool) -> Vec<(i64, String)> {
        let mut out: Vec<(i64, String)> = Vec::new();
        let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
        // Operator-removed scanner picks: an excluded underlying never enters
        // the universe, so it is neither ranked (Top Movers) nor
        // resolved to an option leg nor traded, until it is restored.
        let excluded: std::collections::HashSet<i64> = self
            .doc()
            .map(|d| d.settings.scanner_exclude.iter().copied().collect())
            .unwrap_or_default();
        let push = |sid: i64, seg: String, out: &mut Vec<(i64, String)>, seen: &mut std::collections::HashSet<i64>| {
            if sid <= 0 || excluded.contains(&sid) || !seen.insert(sid) {
                return;
            }
            let u = seg.to_uppercase();
            if u == "IDX_I" && !include_indices {
                return;
            }
            if u == "MCX_COMM" && !include_commodities {
                return;
            }
            out.push((sid, seg));
        };
        for (sid, seg) in crate::market::securities() {
            push(sid, seg, &mut out, &mut seen);
        }
        if let Some(d) = self.doc() {
            for s in &d.strategies {
                if s.security_id > 0 {
                    push(s.security_id, s.exchange_segment.clone(), &mut out, &mut seen);
                }
            }
            if include_commodities {
                let mut list = d.settings.commodity_list.clone();
                if list.is_empty() {
                    list = crate::market::commodity_ids();
                }
                for id in list {
                    push(id, "MCX_COMM".to_string(), &mut out, &mut seen);
                }
            }
        }
        out
    }

    /// Build scanner rows for a `(security_id, segment)` universe. Prefers the
    /// live websocket quote cache (populated for every catalog symbol while the
    /// feed is up) and only falls back to Dhan's REST quote API for ids the feed
    /// has not streamed yet. Logs when a scan comes back empty.
    async fn quote_rows(&self, univ: &[(i64, String)]) -> Vec<Value> {
        let mut rows: Vec<Value> = Vec::new();
        let mut missing: Vec<(i64, String)> = Vec::new();
        {
            let cache = self.dhan.market.quotes.lock().ok();
            for (id, seg) in univ {
                if *id <= 0 {
                    continue;
                }
                let key = crate::market::quote_key(*id, seg);
                let cached = cache
                    .as_ref()
                    .and_then(|c| c.get(&key))
                    .filter(|q| q.get("synth").is_none());
                if let Some(q) = cached {
                    let prev = q.get("close").and_then(|x| x.as_f64()).unwrap_or(0.0);
                    let pct = q
                        .get("change_pct")
                        .and_then(|x| x.as_f64())
                        .unwrap_or_else(|| {
                            let chg = q.get("change").and_then(|x| x.as_f64()).unwrap_or(0.0);
                            if prev > 0.0 { chg / prev * 100.0 } else { 0.0 }
                        });
                    let ltp = q
                        .get("ltp")
                        .and_then(|x| x.as_f64())
                        .or_else(|| q.get("last").and_then(|x| x.as_f64()))
                        .unwrap_or(0.0);
                    if ltp > 0.0 {
                        rows.push(json!({
                            "securityId": id,
                            "segment": seg,
                            "last": ltp,
                            "changePct": pct,
                            "volume": q.get("volume").and_then(|x| x.as_f64()).unwrap_or(0.0),
                        }));
                        continue;
                    }
                }
                missing.push((*id, seg.clone()));
            }
        }
        if missing.is_empty() {
            if rows.is_empty() {
                self.log_throttled(
                    "scanner-quote-empty-cache",
                    15_000,
                    "warn",
                    &format!("scanner quote empty for {} ids (feed cache)", univ.len()),
                );
            }
            self.seed_ltp_from_rows(&rows);
            return rows;
        }
        let Some(client) = self.dhan.session_client().await else {
            self.log_throttled("scanner-quote-nosession", 15_000, "warn", "scanner quote: no Dhan session client");
            return rows;
        };
        for chunk in missing.chunks(100) {
            let mut req: BTreeMap<String, Vec<i64>> = BTreeMap::new();
            for (id, seg) in chunk {
                if *id > 0 {
                    req.entry(seg.to_uppercase()).or_default().push(*id);
                }
            }
            if req.is_empty() {
                continue;
            }
            self.dhan.dhan_throttle().await;
            match client.market_feed_quote(&req).await {
                Ok(resp) => {
                    for (seg, m) in resp.iter() {
                        for (sid, q) in m.iter() {
                            let prev = q.ohlc.as_ref().map(|o| o.close).unwrap_or(0.0);
                            let pct = if prev > 0.0 { q.net_change / prev * 100.0 } else { 0.0 };
                            rows.push(json!({
                                "securityId": sid.parse::<i64>().unwrap_or(0),
                                "segment": seg,
                                "last": q.last_price,
                                "changePct": pct,
                                "volume": q.volume,
                            }));
                        }
                    }
                }
                Err(e) => self.log_throttled(
                    "scanner-quote-failed",
                    15_000,
                    "warn",
                    &format!("scanner quote failed ({} ids): {e}", chunk.len()),
                ),
            }
        }
        if rows.is_empty() {
            self.log_throttled(
                "scanner-quote-empty",
                15_000,
                "warn",
                &format!("scanner quote empty for {} ids", univ.len()),
            );
        }
        // Seed the engine LTP cache from the poll we just paid for, so the entry
        // path reads the underlying's price here instead of making another
        // throttled quote call before it can resolve the option leg.
        self.seed_ltp_from_rows(&rows);
        rows
    }

    async fn refresh_movers(&self) {
        let now = now_ms();
        if now - self.last_movers.load(Ordering::Relaxed) < 100 {
            return;
        }
        let (enabled, gain_n, lose_n, indices) = self
            .doc()
            .map(|d| (d.settings.movers_on, d.settings.movers_gainers, d.settings.movers_losers, d.settings.movers_indices.clone()))
            .unwrap_or((false, 5, 5, Vec::new()));
        if !enabled {
            self.mover_bias.store(0, Ordering::Relaxed);
            self.update_picked("Top Movers", Vec::new());
            return;
        }
        if !self.dhan.is_connected().await {
            return;
        }
        // Yield Dhan's single data slot while the operator is interacting; the
        // next engine tick retries (last_movers is deliberately left untouched).
        if self.dhan.user_active() {
            return;
        }
        self.last_movers.store(now, Ordering::Relaxed);

        let commodity_on = self.doc().map(|d| d.settings.commodity_on).unwrap_or(false);
        let mut univ: Vec<(i64, String)> = self.scan_universe(false, commodity_on);
        // Indices live under IDX_I (`IDX_I:<sid>` quote key), not NSE_EQ -
        // otherwise the feed cache lookup never matches and the user's
        // "Indices" picks are silently dropped from the movers scan.
        univ.extend(index_legs(&indices));
        univ.sort();
        univ.dedup();
        // Stream the whole scan universe on the live feed so the Top Movers /
        // Top Losers re-rank from the websocket cache (fast) instead of a REST
        // round-trip on every 100ms pass. The first pass still REST-seeds.
        self.dhan.watch_options_now(&univ);
        let rows: Vec<Value> = self.quote_rows(&univ).await;
        let mut sorted = rows.clone();
        sorted.sort_by(|a, b| jf(b, "changePct").partial_cmp(&jf(a, "changePct")).unwrap_or(std::cmp::Ordering::Equal));
        let gainers: Vec<Value> = sorted.into_iter().take(gain_n.max(0) as usize).collect();
        let mut sorted_l = rows.clone();
        sorted_l.sort_by(|a, b| jf(a, "changePct").partial_cmp(&jf(b, "changePct")).unwrap_or(std::cmp::Ordering::Equal));
        let losers: Vec<Value> = sorted_l.into_iter().take(lose_n.max(0) as usize).collect();
        let up = rows.iter().filter(|r| jf(r, "changePct") > 0.0).count() as i64;
        let down = rows.iter().filter(|r| jf(r, "changePct") < 0.0).count() as i64;
        // Auto CE/PE side (old AST `autoRunInSide` parity): classify the ACTUAL
        // traded universe - the operator's selected top gainers / top losers -
        // not whole-market breadth. Gainers are bullish (CE), losers bearish
        // (PE), so switching "Top gainers" <-> "Top losers" (or setting one
        // count to 0) flips the auto side instead of it being pinned to the
        // market-wide up/down count. When both legs are selected the dominant
        // one wins; an exact tie falls back to market breadth.
        let want_g = gain_n > 0 && !gainers.is_empty();
        let want_l = lose_n > 0 && !losers.is_empty();
        let pos = gainers.iter().filter(|r| jf(r, "changePct") >= 0.0).count() as i64;
        let neg = losers.iter().filter(|r| jf(r, "changePct") < 0.0).count() as i64;
        let bias = movers_auto_bias(want_g, want_l, pos, neg, up, down);
        self.mover_bias.store(bias, Ordering::Relaxed);
        let payload = json!({ "ok": true, "at": now, "gainers": gainers, "losers": losers, "bias": bias, "up": up, "down": down });
        if let Ok(mut c) = self.movers_cache.lock() {
            *c = (now, payload);
        }
        // "Picked Strikes": each gainer resolves to its CE leg, each loser to PE.
        // Only the side the engine is actually trading is surfaced, so switching
        // gainers<->losers (or a trend/direction flip) replaces the previous
        // picks instead of leaving them beside the fresh ones. When no direction
        // is decided both legs are shown.
        let settings = self.doc().map(|d| d.settings.clone()).unwrap_or_default();
        // Same single direction source as the engine: with Filter-side routing on
        // there is no global side, so both gainer (CE) and loser (PE) rows show.
        let active = self.active_side(&settings);
        let mut legs: Vec<Value> = Vec::new();
        for (side, list) in [("CE", &gainers), ("PE", &losers)] {
            if active.is_some() && active != Some(side) {
                continue;
            }
            for r in list.iter() {
                let sid = ji(r, "securityId");
                // An operator index with an explicit leg is shown once, from the
                // dedicated indices pass below - never doubled up (possibly on
                // the opposite side) by its coincidental top-N ranking.
                if indices.contains(&sid) && movers_index_leg(&settings, sid).is_some() {
                    continue;
                }
                let spot = jf(r, "last");
                let Some((name, seg, inst)) = crate::market::symbol_meta(sid) else { continue };
                if let Some(leg) = self.pick_leg(&settings, sid, &name, &seg, &inst, spot, side) {
                    legs.push(json!({
                        "securityId": leg.security_id,
                        "underlying": name,
                        "side": side,
                        "tradingSymbol": leg.trading_symbol,
                        "segment": leg.exchange_segment,
                        "instrument": leg.instrument,
                        "spot": round2(spot),
                        "changePct": jf(r, "changePct"),
                        "source": "Top Movers",
                        "runMode": run_mode_for(&seg, &inst, &settings),
                        "underlyingSecurityId": sid,
                        "underlyingSegment": seg,
                        "underlyingInstrument": inst,
                    }));
                }
            }
        }
        // Operator indices: each resolves to its explicitly assigned leg only.
        // An index without an assigned leg contributes no pick (skipped), and the
        // assigned leg is shown regardless of the live auto direction.
        for id in &indices {
            let Some(side) = movers_index_leg(&settings, *id) else { continue };
            let Some((name, seg, inst)) = crate::market::symbol_meta(*id) else { continue };
            let spot = self.ltp_of(*id, &seg);
            let chg = rows
                .iter()
                .find(|r| ji(r, "securityId") == *id)
                .map(|r| jf(r, "changePct"))
                .unwrap_or(0.0);
            if let Some(leg) = self.pick_leg(&settings, *id, &name, &seg, &inst, spot, side) {
                legs.push(json!({
                    "securityId": leg.security_id,
                    "underlying": name,
                    "side": side,
                    "tradingSymbol": leg.trading_symbol,
                    "segment": leg.exchange_segment,
                    "instrument": leg.instrument,
                    "spot": round2(spot),
                    "changePct": chg,
                    "source": "Top Movers",
                    "runMode": run_mode_for(&seg, &inst, &settings),
                    "underlyingSecurityId": *id,
                    "underlyingSegment": seg,
                    "underlyingInstrument": inst,
                }));
            }
        }
        self.update_picked("Top Movers", legs);
    }

    async fn movers_readout(&self) -> Value {
        let (at, payload) = self.movers_cache.lock().map(|g| (g.0, g.1.clone())).unwrap_or((0, Value::Null));
        if payload.is_object() {
            payload
        } else {
            json!({ "ok": true, "at": at, "gainers": [], "losers": [], "bias": self.mover_bias.load(Ordering::Relaxed) })
        }
    }

    /// NIFTY trend pass #1 - REST-backed direction refresh. Computes every
    /// selected straight-line indicator on the NIFTY candles at the same
    /// timeframe the operator ticked for the charts (`1 min` / `5 min`; both =
    /// the two must agree) and applies the resulting direction. Throttled to 1s
    /// because it may round-trip REST to seed the live series; the tick-native
    /// [`refresh_nifty_flip`] then tracks the forming bar between refreshes.
    async fn refresh_nifty_trend(&self) {
        let now = now_ms();
        if now - self.last_trend.load(Ordering::Relaxed) < 2_500 {
            return;
        }
        let (enabled, tf, conf, slope, color) = self
            .doc()
            .map(|d| (d.settings.nifty_trend_on, nifty_timeframe(&d.settings), d.settings.nifty_trend_conf_inds.clone(), d.settings.nifty_trend_slope_len, d.settings.nifty_trend_color))
            .unwrap_or((false, "5min".into(), Vec::new(), 5, false));
        self.last_trend.store(now, Ordering::Relaxed);
        if !enabled {
            self.clear_nifty_direction();
            return;
        }
        // Colour mode re-reads the assigned indicators on the fixed colour scan
        // cadence, not on the 2.5s REST refresh.
        if color {
            if now - self.last_color_scan.load(Ordering::Relaxed) < COLOR_SCAN_MS {
                return;
            }
            self.last_color_scan.store(now, Ordering::Relaxed);
        }
        if !self.dhan.is_connected().await {
            return;
        }
        if let Some((bull, bear, net)) = self.resolve_nifty_direction(&tf, &conf, slope, color, true).await {
            self.apply_nifty_direction(bull, bear, net, &tf);
        }
    }

    /// NIFTY trend tick-native flip watchdog. Reads the live *forming* candle
    /// series (no REST) and re-applies the direction on every market tick, so a
    /// bullish<->bearish flip switches leg/strikes within milliseconds of the
    /// straight line turning - never on a candle close. Old trades are never
    /// closed by a switch.
    async fn refresh_nifty_flip(&self) {
        let now = now_ms();
        if now - self.last_nifty_flip.load(Ordering::Relaxed) < 20 {
            return;
        }
        self.last_nifty_flip.store(now, Ordering::Relaxed);
        let (enabled, tf, conf, slope, color) = self
            .doc()
            .map(|d| (d.settings.nifty_trend_on, nifty_timeframe(&d.settings), d.settings.nifty_trend_conf_inds.clone(), d.settings.nifty_trend_slope_len, d.settings.nifty_trend_color))
            .unwrap_or((false, "5min".into(), Vec::new(), 5, false));
        if !enabled {
            return;
        }
        // Colour mode: re-read the assigned indicators' colour every
        // COLOR_SCAN_MS instead of on every tick.
        if color {
            if now - self.last_color_scan.load(Ordering::Relaxed) < COLOR_SCAN_MS {
                return;
            }
            self.last_color_scan.store(now, Ordering::Relaxed);
        }
        if let Some((bull, bear, net)) = self.resolve_nifty_direction(&tf, &conf, slope, color, false).await {
            self.apply_nifty_direction(bull, bear, net, &tf);
        }
    }

    /// Resolve the net NIFTY direction for the requested timeframe. `allow_rest`
    /// lets a stale/absent live series trigger a REST seed; the tick watchdog
    /// passes `false` so it can never flood REST between refreshes. `both` needs
    /// the 1 min and 5 min to agree, otherwise the previous direction is kept.
    async fn resolve_nifty_direction(
        &self,
        tf: &str,
        conf: &[String],
        slope_len: i64,
        color: bool,
        allow_rest: bool,
    ) -> Option<(Vec<String>, Vec<String>, i64)> {
        if tf.eq_ignore_ascii_case("both") {
            let a = self.nifty_assignment_for("1min", conf, slope_len, color, allow_rest).await;
            let b = self.nifty_assignment_for("5min", conf, slope_len, color, allow_rest).await;
            match (a, b) {
                (Some(xa), Some(xb)) => {
                    if xa.2.signum() == xb.2.signum() {
                        Some(xb)
                    } else {
                        None
                    }
                }
                (Some(x), None) => Some(x),
                (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        } else {
            self.nifty_assignment_for(tf, conf, slope_len, color, allow_rest).await
        }
    }

    /// Assignment for one timeframe: `(bull_ids, bear_ids, net)` or `None` while
    /// the NIFTY candles are not warm enough for the straight-line engine.
    async fn nifty_assignment_for(
        &self,
        tf: &str,
        conf: &[String],
        slope_len: i64,
        color: bool,
        allow_rest: bool,
    ) -> Option<(Vec<String>, Vec<String>, i64)> {
        let c = if allow_rest {
            self.live_candles(NIFTY_SEC, "IDX_I", "INDEX", tf).await.ok()?
        } else {
            // Tick path: serve the live forming series only, accepting it up to
            // 15 min old so a quiet feed never triggers a REST storm.
            self.dhan
                .market
                .live_bars_for(NIFTY_SEC, tf, Duration::from_secs(900))?
        };
        if c.len() < 35 {
            return None;
        }
        Some(nifty_indicator_assignment(&c, conf, slope_len, color))
    }

    /// Publish a resolved direction: store the per-leg filter split, swap the
    /// sign, and on a change re-arm the strike scan + emit the flip signal/log.
    fn apply_nifty_direction(&self, bull: Vec<String>, bear: Vec<String>, net: i64, tf: &str) {
        let d = net.signum();
        let cur = self.nifty_dir.load(Ordering::Relaxed);
        let agree = if !bull.is_empty() && bear.is_empty() {
            1
        } else if bull.is_empty() && !bear.is_empty() {
            -1
        } else {
            0
        };
        if d == cur {
            // The live read agrees with the committed side. Refresh the strict-gate
            // state and the filter split from this read, and drop any in-flight
            // candidate - confirmation must be continuous, not cumulative.
            self.nifty_pending_dir.store(0, Ordering::Relaxed);
            self.nifty_raw_net.store(d, Ordering::Relaxed);
            self.nifty_agree.store(agree, Ordering::Relaxed);
            if d != 0 {
                if let Ok(mut b) = self.nifty_bull_filters.lock() {
                    *b = bull;
                }
                if let Ok(mut b) = self.nifty_bear_filters.lock() {
                    *b = bear;
                }
            }
            return;
        }
        if d == 0 {
            // A dead-heat carries no committed direction. Hold the previous side
            // and its gate state - a momentary tie must never free BOTH legs and
            // let the engine trade both directions. Only forget a pending flip.
            self.nifty_pending_dir.store(0, Ordering::Relaxed);
            return;
        }
        // The read disagrees with the committed side. The lines are read off the
        // live forming bar on every tick, so a single wiggle can net the opposite
        // sign for a few milliseconds; require the new direction to PERSIST before
        // it replaces the side, otherwise the leg/strikes flip CE<->PE too fast
        // for any entry to survive. From NEUTRAL (no side committed yet) commit at
        // once so startup is not delayed.
        let now = now_ms();
        let cand = self.nifty_pending_dir.load(Ordering::Relaxed);
        if cand != d {
            self.nifty_pending_dir.store(d, Ordering::Relaxed);
            self.nifty_pending_since.store(now, Ordering::Relaxed);
            if cur != 0 {
                return;
            }
        } else if now - self.nifty_pending_since.load(Ordering::Relaxed) < NIFTY_FLIP_CONFIRM_MS {
            return;
        }
        self.nifty_pending_dir.store(0, Ordering::Relaxed);
        if let Ok(mut b) = self.nifty_bull_filters.lock() {
            *b = bull;
        }
        if let Ok(mut b) = self.nifty_bear_filters.lock() {
            *b = bear;
        }
        self.nifty_raw_net.store(d, Ordering::Relaxed);
        self.nifty_agree.store(agree, Ordering::Relaxed);
        let prev = self.nifty_dir.swap(d, Ordering::Relaxed);
        if prev == d {
            return;
        }
        // Re-resolve the picked strikes on the new side immediately; the old
        // trades keep running (a switch never closes them).
        self.last_nifty_scan.store(0, Ordering::Relaxed);
        let seq = self.nifty_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let from = prev.signum();
        if let Ok(mut f) = self.nifty_flip.lock() {
            *f = json!({ "from": from, "to": d, "at": now, "seq": seq, "tf": tf });
        }
        self.log(
            "info",
            &format!(
                "NIFTY trend change {} -> {} on {} (straight-line confirm) - switching leg/strikes, old trades kept running",
                nifty_dir_label(from),
                nifty_dir_label(d),
                tf
            ),
        );
    }

    fn clear_nifty_direction(&self) {
        self.nifty_dir.store(0, Ordering::Relaxed);
        self.nifty_raw_net.store(0, Ordering::Relaxed);
        self.nifty_agree.store(0, Ordering::Relaxed);
        self.nifty_pending_dir.store(0, Ordering::Relaxed);
        // Force the next colour scan to run immediately when the trend (or its
        // colour mode) is switched back on.
        self.last_color_scan.store(0, Ordering::Relaxed);
        if let Ok(mut b) = self.nifty_bull_filters.lock() {
            b.clear();
        }
        if let Ok(mut b) = self.nifty_bear_filters.lock() {
            b.clear();
        }
    }

    /// NIFTY trend pass #2 - pick + assignment. Top Gainers are gated by the
    /// bullish straight-line filters and resolve to CE legs; Top Losers are
    /// gated by the bearish filters and resolve to PE legs. Both legs come from
    /// the Top Movers scanner lists - NIFTY trend never ranks its own universe,
    /// so with Top Movers off there is nothing to trade. When only one side is
    /// selected only that side trades; when both are selected both filter lists
    /// run, each strictly on its own side.
    async fn refresh_nifty_scan(&self) {
        let now = now_ms();
        if now - self.last_nifty_scan.load(Ordering::Relaxed) < 100 {
            return;
        }
        let (enabled, movers_on, settings) = self
            .doc()
            .map(|d| (d.settings.nifty_trend_on, d.settings.movers_on, d.settings.clone()))
            .unwrap_or((false, false, Settings::default()));
        if !enabled || !movers_on {
            self.update_picked("NIFTY trend", Vec::new());
            if let Ok(mut g) = self.nifty_picks.lock() {
                *g = (now, Value::Null);
            }
            return;
        }
        if !self.dhan.is_connected().await {
            return;
        }
        // Yield Dhan's single data slot while the operator is interacting; the
        // next engine tick retries (last_nifty_scan is deliberately untouched).
        if self.dhan.user_active() {
            return;
        }
        self.last_nifty_scan.store(now, Ordering::Relaxed);
        // Follow only the net straight-line direction: a bullish NIFTY builds CE
        // legs, a bearish NIFTY PE legs. The opposite side is never armed, even
        // when a single line reads against the majority.
        let dir = self.nifty_dir.load(Ordering::Relaxed);
        let bull_on = dir > 0 && self.nifty_bull_filters.lock().map(|g| !g.is_empty()).unwrap_or(false);
        let bear_on = dir < 0 && self.nifty_bear_filters.lock().map(|g| !g.is_empty()).unwrap_or(false);
        // Strict gate(s): a side that does not match the live assigned-line read
        // (net / all-agree) is dropped here too, so the Picked-Strikes readout
        // never lists a leg the engine would refuse to trade.
        let bull_on = bull_on && self.nifty_line_side_ok(&settings, true);
        let bear_on = bear_on && self.nifty_line_side_ok(&settings, false);
        // No committed direction (or no line assigned yet): nothing to assign.
        if !bull_on && !bear_on {
            self.update_picked("NIFTY trend", Vec::new());
            return;
        }
        // The tradeable universe is exactly the Top Movers scanner lists -
        // NIFTY trend follows the operator's selected Top Gainers / Top Losers.
        let payload = self.movers_cache.lock().map(|g| g.1.clone()).unwrap_or(Value::Null);
        let gainers: Vec<Value> = jarr(&payload, "gainers");
        let losers: Vec<Value> = jarr(&payload, "losers");
        let mut legs: Vec<Value> = Vec::new();
        for (side, list, active) in [("CE", &gainers, bull_on), ("PE", &losers, bear_on)] {
            if !active {
                continue;
            }
            for r in list.iter() {
                let sid = ji(r, "securityId");
                let spot = jf(r, "last");
                let Some((name, seg, inst)) = crate::market::symbol_meta(sid) else { continue };
                // NIFTY Trend Following applies to F&O stocks only: an index or
                // commodity underlying in the Top Movers lists is never picked.
                if category_for(&seg, &inst) != StratCat::Fno {
                    continue;
                }
                if let Some(leg) = self.pick_leg(&settings, sid, &name, &seg, &inst, spot, side) {
                    legs.push(json!({
                        "securityId": leg.security_id,
                        "underlying": name,
                        "side": side,
                        "tradingSymbol": leg.trading_symbol,
                        "segment": leg.exchange_segment,
                        "instrument": leg.instrument,
                        "spot": round2(spot),
                        "changePct": jf(r, "changePct"),
                        "source": "NIFTY trend",
                        "runMode": run_mode_for(&seg, &inst, &settings),
                        "underlyingSecurityId": sid,
                        "underlyingSegment": seg,
                        "underlyingInstrument": inst,
                    }));
                }
            }
        }
        self.update_picked("NIFTY trend", legs.clone());
        let payload = json!({
            "ok": true, "at": now, "dir": self.nifty_dir.load(Ordering::Relaxed),
            "bullFilters": self.nifty_bull_filters.lock().map(|g| g.clone()).unwrap_or_default(),
            "bearFilters": self.nifty_bear_filters.lock().map(|g| g.clone()).unwrap_or_default(),
            "gainers": gainers, "losers": losers, "picks": legs,
        });
        if let Ok(mut g) = self.nifty_picks.lock() {
            *g = (now, payload);
        }
        // A picked stock that stops qualifying is dropped immediately: close any
        // open position originally tagged as a NIFTY-trend pick. Positions opened
        // under a different direction (i.e. before a bull/bear flip) are left
        // running - a switch never closes the old trades.
        let keep = self.nifty_pick_underlyings();
        let cur_dir = self.nifty_dir.load(Ordering::Relaxed);
        let drops: Vec<String> = self
            .doc()
            .map(|d| {
                d.positions
                    .iter()
                    .filter(|p| {
                        jb(p, "niftyPick")
                            && ji(p, "niftyDir") == cur_dir
                            && !keep.contains(&js(p, "underlying").to_uppercase())
                    })
                    .map(|p| js(p, "id"))
                    .collect()
            })
            .unwrap_or_default();
        for id in drops {
            let ltp = self.position_ltp(&id);
            let _ = self.close_position(&id, "nifty_pick_drop", ltp).await;
        }
    }

    async fn nifty_readout(&self) -> Value {
        let (at, payload) = self.nifty_picks.lock().map(|g| (g.0, g.1.clone())).unwrap_or((0, Value::Null));
        if payload.is_object() {
            payload
        } else {
            json!({ "ok": true, "at": at, "dir": self.nifty_dir.load(Ordering::Relaxed), "picks": [] })
        }
    }

    /// Underlyings currently picked by the NIFTY-trend scanner, used to tag the
    /// positions it opened (and to close a pick the moment it drops out).
    fn nifty_pick_underlyings(&self) -> Vec<String> {
        let payload = self.nifty_picks.lock().map(|g| g.1.clone()).unwrap_or(Value::Null);
        jarr(&payload, "picks")
            .iter()
            .map(|p| js(p, "underlying").to_uppercase())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Resolve the direction-specific AST template (if assigned) and return its
    /// saved settings, which override the manual gate/filter set for this entry.
    fn template_for_direction(&self, settings: &Settings, strat: &Strategy) -> Option<Settings> {
        let mut name = String::new();
        if per_strategy_side(settings) {
            // Per-strategy side routing: the strategy's own side selects the
            // template, never the Top-Movers bias (which could disagree with the
            // routed leg).
            name = if strategy_is_bull(strat) {
                settings.bull_template.clone()
            } else {
                settings.bear_template.clone()
            };
        } else if assigned_leg(settings, strat).is_none()
            && settings.movers_on
            && !settings.manual_strikes_enabled
        {
            let b = self.mover_bias.load(Ordering::Relaxed);
            if b > 0 {
                name = settings.mover_bull_template.clone();
            } else if b < 0 {
                name = settings.mover_bear_template.clone();
            }
        }
        if name.is_empty() {
            name = if strategy_is_bull(strat) {
                settings.bull_template.clone()
            } else {
                settings.bear_template.clone()
            };
        }
        if name.is_empty() {
            return None;
        }
        let v = self.doc()?.templates.get(&name).cloned()?;
        serde_json::from_value::<Settings>(v.get("settings")?.clone()).ok()
    }

    // -----------------------------------------------------------------------
    // Scanner-driven instruments (Top Movers / MCX commodities)
    // -----------------------------------------------------------------------
    //
    // The scanners decide *what* to trade; the indicator-filter gate decides
    // *when*. Each picked instrument is expanded into a synthetic strategy per
    // side (CE = bullish / PE = bearish) so the normal scan loop runs it through
    // the exact same gate, Dhan option resolution and execution path as a saved
    // strategy - this is the old AST "Indicator filter based trades" behaviour.

    /// Resolve the option leg a scanner pick would execute at `spot` for an
    /// explicit `side` (CE/PE), using the operator's strike preferences. Powers
    /// the "Picked Strikes" readout.
    fn pick_leg(&self, settings: &Settings, sid: i64, name: &str, seg: &str, inst: &str, spot: f64, side: &str) -> Option<Strategy> {
        let side = if side.eq_ignore_ascii_case("PE") { "PE" } else { "CE" };
        let bull = side == "CE";
        let mut s2 = settings.clone();
        s2.run_in_enabled = false;
        // The readout lists each pick's own side explicitly, so keep the
        // per-strategy side routing from collapsing both rows onto one side.
        s2.filter_side_route = false;
        s2.option_side = side.to_string();
        let base = Strategy {
            id: format!("pick:{}:{side}", sid),
            name: format!("{name} [{side}]"),
            category: if bull { "BULLISH".into() } else { "BEARISH".into() },
            timeframe: String::new(),
            security_id: sid,
            exchange_segment: seg.to_string(),
            instrument: inst.to_string(),
            trading_symbol: name.to_string(),
            side: "BUY".into(),
            enabled: true,
            conditions: Vec::new(),
            last_signal: 0,
            last_error: String::new(),
            synthetic: true,
            lot: 0.0,
            group: String::new(),
            manual: false,
        };
        self.resolve_option_strategy(&base, spot, &s2, true)
    }

    /// CE/PE side a scanner pick resolves to from the live NIFTY direction /
    /// "Run Strategy In" preference (defaults to CE when neither is decided).
    fn pick_side(&self, settings: &Settings) -> String {
        self.active_side(settings).unwrap_or("CE").to_string()
    }

    /// Replace the "Picked Strikes" rows belonging to one scanner `source`,
    /// keeping the other scanner's rows intact (Top Movers feeds this readout).
    fn update_picked(&self, source: &str, rows: Vec<Value>) {
        if let Ok(mut g) = self.picked_strikes.lock() {
            g.1.retain(|r| js(r, "source") != source);
            g.1.extend(rows);
            g.0 = now_ms();
        }
    }

    /// Resolve a Manual Strike Select pick to its concrete option contract
    /// (security id, trading symbol, execution segment, instrument). Re-read from
    /// the scrip master on every pass so a rolled/re-issued contract id is never
    /// stale.
    fn manual_contract(&self, m: &ManualStrike) -> Option<(i64, String, String, String)> {
        let sc = scrip::get()?;
        let ot = m.option_type.to_uppercase();
        if !(ot == "CE" || ot == "PE") || m.strike <= 0.0 {
            return None;
        }
        let res = sc.resolve(&m.symbol_name, &m.expiry, m.strike, &ot, &m.exchange_segment)?;
        let exch = scrip::scrip_exch(&m.exchange_segment);
        let seg = fno_segment(exch).to_string();
        let prefix = scrip::fno_underlying(&m.symbol_name);
        let inst = if scrip::is_index_prefix(&prefix) { "OPTIDX" } else { "OPTSTK" }.to_string();
        Some((res.security_id, res.trading_symbol, seg, inst))
    }

    /// "Picked Strikes" rows for Manual Strike Select: the operator's explicit
    /// contracts, in the exact shape the scanner readout uses. The snapshot feeds
    /// these into the UI `strikes` array, so Picked Strikes - and the Running view
    /// - show the manual selection and nothing else while the mode is on.
    fn manual_picked_rows(&self, settings: &Settings) -> Vec<Value> {
        let mut rows: Vec<Value> = Vec::new();
        for m in &settings.manual_strikes {
            let Some((sid, symbol, seg, inst)) = self.manual_contract(m) else { continue };
            let (underlying, useg, uinst) = crate::market::symbol_meta(m.security_id)
                .unwrap_or_else(|| (m.symbol_name.clone(), m.exchange_segment.clone(), String::new()));
            rows.push(json!({
                "securityId": sid,
                "underlying": underlying,
                "side": m.option_type.to_uppercase(),
                "tradingSymbol": symbol,
                "segment": seg,
                "instrument": inst,
                "spot": 0.0,
                "changePct": 0.0,
                "strike": m.strike,
                "expiry": m.expiry,
                "source": "Manual",
                "runMode": run_mode_for(&seg, &inst, settings),
                "underlyingSecurityId": m.security_id,
                "underlyingSegment": useg,
                "underlyingInstrument": uinst,
            }));
        }
        rows
    }

    /// In Manual Strike Select the Picked Strikes readout is the manual list, not
    /// the scanner resolution. Replaces every row instead of merging a source.
    fn refresh_manual_picked(&self) {
        let settings = match self.doc() {
            Some(d) => d.settings.clone(),
            None => return,
        };
        let rows = self.manual_picked_rows(&settings);
        if let Ok(mut g) = self.picked_strikes.lock() {
            g.1 = rows;
            g.0 = now_ms();
        }
    }

    /// Remember the option-premium leg the engine resolved for a running
    /// strategy. `kind` is `"run"` (the chart the entry conditions are evaluated
    /// on) or `"trade"` (the chart the order executes on). The Running
    /// Strategies view reads these so it can show - and open - the exact chart
    /// each strategy is running on instead of only the picked strikes.
    fn record_strat_leg(&self, id: &str, kind: &str, s: &Strategy) {
        if id.is_empty() || s.security_id <= 0 {
            return;
        }
        if let Ok(mut m) = self.strat_legs.lock() {
            let e = m.entry(id.to_string()).or_insert_with(|| json!({}));
            if let Some(o) = e.as_object_mut() {
                o.insert(
                    kind.to_string(),
                    json!({
                        "securityId": s.security_id,
                        "tradingSymbol": s.trading_symbol,
                        "segment": s.exchange_segment,
                        "instrument": s.instrument,
                    }),
                );
            }
        }
    }

    fn scanner_targets(&self, settings: &Settings) -> Vec<Strategy> {
        let (bull_side, bear_side) = if settings.premium_only {
            // "Premium chart only": both sides gate on the bullish filter set, so
            // a side is tradeable whenever ANY indicator filter is enabled. The
            // operator's selected side still narrows this via `side_allowed`.
            let any = settings
                .filters
                .iter()
                .any(|(k, v)| *v && (filter_is_bull(k) || filter_is_bear(k)));
            (any, any)
        } else {
            (
                settings.filters.iter().any(|(k, v)| *v && filter_is_bull(k)),
                settings.filters.iter().any(|(k, v)| *v && filter_is_bear(k)),
            )
        };
        // NIFTY-trend assignment: the bullish straight-line filters gate the Top
        // Gainer (CE) legs, the bearish ones the Top Loser (PE) legs. The engine
        // follows ONLY the net straight-line direction, so a bullish NIFTY runs
        // CE legs and a bearish NIFTY runs PE legs - never both at once. A single
        // dissenting line must not arm the opposite side; a tie (net 0) has no
        // committed direction, so no NIFTY-trend leg runs.
        let nifty_dir = self.nifty_dir.load(Ordering::Relaxed);
        let nifty_allow_bull = settings.nifty_trend_on
            && nifty_dir > 0
            && self.nifty_bull_filters.lock().map(|g| !g.is_empty()).unwrap_or(false);
        let nifty_allow_bear = settings.nifty_trend_on
            && nifty_dir < 0
            && self.nifty_bear_filters.lock().map(|g| !g.is_empty()).unwrap_or(false);
        if !bull_side && !bear_side && !nifty_allow_bull && !nifty_allow_bear {
            return Vec::new();
        }
        let active = self.active_side(settings);
        let (allow_bull, allow_bear) = side_allowed(bull_side, bear_side, active);
        if !allow_bull && !allow_bear && !nifty_allow_bull && !nifty_allow_bear {
            return Vec::new();
        }

        fn add(
            out: &mut Vec<Strategy>,
            seen: &mut std::collections::HashSet<String>,
            sid: i64,
            force: Option<bool>,
            allow_bull: bool,
            allow_bear: bool,
        ) {
            if sid <= 0 {
                return;
            }
            let Some((name, seg, inst)) = crate::market::symbol_meta(sid) else { return };
            let lot = crate::scrip::get()
                .and_then(|s| s.lot_for(&name, &seg).map(|(l, _)| l))
                .unwrap_or(0.0);
            let mk = |out: &mut Vec<Strategy>, seen: &mut std::collections::HashSet<String>, bull: bool| {
                let key = format!("scan:{}:{}", sid, if bull { "CE" } else { "PE" });
                if !seen.insert(key.clone()) {
                    return;
                }
                out.push(Strategy {
                    id: key,
                    name: format!("{} [scan {}]", name, if bull { "bullish" } else { "bearish" }),
                    category: if bull { "BULLISH".into() } else { "BEARISH".into() },
                    timeframe: String::new(),
                    security_id: sid,
                    exchange_segment: seg.clone(),
                    instrument: inst.clone(),
                    trading_symbol: name.clone(),
                    side: "BUY".into(),
                    enabled: true,
                    conditions: Vec::new(),
                    last_signal: 0,
                    last_error: String::new(),
                    synthetic: true,
                    lot,
                    group: String::new(),
                    manual: false,
                });
            };
            match force {
                Some(true) => {
                    if allow_bull {
                        mk(out, seen, true);
                    }
                }
                Some(false) => {
                    if allow_bear {
                        mk(out, seen, false);
                    }
                }
                None => {
                    if allow_bull {
                        mk(out, seen, true);
                    }
                    if allow_bear {
                        mk(out, seen, false);
                    }
                }
            }
        }

        let mut out: Vec<Strategy> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Manual Strike Select (testing): the tradeable universe is the operator's
        // explicit picks instead of the Top Movers / NIFTY-trend / commodity
        // scanners. Each picked underlying becomes a synthetic strategy on the
        // side(s) the operator chose, so the ticked Indicator filters gate entries
        // on the manual strikes exactly like the normal Indicator-filters mode -
        // the engine still only ever executes the manual contracts.
        if settings.manual_strikes_enabled {
            for (sid, has_ce, has_pe) in manual_target_specs(&settings.manual_strikes) {
                if has_ce {
                    add(&mut out, &mut seen, sid, Some(true), allow_bull, allow_bear);
                }
                if has_pe {
                    add(&mut out, &mut seen, sid, Some(false), allow_bull, allow_bear);
                }
            }
            return out;
        }

        // Commodities (MCX): the operator's list, else the default near-month set.
        if settings.commodity_on {
            let rows = crate::market::commodity_rows();
            let rows: Vec<_> = if settings.commodity_list.is_empty() {
                rows
            } else {
                rows.into_iter()
                    .filter(|c| settings.commodity_list.contains(&c.security_id))
                    .collect()
            };
            for c in rows {
                // Operator-assigned leg (CE/PE) is authoritative: only that leg
                // runs and it bypasses the global side gate, so the ticked
                // indicator filters for that side (CE = bullish, PE = bearish)
                // are the whole entry rule. A commodity with no assignment keeps
                // the old global-side behaviour.
                if let Some(bull) = commodity_target_side(settings, c.security_id) {
                    add(&mut out, &mut seen, c.security_id, Some(bull), true, true);
                } else {
                    add(&mut out, &mut seen, c.security_id, None, allow_bull, allow_bear);
                }
            }
        }

        // Top Movers: gainers are bullish, losers bearish, plus operator indices.
        // When the NIFTY-trend strict gate(s) are ON the forced gainer/loser legs
        // are additionally bound to the assigned straight-line direction: a Top
        // Gainer (CE) leg is dropped unless the lines confirm bullish, a Top Loser
        // (PE) leg unless they confirm bearish. Operator-assigned index legs are
        // explicit picks, so they are left untouched.
        if settings.movers_on {
            let payload = self.movers_cache.lock().map(|g| g.1.clone()).unwrap_or(Value::Null);
            // Once NIFTY Trend Following has committed a direction it OWNS this
            // universe's side: a bullish NIFTY must arm the Top Gainer (CE) legs
            // even when the Top-Movers auto bias happens to point PE (a down day
            // with the index line rising), and vice versa. Otherwise the auto
            // bias's global side gate silently cancelled the NIFTY call and the
            // "bullish CE / Top Gainer" legs never ran. With no committed NIFTY
            // direction the old global side gate applies unchanged.
            let (mv_allow_bull, mv_allow_bear) = match self.nifty_dir.load(Ordering::Relaxed) {
                d if settings.nifty_trend_on && d > 0 => (true, false),
                d if settings.nifty_trend_on && d < 0 => (false, true),
                _ => (allow_bull, allow_bear),
            };
            if mv_allow_bull && self.nifty_line_side_ok(settings, true) {
                for r in jarr(&payload, "gainers") {
                    add(&mut out, &mut seen, ji(&r, "securityId"), Some(true), true, true);
                }
            }
            if mv_allow_bear && self.nifty_line_side_ok(settings, false) {
                for r in jarr(&payload, "losers") {
                    add(&mut out, &mut seen, ji(&r, "securityId"), Some(false), true, true);
                }
            }
            for id in &settings.movers_indices {
                // Only the operator-assigned leg runs; an index with no leg is
                // skipped (index trading needs an explicit trend call). The
                // assigned leg is authoritative, so it bypasses the global side
                // gates (both allow flags are passed as true).
                if let Some(bull) = index_target_side(settings, *id) {
                    add(&mut out, &mut seen, *id, Some(bull), true, true);
                }
            }
        }

        // NIFTY trend: Top Gainers (CE) gated by the bullish straight-line
        // filters, Top Losers (PE) by the bearish ones. Both sides run together
        // when both lists are assigned. Only F&O-stock underlyings ever run - an
        // index or commodity pick is dropped here (NIFTY Trend Following applies
        // to F&O stocks only).
        if settings.nifty_trend_on {
            let payload = self.nifty_picks.lock().map(|g| g.1.clone()).unwrap_or(Value::Null);
            for p in jarr(&payload, "picks") {
                let sid = ji(&p, "underlyingSecurityId");
                if sid <= 0 {
                    continue;
                }
                let Some((_, seg, inst)) = crate::market::symbol_meta(sid) else { continue };
                if category_for(&seg, &inst) != StratCat::Fno {
                    continue;
                }
                if js(&p, "side").eq_ignore_ascii_case("CE") {
                    if nifty_allow_bull && self.nifty_line_side_ok(settings, true) {
                        add(&mut out, &mut seen, sid, Some(true), true, true);
                    }
                } else if nifty_allow_bear && self.nifty_line_side_ok(settings, false) {
                    add(&mut out, &mut seen, sid, Some(false), true, true);
                }
            }
        }

        out
    }

    // -----------------------------------------------------------------------
    // Signal scan
    // -----------------------------------------------------------------------

    async fn scan_signals(&self) {
        // Entries only while Dhan is connected AND the live feed is streaming a
        // fresh tick: a silent / closed / disconnected feed must not trade on a
        // stale mark (which would produce fake results).
        if !self.dhan.is_connected().await || !self.dhan.feed_live() {
            return;
        }
        let (mut strategies, selected, trade_limit, trade_limit_count, ai_trades, movers_on, settings0) = {
            let Some(d) = self.doc() else { return };
            if !d.strategy_time_ok() {
                return;
            }
            (
                d.strategies.clone(),
                d.selected.clone(),
                d.settings.trade_limit,
                d.settings.trade_limit_count.max(0),
                d.settings.ai_trades,
                d.settings.movers_on,
                d.settings.clone(),
            )
        };
        // Scanner-picked instruments (Top Movers / MCX commodities)
        // join the run set as synthetic strategies so the indicator-filter gate +
        // Dhan execution path below drives real trades for them too.
        strategies.extend(self.scanner_targets(&settings0));
        let commodity_on = settings0.commodity_on;
        // A strategy runs when it is enabled in the Saved Strategies table, OR it
        // is ticked into the AST run set (`selected`, also fed by AI auto-pick)
        // and "Call manually selected strategies" is on, OR it is one of the
        // live top-N AI trader picks while "Smart AI trader picked strategies"
        // is on (old AST `activeStrategies()` parity - AI picks keep updating
        // every scan instead of only at Fetch time).
        let ai_picks: std::collections::BTreeSet<String> =
            if settings0.ai_pick { self.doc().map(|d| ai_pick_ids(&d).into_iter().collect()).unwrap_or_default() } else { Default::default() };
        let call_manual = settings0.call_manual;
        // Run mode (old AST run-mode section): Normal mode runs the ticked /
        // enabled / AI-picked strategies (their own entry conditions, with the
        // indicator filters acting as extra gates). Indicator-filters mode runs
        // ONLY the scanner-universe synthetic strategies - their entry rule is
        // the ticked indicator filters, gated by majority (or strict AND / AI
        // Brain when those are enabled). The two
        // sets are mutually exclusive, exactly like the old engine.
        let filter_mode = settings0.filter_mode;
        // A strategy runs when enabled / manually ticked / AI-picked, and the
        // "Research stream" scoping (when any group is ticked on its side) allows
        // its group. Synthetic scanner rows have group "other" and always pass.
        let in_run = |s: &Strategy| {
            s.synthetic == filter_mode
                && (s.enabled || (call_manual && *selected.get(&s.id).unwrap_or(&false)) || ai_picks.contains(&s.id))
                && stream_allows(&settings0, s)
        };
        let eligible = strategies
            .iter()
            .filter(|s| in_run(s) && s.security_id > 0 && (!s.conditions.is_empty() || s.synthetic))
            .count();
        if eligible == 0 && !movers_on && !commodity_on {
            self.log_throttled(
                "no-instruments",
                15_000,
                "warn",
                "No tradeable instruments resolved - open a chart symbol, enable Top Movers or Commodities, and tick at least one indicator filter",
            );
        }
        self.log_throttled(
            "scan-diag",
            15_000,
            "info",
            &format!("scan pass: {} strategies, {} eligible, filter_mode={} movers={} comm={}", strategies.len(), eligible, filter_mode, movers_on, commodity_on),
        );
        // Per-strategy scan gap. When Engine Scan is active the outer engine loop
        // already gates the whole pass (fixed interval or bar close), so the
        // per-strategy throttle is disabled (0) and every eligible strategy is
        // evaluated on that pass. With Engine Scan off, keep the historical
        // 100ms per-strategy cadence.
        let scan_gap_ms: i64 = if settings0.scan_interval_on {
            0
        } else {
            100
        };
        for mut strat in strategies {
            if !in_run(&strat) || strat.security_id <= 0 || (strat.conditions.is_empty() && !strat.synthetic) {
                continue;
            }
            let order_per_sec = self
                .doc()
                .map(|d| order_per_sec_budget(d.settings.order_per_sec))
                .unwrap_or(6);
            let settings = self.doc().map(|d| d.settings.clone()).unwrap_or_default();
            // Engine controls: the 1min/5min checkboxes (or the strategy's own TF
            // when "Use AST settings" is off) and Multi-TF confirm choose the run
            // chart's timeframe. Override the strategy's stored TF so every
            // downstream fetch (spot, option premium, AI ATR SL) uses it.
            let mtf_tfs = if settings.mtf { mtf_pair(&settings) } else { None };
            strat.timeframe = match &mtf_tfs {
                Some((entry, _)) => entry.clone(),
                None => engine_tf(&settings, &strat),
            };
            // Strict NIFTY straight-line direction lock: while NIFTY Trend
            // Following has committed a direction, ONLY that side may trade. A
            // strategy whose bullish/bearish side disagrees with the locked side
            // is skipped outright, independent of the "Overall Bullish/Bearish"
            // toggle and of any Run-in override - so a bullish NIFTY can never
            // open a PE trade (and vice versa).
            if let Some(locked) = self.nifty_locked_side(&strat, &settings) {
                if strategy_is_bull(&strat) != (locked == "CE") {
                    continue;
                }
            }
            // Multi-position support: count the strategy's open positions, kept
            // for the legacy single-position fallback. The old hard stop ("never
            // open a second position for the same strategy") is gone - the entry
            // gate further down decides how many concurrent positions a strategy
            // may hold, capped only by the global orders/sec limit.
            let running = self
                .doc()
                .map(|d| d.positions.iter().filter(|p| js(p, "strategyId") == strat.id).count())
                .unwrap_or(0);
            let now = now_ms();
            // Tick-native: every strategy is evaluated on the LIVE forming bar at
            // the engine's ultrafast cadence. There is no closed-bar wait and no
            // bar-grid self-throttle any more, so a signal fires on the very tick
            // it appears instead of at the next bar close.
            {
                let last = self.last_sig.lock().ok().and_then(|m| m.get(&strat.id).copied()).unwrap_or(0);
                if now - last < scan_gap_ms {
                    continue;
                }
                if let Ok(mut m) = self.last_sig.lock() {
                    m.insert(strat.id.clone(), now);
                }
            }
            let candles = match self
                .live_candles(strat.security_id, &strat.exchange_segment, &strat.instrument, &strat.timeframe)
                .await
            {
                Ok(c) if c.len() >= 3 => c,
                Ok(c) => {
                    if strat.synthetic {
                        self.log_throttled(&format!("nofetch:{}", strat.id), 60_000, "warn", &format!("scan: only {} candles for {} [{}]", c.len(), strat.name, strat.timeframe));
                    }
                    continue;
                }
                Err(e) => {
                    if strat.synthetic {
                        self.log_throttled(&format!("nofetch:{}", strat.id), 60_000, "warn", &format!("scan: candle fetch failed for {}: {e}", strat.name));
                    }
                    continue;
                }
            };
            // Live forming bar: offset 0 is the bar currently being built from
            // the tick stream, so a rule reacts to the moving price itself - never
            // to a bar that had to close first.
            let offset = 0usize;
            // Overall Bullish/Bearish idea: when on, only the overall direction's
            // side is traded (NIFTY trend / top-movers decide the overall side).
            // An operator-assigned index/commodity leg is authoritative, so it is
            // never overridden by the overall direction.
            if settings.overall_dir && assigned_leg(&settings, &strat).is_none() {
                if let Some(side) = self.active_side(&settings) {
                    let want_bull = side == "CE";
                    if strategy_is_bull(&strat) != want_bull {
                        continue;
                    }
                }
            }
            // "Strategy should be run in": spot chart (the strategy's own
            // instrument), the resolved option-premium chart, or both (dual
            // confirmation). Every evaluated chart must pass before entry.
            let run_mode = run_mode_of(&strat, &settings);
            let want_spot = run_mode != "premium";
            // `futures` is the commodity near-month contract, i.e. its own spot
            // chart (old app maps futures -> spot), never the option premium.
            let want_premium = run_mode != "spot" && run_mode != "futures";
            let mut eval: Vec<(Strategy, Vec<Candle>)> = Vec::new();
            if want_spot {
                // "Run Strategy In: Spot chart" for an option-instrument strategy
                // must analyse the UNDERLYING spot chart, not the option's own
                // premium chart. Fall back to the strategy's own candles if the
                // underlying cannot be resolved.
                if is_option_leg(&strat) {
                    if let Some(pair) = self.underlying_candles(&strat).await {
                        self.record_strat_leg(&strat.id, "run", &pair.0);
                        eval.push(pair);
                    } else {
                        self.record_strat_leg(&strat.id, "run", &strat);
                        eval.push((strat.clone(), candles.clone()));
                    }
                } else {
                    self.record_strat_leg(&strat.id, "run", &strat);
                    eval.push((strat.clone(), candles.clone()));
                }
            }
            if want_premium {
                if let Some(pair) = self.premium_candles(&strat, &settings).await {
                    eval.push(pair);
                } else {
                    let side = self.desired_option_side(&settings, &strat);
                    if run_mode == "premium" {
                        self.log_throttled(
                            &format!("skip-premium:{}:{side}", strat.id),
                            60_000,
                            "warn",
                            &format!(
                                "No {side} option contracts for {} - skipping (Run Strategy In {side})",
                                strat.name
                            ),
                        );
                        continue;
                    }
                    if run_mode == "both" {
                        self.log_throttled(
                            &format!("skip-both:{}:{side}", strat.id),
                            60_000,
                            "warn",
                            &format!(
                                "No {side} option contracts for {} - skipping (dual run needs spot + premium)",
                                strat.name
                            ),
                        );
                    }
                }
            }
            if eval.is_empty() {
                continue;
            }
            let mut all_pass = true;
            let synth = strat.synthetic;
            let mut gate_settings = self.template_for_direction(&settings, &strat).unwrap_or_else(|| settings.clone());
            // "Premium chart only" is an engine-wide switch: a direction template
            // must not silently turn the bullish-filter-only rule back off.
            gate_settings.premium_only = settings.premium_only;
            for (s, c) in &eval {
                // One memo scope per leg: conditions, indicator gate, direction
                // guard and fresh-meet all read the same candle slice, so every
                // shared indicator is derived once instead of once per check.
                // The scope drops at the end of this iteration (no await inside),
                // so it can never outlive the borrowed candles.
                let _gate_scope = IndScope::enter();
                // Synthetic scanner instruments carry no entry conditions of their
                // own - the ticked indicator filters are the whole entry rule.
                let cond_ok = synth || conditions_met(&s.conditions, c, offset);
                let fg = filter_gate(&gate_settings, s, c, offset);
                let slc = strict_line_gate_ok(&gate_settings, s, c, offset);
                let dg = direction_opposite(&gate_settings, s, c, offset);
                if !cond_ok || !fg || !slc || dg {
                    if strat.synthetic {
                        let bull_s = gate_settings.premium_only || strategy_is_bull(s);
                        let keys: Vec<String> = gate_settings
                            .filters
                            .iter()
                            .filter(|(k, v)| **v && !is_stream_flag(k) && (if bull_s { filter_is_bull(k) } else { filter_is_bear(k) }))
                            .map(|(k, _)| k.clone())
                            .collect();
                        // Arrow filters are OR-ed into one trigger, so list them as a
                        // group instead of reporting each arrow as an individual fail.
                        let arrow_on = keys.iter().any(|k| is_arrow_flag(k));
                        let arrow_pass = arrow_on
                            && keys.iter().any(|k| {
                                is_arrow_flag(k) && filter_eval(k, c, offset).unwrap_or(false)
                            });
                        let failed: Vec<String> = keys
                            .iter()
                            .filter(|k| !is_arrow_flag(k) && filter_eval(k, c, offset) == Some(false))
                            .cloned()
                            .collect();
                        let gate_expl = filter_gate_explain(&gate_settings, s, c, offset);
                        let dir_expl = if gate_settings.dir_guard { "on" } else { "off" };
                        self.log_throttled(
                            &format!("gate:{}", strat.id),
                            60_000,
                            "info",
                            &format!("scan gate fail {}: cond={cond_ok} filter={fg} line={slc} dir={dg} overallDir={} dirGuard={dir_expl} arrow=[{arrow_on}/{arrow_pass}] gate=[{gate_expl}] keys={keys:?} failed={failed:?}", strat.name, settings.overall_dir),
                        );
                    }
                    all_pass = false;
                    break;
                }
            }
            if !all_pass {
                if self.end_signal_if_due(&strat.id) {
                    self.clear_signal_edge(&strat.id);
                }
                continue;
            }
            // Spot forming-candle colour gate: a Top Gainer (bullish) leg may enter
            // only while the spot chart's live forming candle is GREEN, a Top Loser
            // (bearish) leg only while it is RED. The signal is HELD (not consumed)
            // while the candle is the wrong colour, so a still-met condition enters
            // as soon as the colour flips. Runs only when the entry rule is on the
            // spot chart and the checkbox is on.
            if let Some(reason) = self.spot_candle_gate_skip(&settings, &strat).await {
                self.log_throttled(
                    &format!("candlegate:{}", strat.id),
                    10_000,
                    "info",
                    &format!("skip entry {}: {reason}", strat.name),
                );
                if self.end_signal_if_due(&strat.id) {
                    self.clear_signal_edge(&strat.id);
                }
                continue;
            }
            // Multi-TF confirm: the higher ticked timeframe must also satisfy the
            // same gate (old `evalEntryMtfLive` trend leg) before the entry fires.
            if let Some((_, trend_tf)) = &mtf_tfs {
                for (s, _) in &eval {
                    let tc = match self
                        .live_candles(s.security_id, &s.exchange_segment, &s.instrument, trend_tf)
                        .await
                    {
                        Ok(c) if c.len() >= 3 => c,
                        _ => {
                            all_pass = false;
                            break;
                        }
                    };
                    if !(synth || conditions_met(&s.conditions, &tc, offset))
                        || !filter_gate(&gate_settings, s, &tc, offset)
                        || !strict_line_gate_ok(&gate_settings, s, &tc, offset)
                        || direction_opposite(&gate_settings, s, &tc, offset)
                    {
                        all_pass = false;
                        break;
                    }
                }
                if !all_pass {
                    if self.end_signal_if_due(&strat.id) {
                        self.clear_signal_edge(&strat.id);
                    }
                    continue;
                }
            }
            // The whole gate (spot + premium + multi-TF) held on this scan: the
            // signal is still running, so cancel any pending signal-end debounce.
            self.note_signal_pass(&strat.id);
            // Multi-position entry gate (replaces the old single-position rule).
            // Always-on stacks a position every scan while the gate holds; fresh
            // fires only on the false -> true edge, so a gate that stays true
            // opens exactly one. Concurrent positions are unlimited; the
            // orders/sec cap below still bounds how fast fills go out. With both
            // toggles off the legacy "one open position per strategy" applies.
            let entry_allowed = if settings.multi_always_on {
                true
            } else if settings.multi_fresh_on {
                !self.sig_state_is_set(&strat.id)
            } else {
                running == 0
            };
            if !entry_allowed {
                continue;
            }
            // "Trade should be executed in": spot keeps the strategy's own
            // tradeable instrument; anything else executes the resolved option
            // premium leg. Index+spot has no directly tradeable spot, so it
            // falls through to the premium leg (same as the old engine).
            let trade_mode = trade_mode_of(&strat, &settings);
            let premium_leg = eval
                .iter()
                .rev()
                .find(|(s, _)| s.instrument.eq_ignore_ascii_case("OPTIDX") || s.exchange_segment.eq_ignore_ascii_case("NSE_FNO"))
                .map(|(s, _)| s.clone());
            let exec_strat = if trade_mode == "spot" && strat_category(&strat) != StratCat::Index {
                strat.clone()
            } else if let Some(p) = premium_leg {
                p
            } else {
                // Run mode evaluated only the spot chart, but execution targets the
                // option premium: resolve the contract now. The old engine resolves
                // the execution leg regardless of which chart the entry rule ran on;
                // trading the raw spot would book F&O-sized quantity at full notional
                // and trip the margin gate.
                let mut spot = self.ltp_of(strat.security_id, &strat.exchange_segment);
                if spot <= 0.0 && !self.paper {
                    spot = self.fetch_ltp(strat.security_id, &strat.exchange_segment).await;
                }
                // Resolve the execution contract from the operator's strike
                // preferences (ATM / ITM / OTM / strike mode).
                let resolved = self.resolve_option_strategy(&strat, spot, &settings, false);
                match resolved {
                    Some(p) => p,
                    None => {
                        let side = self.desired_option_side(&settings, &strat);
                        self.log_throttled(
                            &format!("no-premium:{}:{side}", strat.id),
                            60_000,
                            "warn",
                            &format!("No {side} option premium for {} - skipping execution", strat.name),
                        );
                        continue;
                    }
                }
            };
            // Publish the chart the order will actually execute on, mirroring
            // the run leg above, so the Running Strategies view names both.
            self.record_strat_leg(&strat.id, "trade", &exec_strat);
            // Same-strike guard: skip the entry when this exact option contract
            // already has a running position. This catches a second strategy
            // resolving to the same strike and a scan after a restart (which
            // clears the in-memory signal edge). It is exempt for the operator's
            // explicit "repeat while condition holds" stacking mode, and it defers
            // entirely to the signal-scoped rule below when "one trade per signal"
            // is ON: that rule permits a genuinely NEW signal to re-enter the same
            // strike, so a running position must not veto it here.
            let once_signal_rules = settings.once_per_signal && strat.synthetic;
            if !once_signal_rules && self.duplicate_entry_blocked(&settings, exec_strat.security_id, true) {
                self.log_throttled(
                    &format!("dupe:{}", strat.id),
                    10_000,
                    "info",
                    &format!(
                        "skip duplicate entry {} -> {} (same strike already running)",
                        strat.name, exec_strat.trading_symbol
                    ),
                );
                continue;
            }
            // "One trade per signal": once a scanner leg has traded a contract in
            // the current signal pulse, hold every further entry on that contract
            // until the pulse ends. A pulse only ends after the gate has stayed
            // false for the debounce window (see `end_signal_if_due`), so a single
            // flicker can never re-arm the guard and repeat the same signal. When
            // a new signal begins, a second entry - even on the same strike, even
            // while the older position is still open - is allowed.
            if let Some(reason) = self.once_per_signal_block_reason(&settings, &strat, exec_strat.security_id) {
                self.log_throttled(
                    &format!("once:{}", strat.id),
                    10_000,
                    "info",
                    &format!(
                        "skip repeat entry {} -> {} ({}; waiting for a fresh signal)",
                        strat.name, exec_strat.trading_symbol, reason
                    ),
                );
                continue;
            }
            // Paper execution is fully REST-free, so make sure the leg it will
            // trade is streaming on the live websocket before the fill is booked.
            if self.paper {
                self.dhan
                    .subscribe_options(&[(exec_strat.security_id, exec_strat.exchange_segment.clone())])
                    .await;
            }
            // "Skip when margin unavailable": decide BEFORE the signal is queued.
            // The margin gate is pure in-memory arithmetic (microseconds), so when
            // the leg's required margin already exceeds the free budget the signal
            // is consumed here - it is never queued/retried, so it can never fill
            // minutes later at a worse price once margin frees. A genuinely new
            // signal after the pulse ends is still allowed.
            if settings.skip_on_margin_block {
                let leg_ltp = self.ltp_of(exec_strat.security_id, &exec_strat.exchange_segment);
                if leg_ltp > 0.0 {
                    if let Some(reason) = self.early_margin_skip_reason(&exec_strat, leg_ltp) {
                        self.consume_signal_for_margin(&strat.id, exec_strat.security_id);
                        self.log_throttled(
                            &format!("marginskip:{}", strat.id),
                            10_000,
                            "warn",
                            &format!(
                                "skip (margin unavailable, no late entry) {} -> {}: {}",
                                strat.name, exec_strat.trading_symbol, reason
                            ),
                        );
                        continue;
                    }
                }
            }
            // Trades per strategy: hard cap counted across open + closed entries.
            // "AI auto trades" removes the cap when enabled.
            if !trade_limit_allows(trade_limit, ai_trades, trade_limit_count, self.trades_done(&strat.id)) {
                continue;
            }
            if let Ok(mut m) = self.last_sig.lock() {
                m.insert(strat.id.clone(), now);
            }
            // Orders/sec cap: never send more than the configured number of
            // entries in any trailing one-second window (Dhan allows ~6/sec).
            if !self.order_allow(order_per_sec) {
                continue;
            }
            // Entry-timing diagnostics: stamp the moment the full filter set
            // first held so the eventual fill delay can be reported.
            if let Ok(mut m) = self.et_pending.lock() {
                m.entry(strat.id.clone())
                    .or_insert((now, self.et_placed.load(Ordering::Relaxed)));
            }
            // Paper-only latency simulation: when the operator turns on the
            // execution-delay checkbox, a fresh signal is not filled on this
            // tick. It is queued and placed `paperExecDelayMs` later using the
            // live LTP at that moment - exactly the extra lag the real tab sees
            // between a condition firing and the Dhan order landing. Off this
            // branch is skipped entirely, so the real tab and a paper tab with
            // the box unticked keep behaving identically.
            if self.paper && settings.paper_exec_delay_on && settings.paper_exec_delay_ms > 0.0 {
                self.set_sig_state(&strat.id, true);
                self.queue_delayed_entry(&strat.id, &exec_strat, settings.paper_exec_delay_ms);
                continue;
            }
            match self.open_entry(&exec_strat, true).await {
                Ok(()) => {
                    self.set_sig_state(&strat.id, true);
                    if strat.synthetic {
                        self.log_throttled(&format!("entry:{}", strat.id), 5_000, "info", &format!("scan ENTRY ok {} -> {} [{}]", strat.name, exec_strat.trading_symbol, exec_strat.instrument));
                    }
                    self.order_record();
                }
                Err(e) => {
                    if settings.skip_on_margin_block && e.starts_with("trade blocked:") {
                        // Consume the signal so the still-true gate cannot re-fire
                        // every scan and eventually fill late once margin frees.
                        self.consume_signal_for_margin(&strat.id, exec_strat.security_id);
                        self.log_throttled(
                            &format!("marginskip:{}", strat.id),
                            10_000,
                            "warn",
                            &format!(
                                "skip (margin unavailable, no late entry) {} -> {}: {e}",
                                strat.name, exec_strat.trading_symbol
                            ),
                        );
                    } else {
                        self.log("error", &format!("entry {} failed: {e}", strat.name));
                    }
                    if let Some(mut d) = self.doc() {
                        if let Some(s) = d.strategies.iter_mut().find(|s| s.id == strat.id) {
                            s.last_error = e.clone();
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Live Data Pool (AI Smart Trading readout)
    // -----------------------------------------------------------------------

    /// Per-strategy live readout: latest candle + each condition's indicator
    /// value and PASS/FAIL. Mirrors the old app's Data Pool so the operator sees
    /// exactly what the strategies are reading. Cached for a few seconds so the
    /// UI poll cannot starve the chart's Dhan candle budget.
    /// Live Data Pool readout. Always answers from cache (never blocks the UI
    /// poll on the throttled Dhan candle fetches a compute needs); a stale or
    /// forced read kicks a single background refresh whose result lands in the
    /// cache for the next poll.
    async fn pool_readout(&self, force: bool) -> Value {
        let now = now_ms();
        let (has_cache, at, cached) = self
            .pool_cache
            .lock()
            .map(|g| (g.1.is_object(), g.0, g.1.clone()))
            .unwrap_or((false, 0, Value::Null));
        let stale = !has_cache || now - at >= 3000;
        if (has_cache && stale) || force {
            if !self.pool_busy.swap(true, Ordering::AcqRel) {
                let me = self.clone();
                tokio::spawn(async move {
                    let v = me.pool_compute().await;
                    if let Ok(mut g) = me.pool_cache.lock() {
                        *g = (now_ms(), v);
                    }
                    me.pool_busy.store(false, Ordering::Release);
                });
            }
        }
        if has_cache {
            cached
        } else {
            json!({ "ok": true, "at": now, "loading": true, "rows": [], "scanner": [] })
        }
    }

    async fn pool_compute(&self) -> Value {
        let now = now_ms();
        let strategies = self.doc().map(|d| d.strategies.clone()).unwrap_or_default();
        let connected = self.dhan.is_connected().await;
        let mut rows: Vec<Value> = Vec::new();
        for strat in strategies {
            let mut row = json!({
                "id": strat.id,
                "name": strat.name,
                "timeframe": strat.timeframe,
                "securityId": strat.security_id,
                "exchangeSegment": strat.exchange_segment,
                "instrument": strat.instrument,
                "tradingSymbol": strat.trading_symbol,
                "side": strat.side,
                "enabled": strat.enabled,
            });
            if strat.security_id <= 0 || strat.conditions.is_empty() {
                row["readout"] = Value::Null;
                rows.push(row);
                continue;
            }
            if !connected {
                row["readout"] = json!({ "error": "not connected" });
                rows.push(row);
                continue;
            }
            match self
                .live_candles(strat.security_id, &strat.exchange_segment, &strat.instrument, &strat.timeframe)
                .await
            {
                Ok(c) if c.len() >= 2 => {
                    let offset = 0usize;
                    let (pass, conds) = condition_detail(&strat.conditions, &c, offset);
                    let idx = c.len().saturating_sub(1 + offset);
                    let bar = &c[idx];
                    row["readout"] = json!({
                        "candle": {
                            "t": bar.time, "open": bar.open, "high": bar.high,
                            "low": bar.low, "close": bar.close, "volume": bar.volume
                        },
                        "conditions": conds,
                        "pass": pass,
                        "offset": offset,
                    });
                }
                Ok(_) => row["readout"] = json!({ "error": "not enough candles" }),
                Err(e) => row["readout"] = json!({ "error": e.to_string() }),
            }
            rows.push(row);
        }
        // Scanner-resolved instruments (Top Movers / NIFTY trend-following): let
        // the operator see the exact option legs the engine will trade even when
        // no saved strategy targets them. Capped so the chart keeps its candle
        // budget; each row carries the latest live candle when connected.
        let picked = self
            .picked_strikes
            .lock()
            .map(|g| g.1.clone())
            .unwrap_or_default();
        let mut scanner: Vec<Value> = Vec::new();
        for p in picked.iter().take(12) {
            let sid = ji(p, "securityId");
            if sid <= 0 {
                continue;
            }
            let (symbol, seg, inst) = match crate::market::symbol_meta(sid) {
                Some((n, s, i)) => (n, s, i),
                None => (
                    js(p, "tradingSymbol"),
                    js(p, "segment"),
                    String::new(),
                ),
            };
            let mut row = json!({
                "securityId": sid,
                "tradingSymbol": if symbol.is_empty() { js(p, "tradingSymbol") } else { symbol },
                "underlying": js(p, "underlying"),
                "side": js(p, "side"),
                "source": js(p, "source"),
                "segment": if seg.is_empty() { js(p, "segment") } else { seg.clone() },
                "spot": jf(p, "spot"),
                "changePct": jf(p, "changePct"),
            });
            if connected {
                let inst = if inst.is_empty() { "OPTIDX".to_string() } else { inst };
                if let Ok(c) = self.live_candles(sid, &seg, &inst, "5min").await {
                    if let Some(bar) = c.last() {
                        row["candle"] = json!({
                            "t": bar.time, "open": bar.open, "high": bar.high,
                            "low": bar.low, "close": bar.close, "volume": bar.volume
                        });
                    }
                }
            }
            scanner.push(row);
        }
        let payload = json!({ "ok": true, "at": now, "rows": rows, "scanner": scanner });
        if let Ok(mut g) = self.pool_cache.lock() {
            *g = (now, payload.clone());
        }
        payload
    }

    // -----------------------------------------------------------------------
    // Position management
    // -----------------------------------------------------------------------

    async fn manage_positions(&self) {
        let ids: Vec<String> = {
            let Some(d) = self.doc() else { return };
            d.positions
                .iter()
                .filter(|p| !js(p, "status").eq_ignore_ascii_case("closing"))
                .map(|p| js(p, "id"))
                .collect()
        };
        for id in ids {
            let (side, entry, ltp, pos_sl, overall_sl, tp, trail, point_trail, trail_tp, mut peak_profit) = {
                let Some(d) = self.doc() else { return };
                let Some(p) = d.positions.iter().find(|p| js(p, "id") == id) else { continue };
                (
                    js(p, "side"),
                    jf(p, "fillPrice").max(jf(p, "entry")),
                    self.ltp_of(ji(p, "securityId"), &js(p, "exchangeSegment")),
                    jf(p, "sl"),
                    jf(p, "overallSl"),
                    jf(p, "tp"),
                    jf(p, "trail"),
                    jf(p, "pointTrail"),
                    jf(p, "trailTp"),
                    jf(p, "peakProfit"),
                )
            };
            if ltp <= 0.0 {
                continue;
            }
            // Reflect the fresh LTP on the ledger.
            if let Some(mut d) = self.doc() {
                if let Some(p) = d.positions.iter_mut().find(|p| js(p, "id") == id) {
                    p["ltp"] = json!(ltp);
                }
            }
            let is_buy = side == "BUY";
            // The overall SL captured at entry is the immutable hard floor.
            let overall_sl = if overall_sl > 0.0 { overall_sl } else { pos_sl };
            // Running per-unit profit peak (drives both trail mechanisms and the
            // manual Trail TP).
            let per_unit = if is_buy { ltp - entry } else { entry - ltp };
            if per_unit > peak_profit {
                peak_profit = per_unit;
            }
            let best = if is_buy {
                entry + peak_profit.max(0.0)
            } else {
                entry - peak_profit.max(0.0)
            };
            // Trail SL is profit-armed, never entry-armed, and comes in two
            // independent flavours:
            //   * percent -> algo-side, a % of the RUNNING live profit. Works in
            //     paper (simulated) and real (the algo sends the exit to Dhan).
            //   * points  -> Dhan Super-Order style, a fixed price jump behind
            //     the best price. Simulated app-side in paper; real orders carry
            //     it as the Super Order `trailingJump` and Dhan trails it itself.
            let mut sl = overall_sl;
            let mut sl_trailed = false;
            if trail > 0.0 && peak_profit > 0.0 {
                let stop = profit_trail_stop(is_buy, entry, peak_profit, trail);
                if more_favourable(is_buy, stop, sl) {
                    sl = stop;
                    sl_trailed = true;
                }
            }
            if self.paper && point_trail > 0.0 && peak_profit > 0.0 {
                let stop = point_trail_stop(is_buy, entry, peak_profit, point_trail);
                if more_favourable(is_buy, stop, sl) {
                    sl = stop;
                    sl_trailed = true;
                }
            }
            // Manual Trail TP: once in profit, exit when the running per-unit
            // profit gives back the configured % from its peak.
            let hit_trail_tp = trail_tp > 0.0
                && peak_profit > 0.0
                && per_unit <= peak_profit * (1.0 - trail_tp / 100.0);
            if let Some(mut d) = self.doc() {
                if let Some(p) = d.positions.iter_mut().find(|p| js(p, "id") == id) {
                    p["best"] = json!(round2(best));
                    p["sl"] = json!(round2(sl));
                    p["slTrailed"] = json!(sl_trailed);
                    p["peakProfit"] = json!(round2(peak_profit));
                }
            }
            // All exits are attached to the position and checked exactly (never
            // on the rounded display price) on every market tick:
            //   * overall SL  -> immutable floor captured at entry
            //   * trail SL    -> percent of the running live profit
            //   * take profit -> fixed target
            let hit_overall = overall_sl > 0.0
                && ((is_buy && ltp <= overall_sl) || (!is_buy && ltp >= overall_sl));
            let hit_trail = sl_trailed
                && ((is_buy && ltp <= sl) || (!is_buy && ltp >= sl));
            let hit_tp = tp > 0.0 && ((is_buy && ltp >= tp) || (!is_buy && ltp <= tp));
            let reason = if hit_tp {
                "TARGET"
            } else if hit_trail_tp {
                "TRAIL_TP"
            } else if hit_trail {
                "TRAIL_SL"
            } else if hit_overall {
                "STOP_LOSS"
            } else {
                ""
            };
            if !reason.is_empty() {
                // Paper books a stop exit at the level that triggered it, so the
                // cut lands exactly at the configured stop even when a single tick
                // gaps straight through it (booking the live mark would otherwise
                // realise the whole gap as extra loss). Real positions keep the
                // live mark; the broker's actual fill is reconciled separately.
                let exit_px = if self.paper {
                    match reason {
                        "TRAIL_SL" => sl,
                        "STOP_LOSS" => overall_sl,
                        "TRAIL_TP" => {
                            let kept = peak_profit * (1.0 - trail_tp / 100.0);
                            if is_buy {
                                entry + kept
                            } else {
                                entry - kept
                            }
                        }
                        _ => ltp,
                    }
                } else {
                    ltp
                };
                // Paper-only exit latency: when enabled, the exit is not booked
                // on the triggering tick. It is queued and lands `paper_exit_delay_ms`
                // later at the SAME level it would have booked without the delay
                // (stop -> stop level, trail -> trail level, target -> target), so
                // the delay only postpones the cut and never re-prices it. (Re-pricing
                // to the live mark turned profit-locking trail exits into losses.)
                let (exit_delay_on, exit_delay_ms) = self
                    .doc()
                    .map(|d| (d.settings.paper_exit_delay_on, d.settings.paper_exit_delay_ms))
                    .unwrap_or((false, 0.0));
                if self.paper && exit_delay_on && exit_delay_ms > 0.0 {
                    self.queue_delayed_exit(&id, reason, exit_px, exit_delay_ms);
                    continue;
                }
                if let Err(e) = self.close_position(&id, reason, exit_px).await {
                    self.log("error", &format!("exit {id} failed: {e}"));
                }
                continue;
            }
        }
    }

    /// Reconcile broker-side Super Orders: if Dhan triggered a leg, book it.
    async fn reconcile_super(&self) {
        if self.paper {
            return;
        }
        let ids: Vec<String> = {
            let Some(d) = self.doc() else { return };
            d.positions
                .iter()
                .filter(|p| jb(p, "broker"))
                .flat_map(|p| jarr(p, "superOrders").into_iter().filter_map(|v| v.as_str().map(String::from)))
                .collect()
        };
        if ids.is_empty() {
            return;
        }
        let Some(client) = self.dhan.session_client().await else { return };
        self.dhan.dhan_order_throttle().await;
        let Ok(orders) = client.super_orders().await else { return };
        for o in orders {
            let oid = o.get("orderId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if oid.is_empty() || !ids.contains(&oid) {
                continue;
            }
            let status = o.get("orderStatus").and_then(|v| v.as_str()).unwrap_or("");
            let legs = o.get("legs").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            let mut triggered: Option<(String, f64)> = None;
            for l in &legs {
                let ls = l.get("orderStatus").and_then(|v| v.as_str()).unwrap_or("");
                let leg = l.get("legName").and_then(|v| v.as_str()).unwrap_or("");
                if matches!(ls, "TRADED" | "COMPLETED") && leg != "ENTRY_LEG" {
                    let px = l.get("triggerPrice").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    triggered = Some((leg.to_string(), px));
                }
            }
            let entry_done = legs.iter().any(|l| {
                l.get("legName").and_then(|v| v.as_str()) == Some("ENTRY_LEG")
                    && matches!(l.get("orderStatus").and_then(|v| v.as_str()), Some("TRADED") | Some("COMPLETED"))
            });
            // Find the position that owns this super order.
            let pid = self.doc().and_then(|d| {
                d.positions
                    .iter()
                    .find(|p| jarr(p, "superOrders").iter().any(|v| v.as_str() == Some(oid.as_str())))
                    .map(|p| js(p, "id"))
            });
            let Some(pid) = pid else { continue };
            if let Some((leg, px)) = triggered {
                self.log("info", &format!("super order {oid} leg {leg} triggered"));
                let ltp = if px > 0.0 { px } else { self.position_ltp(&pid) };
                let reason = if leg == "TARGET_LEG" {
                    "TARGET"
                } else {
                    "STOP_LOSS"
                };
                let _ = self.close_position(&pid, reason, ltp).await;
            } else if entry_done {
                if let Some(mut d) = self.doc() {
                    if let Some(p) = d.positions.iter_mut().find(|p| js(p, "id") == pid) {
                        if jf(p, "fillPrice") <= 0.0 {
                            let avg = legs
                                .iter()
                                .find(|l| l.get("legName").and_then(|v| v.as_str()) == Some("ENTRY_LEG"))
                                .and_then(|l| l.get("averageTradedPrice").and_then(|v| v.as_f64()))
                                .unwrap_or(0.0);
                            if avg > 0.0 {
                                p["fillPrice"] = json!(avg);
                                p["entry"] = json!(avg);
                            }
                        }
                    }
                }
            } else if status.eq_ignore_ascii_case("CANCELLED") || status.eq_ignore_ascii_case("REJECTED") {
                self.log("warn", &format!("super order {oid} is {status}"));
            }
        }
    }

    /// Broker-confirmed fill reconciliation for normal / forever / slice orders
    /// and for closed-trade exit prices. Dhan's order book is the source of truth
    /// for `averageTradedPrice` + `filledQty`, so estimates are replaced as soon
    /// as the exchange reports a fill. Throttled so the 100ms loop does not hammer
    /// the order book.
    async fn reconcile_fills(&self) {
        if self.paper {
            return;
        }
        if !self.dhan.is_connected().await {
            return;
        }
        // Only bother when something is still an estimate.
        let (open_ids, exits): (Vec<String>, Vec<String>) = self
            .doc()
            .map(|d| {
                let open = d
                    .positions
                    .iter()
                    .filter(|p| !jb(p, "reconciled"))
                    .flat_map(|p| jarr(p, "orderIds").into_iter().filter_map(|v| v.as_str().map(String::from)))
                    .collect();
                let exits = d
                    .closed
                    .iter()
                    .filter(|c| jb(c, "exitPending"))
                    .flat_map(|c| jarr(c, "exitOrderIds").into_iter().filter_map(|v| v.as_str().map(String::from)))
                    .collect();
                (open, exits)
            })
            .unwrap_or_default();
        if open_ids.is_empty() && exits.is_empty() {
            return;
        }
        let now = now_ms();
        if now - self.last_fills.load(Ordering::Relaxed) < 2000 {
            return;
        }
        self.last_fills.store(now, Ordering::Relaxed);
        let Some(client) = self.dhan.session_client().await else { return };
        self.dhan.dhan_order_throttle().await;
        let Ok(book) = client.order_book().await else { return };
        let map: HashMap<String, (f64, i64, String)> = book
            .iter()
            .map(|o| {
                (
                    o.order_id.clone(),
                    (o.average_traded_price, o.filled_qty.max(0), o.order_status.to_uppercase()),
                )
            })
            .collect();
        let mut changed = false;
        if let Some(mut d) = self.doc() {
            for p in d.positions.iter_mut() {
                if jb(p, "reconciled") {
                    continue;
                }
                let ids = jarr(p, "orderIds");
                if ids.is_empty() {
                    continue;
                }
                let (mut filled, mut notional, mut terminal) = (0i64, 0f64, true);
                for id in &ids {
                    let Some(oid) = id.as_str() else { continue };
                    match map.get(oid) {
                        Some((avg, f, status)) if *f > 0 => {
                            filled += *f;
                            notional += avg * (*f as f64);
                            if status == "PENDING" || status == "OPEN" || status == "TRANSIT" || status == "PART_TRADED" {
                                terminal = false;
                            }
                        }
                        Some((_, _, status))
                            if !(status == "REJECTED" || status == "CANCELLED" || status == "EXPIRED") =>
                        {
                            terminal = false;
                        }
                        _ => {}
                    }
                }
                if filled > 0 {
                    let avg = round2(notional / filled as f64);
                    if avg > 0.0 {
                        p["fillPrice"] = json!(avg);
                        p["entry"] = json!(avg);
                    }
                    p["filledQty"] = json!(filled);
                    changed = true;
                }
                if terminal && filled >= ji(p, "qty") {
                    p["reconciled"] = json!(true);
                    changed = true;
                }
            }
            for c in d.closed.iter_mut() {
                if !jb(c, "exitPending") {
                    continue;
                }
                let ids = jarr(c, "exitOrderIds");
                let mut notional = 0f64;
                let mut filled = 0i64;
                let mut terminal = true;
                for id in &ids {
                    let Some(oid) = id.as_str() else { continue };
                    match map.get(oid) {
                        Some((avg, f, status)) if *f > 0 => {
                            notional += avg * (*f as f64);
                            filled += *f;
                            if status == "PENDING" || status == "OPEN" || status == "TRANSIT" || status == "PART_TRADED" {
                                terminal = false;
                            }
                        }
                        Some((_, _, status))
                            if !(status == "REJECTED" || status == "CANCELLED" || status == "EXPIRED") =>
                        {
                            terminal = false;
                        }
                        _ => {}
                    }
                }
                if filled > 0 {
                    let exit = round2(notional / filled as f64);
                    let qty = ji(c, "qty");
                    let entry = jf(c, "entry");
                    let pnl = if js(c, "side") == "BUY" {
                        (exit - entry) * qty as f64
                    } else {
                        (entry - exit) * qty as f64
                    };
                    c["exit"] = json!(exit);
                    c["pnl"] = json!(round2(pnl));
                    c["exitPending"] = json!(false);
                    changed = true;
                }
                if terminal && filled <= 0 {
                    c["exitPending"] = json!(false);
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
    }

    /// Post-reconnect broker resync.
    ///
    /// The durable engine book survives a Dhan socket/network drop, so a running
    /// trade's SL / trail / target keeps evaluating from memory and never loses
    /// its levels. What can drift while the feed is down is the *broker* side: the
    /// native Super-Order stop leg can fire, a manual trade can appear, an exit
    /// can be rejected, or a partial exit can settle. This pass runs once on every
    /// false -> true edge of "connected" / "feed streaming" and re-aligns the two
    /// books:
    ///   * book a trade the broker already closed (native SL leg / manual exit),
    ///   * align a trade whose broker qty is smaller (a partial exit while offline),
    ///   * adopt an untracked broker position so SL / trail protect it,
    ///   * re-adopt an exposure whose exit order never filled.
    /// It never overwrites the operator's SL / trail / target values, so after a
    /// reconnect a running trade's protection is exactly what it was before.
    async fn reconcile_reconnect(&self) {
        if self.paper {
            return;
        }
        // Fire only on the false -> true edge, so a fresh Connect and every
        // websocket recovery trigger exactly one resync.
        let conn = self.dhan.is_connected().await;
        let up = self.dhan.feed_up();
        let was_conn = self.was_conn.swap(conn, Ordering::Relaxed);
        let was_feed = self.was_feed.swap(up, Ordering::Relaxed);
        if !((conn && !was_conn) || (up && !was_feed)) {
            return;
        }
        // A flapping socket must not hammer Dhan's order / portfolio APIs.
        let now = now_ms();
        if now - self.last_reconnect.load(Ordering::Relaxed) < 5_000 {
            return;
        }
        self.last_reconnect.store(now, Ordering::Relaxed);

        let Some(client) = self.dhan.session_client().await else { return };
        self.dhan.dhan_order_throttle().await;
        let book = match client.order_book().await {
            Ok(b) => b,
            Err(_) => return,
        };
        self.dhan.dhan_acct_throttle().await;
        let rows = match client.positions().await {
            Ok(r) => r,
            Err(_) => return,
        };

        // Broker net signed qty + average entry per (security, segment).
        struct BrokerPos {
            net: i64,
            avg: f64,
            symbol: String,
        }
        let mut broker: HashMap<(i64, String), BrokerPos> = HashMap::new();
        for r in &rows {
            let sid = r.security_id.parse::<i64>().unwrap_or(0);
            if sid <= 0 || r.net_qty == 0 {
                continue;
            }
            let long = r.position_type.eq_ignore_ascii_case("LONG");
            let avg = if long { r.buy_avg } else { r.sell_avg };
            let avg = if avg > 0.0 { avg } else { r.cost_price };
            let key = (sid, r.exchange_segment.to_uppercase());
            let e = broker.entry(key).or_insert(BrokerPos {
                net: 0,
                avg,
                symbol: r.trading_symbol.clone(),
            });
            e.net += r.net_qty;
            if avg > 0.0 {
                e.avg = avg;
            }
            if e.symbol.is_empty() {
                e.symbol = r.trading_symbol.clone();
            }
        }

        // A filled order on the security in the position's exit direction is hard
        // evidence the broker closed (or partly closed) the trade while the feed
        // was down. Newest update_time wins.
        let exit_fill = |sid: i64, side: &str| -> Option<f64> {
            let want = if side == "BUY" { "SELL" } else { "BUY" };
            book.iter()
                .filter(|o| {
                    o.security_id == sid.to_string()
                        && o.transaction_type.eq_ignore_ascii_case(want)
                        && o.filled_qty > 0
                        && o.average_traded_price > 0.0
                })
                .max_by(|a, b| a.update_time.cmp(&b.update_time))
                .map(|o| o.average_traded_price)
        };

        let (positions, settings, method) = {
            let Some(d) = self.doc() else { return };
            (d.positions.clone(), d.settings.clone(), d.method.clone())
        };
        if positions.is_empty() && broker.is_empty() {
            return;
        }

        // 1. Book trades the broker already closed while we were blind.
        let mut skip: std::collections::HashSet<(i64, String)> = std::collections::HashSet::new();
        let mut to_close: Vec<(String, String, f64)> = Vec::new();
        for p in &positions {
            if js(p, "status").eq_ignore_ascii_case("closing") {
                continue;
            }
            let sid = ji(p, "securityId");
            if sid <= 0 {
                continue;
            }
            let key = (sid, js(p, "exchangeSegment").to_uppercase());
            if broker.get(&key).map(|b| b.net).unwrap_or(0) != 0 {
                continue;
            }
            // Too fresh to judge: the entry may still be in transit.
            let opened = ji(p, "openedAt");
            if opened > 0 && now - opened < 5_000 {
                skip.insert(key);
                continue;
            }
            let entry_filled = jb(p, "reconciled")
                || ji(p, "filledQty") > 0
                || jarr(p, "orderIds").iter().any(|id| {
                    id.as_str()
                        .map(|s| book.iter().any(|o| o.order_id == s && o.filled_qty > 0))
                        .unwrap_or(false)
                });
            if !entry_filled {
                skip.insert(key);
                continue;
            }
            let side = js(p, "side");
            let fill = exit_fill(sid, &side);
            if fill.is_none() && rows.is_empty() {
                // No exit order visible and the portfolio answered empty: could be
                // a flaky empty response. Never book a close on a hunch.
                self.log_throttled(
                    "rt-recon-ambiguous",
                    30_000,
                    "warn",
                    &format!(
                        "reconnect: {} not seen at the broker yet; keeping the trade and retrying",
                        js(p, "tradingSymbol")
                    ),
                );
                skip.insert(key);
                continue;
            }
            let px = fill.unwrap_or_else(|| {
                let l = self.ltp_of(sid, &js(p, "exchangeSegment"));
                if l > 0.0 { l } else { jf(p, "ltp") }
            });
            let px = if px > 0.0 { px } else { jf(p, "fillPrice").max(jf(p, "entry")) };
            let reason = if fill.is_some() { "SL_LEG_FILLED" } else { "BROKER_CLOSED" };
            to_close.push((js(p, "id"), reason.to_string(), px));
        }
        for (id, reason, px) in &to_close {
            if self.close_position_ex(id, reason, *px, Some(Vec::new())).await.is_ok() {
                self.log(
                    "warn",
                    &format!("reconnect: booked broker-side exit for {id} ({reason}) @ {px:.2}"),
                );
            }
        }

        // 2. Align engine qty when the broker shows a smaller same-side net (a
        //    partial exit while offline). A single engine leg is the normal case.
        let engine_net: HashMap<(i64, String), i64> = {
            let Some(d) = self.doc() else { return };
            let mut m: HashMap<(i64, String), i64> = HashMap::new();
            for p in &d.positions {
                if js(p, "status").eq_ignore_ascii_case("closing") {
                    continue;
                }
                let sid = ji(p, "securityId");
                if sid <= 0 {
                    continue;
                }
                let q = ji(p, "qty");
                let signed = if js(p, "side") == "BUY" { q } else { -q };
                *m.entry((sid, js(p, "exchangeSegment").to_uppercase())).or_insert(0) += signed;
            }
            m
        };
        for (key, b) in &broker {
            if skip.contains(key) {
                continue;
            }
            let e = engine_net.get(key).copied().unwrap_or(0);
            if e == 0 || (e > 0) != (b.net > 0) || b.net.abs() >= e.abs() {
                continue;
            }
            let ids: Vec<String> = {
                let Some(d) = self.doc() else { continue };
                d.positions
                    .iter()
                    .filter(|p| {
                        !js(p, "status").eq_ignore_ascii_case("closing")
                            && ji(p, "securityId") == key.0
                            && js(p, "exchangeSegment").eq_ignore_ascii_case(&key.1)
                    })
                    .map(|p| js(p, "id"))
                    .collect()
            };
            if ids.len() == 1 {
                let remain = b.net.abs();
                if let Some(mut d) = self.doc() {
                    if let Some(p) = d.positions.iter_mut().find(|p| js(p, "id") == ids[0]) {
                        p["qty"] = json!(remain);
                        let lot = jf(p, "lotSize");
                        if lot > 0.0 {
                            p["lots"] = json!((remain as f64 / lot).round());
                        }
                        p["mismatch"] = json!(true);
                    }
                }
                self.log(
                    "warn",
                    &format!(
                        "reconnect: broker shows a partial exit on security {}; engine qty aligned to the open {} so the exit cannot flip the position",
                        key.0, remain
                    ),
                );
                self.save();
            } else if ids.len() > 1 {
                skip.insert(key.clone());
                self.log_throttled(
                    "rt-recon-split",
                    30_000,
                    "warn",
                    &format!(
                        "reconnect: broker net {} differs from the engine book for security {}; please review manually",
                        b.net, key.0
                    ),
                );
            }
        }

        // 3. Adopt broker exposure the engine does not fully track so it is
        //    protected by the configured SL / trail instead of sitting naked.
        let covered: HashMap<(i64, String), i64> = {
            let Some(d) = self.doc() else { return };
            let mut m: HashMap<(i64, String), i64> = HashMap::new();
            for p in &d.positions {
                if js(p, "status").eq_ignore_ascii_case("closing") {
                    continue;
                }
                let sid = ji(p, "securityId");
                if sid <= 0 {
                    continue;
                }
                let q = ji(p, "qty");
                let signed = if js(p, "side") == "BUY" { q } else { -q };
                *m.entry((sid, js(p, "exchangeSegment").to_uppercase())).or_insert(0) += signed;
            }
            m
        };
        let mut adopted: Vec<Value> = Vec::new();
        let mut adopted_keys: Vec<(i64, String, bool)> = Vec::new();
        for (key, b) in &broker {
            if skip.contains(key) {
                continue;
            }
            let e = covered.get(key).copied().unwrap_or(0);
            let delta = reconnect_delta(e, b.net);
            if delta == 0 {
                continue;
            }
            let seg = key.1.clone();
            let avg = if b.avg > 0.0 { b.avg } else { self.ltp_of(key.0, &seg) };
            if avg <= 0.0 {
                self.log_throttled(
                    "rt-recon-noavg",
                    30_000,
                    "warn",
                    &format!("reconnect: no entry price for broker position {}; not adopting", key.0),
                );
                continue;
            }
            let side = if delta > 0 { "BUY" } else { "SELL" };
            let qty = delta.abs();
            let (sl, tp, trail, trail_tp, point_trail) =
                levels_for(&settings, &json!({}), side, avg, 0.0, 0.0, 0.0);
            let protected = sl > 0.0 || tp > 0.0 || trail > 0.0 || trail_tp > 0.0 || point_trail > 0.0;
            adopted.push(json!({
                "id": gen_id("rtadopt"),
                "strategyId": "recovered",
                "strategyName": "Recovered (broker)",
                "method": method,
                "securityId": key.0,
                "exchangeSegment": seg,
                "instrument": "",
                "tradingSymbol": b.symbol,
                "underlying": underlying_of(&b.symbol, &seg),
                "side": side,
                "qty": qty,
                "lots": 0.0,
                "lotSize": 0.0,
                "filledQty": qty,
                "entry": round2(avg),
                "fillPrice": round2(avg),
                "ltp": round2(avg),
                "sl": round2(sl),
                "overallSl": round2(sl),
                "tp": round2(tp),
                "trail": round2(trail),
                "pointTrail": round2(point_trail),
                "trailTp": round2(trail_tp),
                "peakProfit": 0.0,
                "best": round2(avg),
                "orderIds": [],
                "superOrders": [],
                "broker": true,
                "reconciled": true,
                "adopted": true,
                "status": "running",
                "orderType": "MARKET",
                "limitPrice": 0.0,
                "openedAt": now,
                "log": [{ "t": now, "msg": "recovered from the broker after a reconnect" }],
            }));
            adopted_keys.push((key.0, key.1.clone(), protected));
        }
        if !adopted.is_empty() {
            let n = adopted.len();
            if let Some(mut d) = self.doc() {
                for p in adopted {
                    d.positions.push(p);
                }
            }
            for (sid, seg, protected) in &adopted_keys {
                self.dhan.subscribe_options(&[(*sid, seg.clone())]).await;
                if *protected {
                    self.log(
                        "warn",
                        &format!("reconnect: adopted broker position {} ({}) so SL / trail protect it", sid, seg),
                    );
                } else {
                    self.log(
                        "warn",
                        &format!(
                            "reconnect: adopted broker position {} ({}) but no SL / trail / target is configured - set one to protect it",
                            sid, seg
                        ),
                    );
                }
            }
            self.save();
            self.log("info", &format!("reconnect: broker resync adopted {n} position(s)"));
        }
    }

    fn position_ltp(&self, id: &str) -> f64 {
        let (sid, exch) = self
            .doc()
            .and_then(|d| d.positions.iter().find(|p| js(p, "id") == id).map(|p| (ji(p, "securityId"), js(p, "exchangeSegment"))))
            .unwrap_or((0, String::new()));
        self.ltp_of(sid, &exch)
    }

    // -----------------------------------------------------------------------
    // Entry / exit
    // -----------------------------------------------------------------------

    /// The one CE/PE side a strategy should trade: the strict NIFTY
    /// straight-line direction lock wins over everything (a bullish NIFTY only
    /// trades CE, a bearish NIFTY only PE), then the "Run Strategy In" override
    /// over the Option Type dropdown (old AST `forcedSide`), else the dropdown,
    /// else the live scanner direction, else the strategy's own bullish/bearish
    /// category. Shared by the automatic leg resolver and the testing-only
    /// Manual Strike Select so both always agree on the side.
    fn target_option_side(&self, strat: &Strategy, settings: &Settings) -> &'static str {
        // Filter-side routing (highest priority): the strategy's own filter side
        // picks the contract, so a bullish filter set can only ever trade CE and a
        // bearish set only PE - no scanner/run-in/overall override can flip it.
        if let Some(side) = routed_option_side(settings, strat) {
            return side;
        }
        if let Some(side) = self.nifty_locked_side(strat, settings) {
            return side;
        }
        if let Some(side) = self.effective_run_in_side(settings) {
            return side;
        }
        match settings.option_side.to_uppercase().as_str() {
            "CE" => "CE",
            "PE" => "PE",
            _ => {
                // Manual Strike Select keeps the scanner direction out of the
                // decision: the operator's explicit picks plus the strategy's own
                // bullish/bearish side decide (an explicit run-in always wins).
                if !settings.manual_strikes_enabled {
                    if let Some(side) = self.auto_option_side(settings) {
                        return side;
                    }
                }
                if strategy_is_bull(strat) {
                    "CE"
                } else {
                    "PE"
                }
            }
        }
    }

    /// Testing-only Manual Strike Select: when enabled and the operator has
    /// picked at least one contract for the strategy's underlying, return that
    /// contract instead of the automatic ATM / strike-mode resolution. Prefers a
    /// pick whose side matches `target_option_side` (nearest to spot when several
    /// exist) and falls back to the nearest pick of either side. Returns `None`
    /// when the mode is off, the list is empty, or no pick matches the strategy's
    /// underlying - callers then keep the normal resolution.
    fn resolve_manual_strike(&self, strat: &Strategy, spot: f64, settings: &Settings) -> Option<Strategy> {
        if !settings.manual_strikes_enabled || settings.manual_strikes.is_empty() {
            return None;
        }
        let sc = scrip::get()?;
        let prefix = scrip::fno_underlying(&strat.trading_symbol);
        if prefix.is_empty() {
            return None;
        }
        let want = self.target_option_side(strat, settings);
        let mut matching: Vec<&ManualStrike> = settings
            .manual_strikes
            .iter()
            .filter(|m| {
                m.strike > 0.0
                    && (m.option_type.eq_ignore_ascii_case("CE") || m.option_type.eq_ignore_ascii_case("PE"))
                    && scrip::fno_underlying(&m.symbol_name) == prefix
            })
            .collect();
        if matching.is_empty() {
            return None;
        }
        // Prefer the wanted side; if the operator only picked the other side, use
        // it anyway (manual mode is an explicit testing override, not a filter).
        let side_match: Vec<&ManualStrike> = matching
            .iter()
            .copied()
            .filter(|m| m.option_type.eq_ignore_ascii_case(want))
            .collect();
        if !side_match.is_empty() {
            matching = side_match;
        }
        let pick = if spot > 0.0 {
            matching
                .iter()
                .copied()
                .min_by(|a, b| {
                    (a.strike - spot)
                        .abs()
                        .partial_cmp(&(b.strike - spot).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        } else {
            matching.first().copied()
        }?;
        let ot = pick.option_type.to_uppercase();
        let res = sc.resolve(&pick.symbol_name, &pick.expiry, pick.strike, &ot, &pick.exchange_segment)?;
        let exch = scrip::scrip_exch(&pick.exchange_segment);
        let seg = fno_segment(exch);
        let mut out = strat.clone();
        out.security_id = res.security_id;
        out.exchange_segment = seg.to_string();
        out.instrument = if scrip::is_index_prefix(&prefix) { "OPTIDX" } else { "OPTSTK" }.to_string();
        out.trading_symbol = res.trading_symbol;
        Some(out)
    }

    /// Resolve an index/underlying strategy into a concrete option contract:
    /// nearest expiry, ATM/ITM/OTM strike from the scrip master, CE for bullish
    /// (or BUY) and PE for bearish (or SELL). Mirrors the old engine's option
    /// selection. Returns `None` when the scrip master cannot resolve it.
    fn resolve_option_strategy(&self, strat: &Strategy, spot: f64, settings: &Settings, provisional: bool) -> Option<Strategy> {
        let sc = scrip::get()?;
        // Manual Strike Select (testing) wins over every automatic preference.
        if let Some(m) = self.resolve_manual_strike(strat, spot, settings) {
            return Some(m);
        }
        if spot <= 0.0 {
            return None;
        }
        let prefix = scrip::fno_underlying(&strat.trading_symbol);
        if prefix.is_empty() {
            return None;
        }
        let exch = scrip::scrip_exch(&strat.exchange_segment);
        let expiries = sc.expiries_for(&prefix, exch)?;
        let expiry = expiries.first()?.clone();
        let bucket = sc.bucket(exch, &prefix, &expiry)?;
        if bucket.is_empty() {
            return None;
        }
        let strikes: Vec<f64> = bucket.keys().map(|k| *k as f64 / 100.0).collect();
        let mut idx = 0usize;
        let mut best = f64::MAX;
        for (i, s) in strikes.iter().enumerate() {
            let d = (s - spot).abs();
            if d < best {
                best = d;
                idx = i;
            }
        }
        let ot = self.target_option_side(strat, settings);
        // Strike selection preference (the "Execute Trade In" dropdown supplies the
        // direction for all of them):
        //   * "Only +green premium strikes"  -> first rising (+green) strike.
        //   * "Pick fastest positive rising LTP" -> the biggest riser (max LTP
        //     change %) among the rising strikes; implies +green eligibility.
        //   * "Opposite side fallback" -> if the chosen side is entirely minus,
        //     retry the opposite side of ATM in the same leg.
        // When no preference is on, fall back to the nearest ATM strike.
        let strike_idx = if settings.only_positive || settings.fastest_rising {
            match self.pick_strike_pref(strat, exch, &expiry, &strikes, idx, ot, settings, provisional) {
                Some(i) => i,
                None => return None,
            }
        } else {
            idx
        };
        let strike = strikes[strike_idx];
        let res = sc.resolve(&strat.trading_symbol, &expiry, strike, ot, &strat.exchange_segment)?;
        let seg = fno_segment(exch);
        let mut out = strat.clone();
        out.security_id = res.security_id;
        out.exchange_segment = seg.to_string();
        // Dhan needs OPTIDX for index options, OPTSTK for stock options and
        // OPTFUT for MCX commodity options; the wrong type makes `/charts`
        // answer an empty payload (or one junk bar), so a commodity CE/PE leg
        // would never resolve.
        out.instrument = if scrip::is_index_prefix(&prefix) {
            "OPTIDX"
        } else if exch.eq_ignore_ascii_case("MCX") {
            "OPTFUT"
        } else {
            "OPTSTK"
        }
        .to_string();
        out.trading_symbol = res.trading_symbol;
        Some(out)
    }

    /// The full walk order of strike indices for the "Execute Trade In" dropdown,
    /// starting at ATM `idx` and continuing outward in the selected direction to
    /// the chain's edge. The "+green" scan follows this order and only stops at
    /// the first strike whose premium is plus - "Number of Strikes" does NOT cap
    /// this walk, so a run of minus strikes is skipped over to the next strike in
    /// the same leg (e.g. Above ATM: idx+1 minus -> idx+2 -> idx+3 ...).
    ///
    /// `above`/`above_atm` go up, `below`/`below_atm` go down, `both_atm`/
    /// `both_atm_inc` alternate up/down outward, `atm` is the ATM leg only.
    fn green_walk_order(len: usize, idx: usize, mode: &str) -> Vec<usize> {
        let eq = |s: &str| mode.eq_ignore_ascii_case(s);
        let mut out: Vec<usize> = Vec::with_capacity(len);
        if eq("atm") {
            out.push(idx);
            return out;
        }
        let upward = eq("above") || eq("above_atm");
        let downward = eq("below") || eq("below_atm");
        let both = eq("both_atm") || eq("both_atm_inc");
        // "*_ATM*" modes include the ATM leg itself first.
        if eq("above_atm") || eq("below_atm") || eq("both_atm_inc") {
            out.push(idx);
        }
        if upward {
            let mut j = idx as i64 + 1;
            while j < len as i64 {
                out.push(j as usize);
                j += 1;
            }
        } else if downward {
            let mut j = idx as i64 - 1;
            while j >= 0 {
                out.push(j as usize);
                j -= 1;
            }
        } else if both {
            let mut k = 1i64;
            loop {
                let up = idx as i64 + k;
                let dn = idx as i64 - k;
                let mut any = false;
                if up < len as i64 {
                    out.push(up as usize);
                    any = true;
                }
                if dn >= 0 {
                    out.push(dn as usize);
                    any = true;
                }
                if !any {
                    break;
                }
                k += 1;
            }
        } else {
            // Unknown mode: fall back to the ATM leg only.
            out.push(idx);
        }
        out
    }

    /// Pure sign decision for the "+green" scan. Given the live LTP, the previous
    /// print's LTP and the published change, it returns the LTP's own plus sign
    /// (tick-to-tick) and the change's plus sign. `None` when there is no usable
    /// positive LTP. State-free so it is trivially testable.
    ///
    /// The LTP sign is *never* derived from a previous close: it compares the
    /// current print against the previous print. Only when no previous print has
    /// been observed yet does it fall back to the change's sign so the scan can
    /// still decide on the very first evaluation.
    ///
    /// A strike whose change is not available (never seeded / genuinely flat, i.e.
    /// `0`) leans on the live print for that half of the decision, so the scan
    /// still works without a REST seed. A genuine minus change is never bought.
    fn premium_signs_from(ltp: f64, prev_ltp: f64, change: f64) -> Option<(bool, bool)> {
        if ltp <= 0.0 {
            return None;
        }
        let ltp_plus = if prev_ltp > 0.0 { ltp > prev_ltp } else { change > 0.0 };
        let chg_plus = if change != 0.0 { change > 0.0 } else { ltp_plus };
        Some((ltp_plus, chg_plus))
    }

    /// The opposite ATM side of a single-sided "Execute Trade In" mode, or `None`
    /// when the mode already covers both sides / is ATM-only (no opposite to fall
    /// back to). Above <-> Below (including-ATM variants mirror each other).
    fn opposite_side_mode(mode: &str) -> Option<&'static str> {
        if mode.eq_ignore_ascii_case("above") {
            Some("below")
        } else if mode.eq_ignore_ascii_case("above_atm") {
            Some("below_atm")
        } else if mode.eq_ignore_ascii_case("below") {
            Some("above")
        } else if mode.eq_ignore_ascii_case("below_atm") {
            Some("above_atm")
        } else {
            None
        }
    }

    /// True when score `a` = (published change %, absolute change) outranks `b`:
    /// a higher LTP change % wins; an equal change % is broken by the higher
    /// absolute LTP change. Pure so the "fastest positive rising" ranking is
    /// trivially testable.
    fn rising_score_better(a: (f64, f64), b: (f64, f64)) -> bool {
        a.0 > b.0 || (a.0 == b.0 && a.1 > b.1)
    }

    /// Read one ordered leg list under a single quote-cache lock and report the
    /// best eligible rising strike, whether any strike on this side had a live
    /// quote, and the still-unquoted candidates (in order) the caller should warm
    /// next.
    ///
    /// Eligible = plus on BOTH its live LTP (tick-to-tick) and its published
    /// change, i.e. actually rising - a falling/flat strike is never eligible.
    /// With `fastest` the scan keeps going and keeps the strike with the highest
    /// published `change_pct` (ties broken by the absolute `change`); otherwise it
    /// returns the first rising strike in walk order.
    fn scan_strike_order(
        &self,
        legs: &[(usize, i64)],
        seg: &str,
        fastest: bool,
    ) -> (Option<(usize, i64)>, bool, Vec<(i64, String)>) {
        let mut best: Option<(usize, i64)> = None;
        let mut best_key: Option<(f64, f64)> = None;
        let mut any_quoted = false;
        let mut unquoted: Vec<(i64, String)> = Vec::new();
        let Ok(q) = self.dhan.market.quotes.lock() else {
            return (None, false, unquoted);
        };
        for (i, sid) in legs {
            let key = crate::market::quote_key(*sid, seg);
            let Some(v) = q.get(&key) else {
                unquoted.push((*sid, seg.to_string()));
                continue;
            };
            let ltp = v.get("ltp").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if ltp <= 0.0 {
                unquoted.push((*sid, seg.to_string()));
                continue;
            }
            any_quoted = true;
            let change = v.get("change").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let prev = v.get("prev_ltp").and_then(|x| x.as_f64()).unwrap_or(0.0);
            if !matches!(Self::premium_signs_from(ltp, prev, change), Some((true, true))) {
                continue;
            }
            if !fastest {
                best = Some((*i, *sid));
                break;
            }
            let pct = v.get("change_pct").and_then(|x| x.as_f64()).unwrap_or(0.0);
            let score = (pct, change);
            let better = match best_key {
                None => true,
                Some(bk) => Self::rising_score_better(score, bk),
            };
            if better {
                best_key = Some(score);
                best = Some((*i, *sid));
            }
        }
        (best, any_quoted, unquoted)
    }

    /// Pick the strike for the "Execute Trade In" direction, wiring the strike
    /// preferences together:
    ///   * "Only +green premium strikes" walks the chosen direction and takes the
    ///     FIRST rising (+green) strike; the walk is NOT capped by "Number of
    ///     Strikes" so a run of minus strikes is skipped to the next strike.
    ///   * "Pick fastest positive rising LTP" scans the same direction and takes
    ///     the BIGGEST riser - the highest published LTP change % among the rising
    ///     strikes (ties broken by the absolute LTP change).
    ///   * "Opposite side fallback" -> only once the ENTIRE selected side has a
    ///     live quote and every strike is minus, the opposite side of ATM
    ///     (Above <-> Below) is scanned in the SAME option leg (CE/PE).
    /// Any still-unquoted candidate on the chosen side is warmed first so a rising
    /// strike that lives on the chosen side is never missed. `None` only when no
    /// rising strike exists in either direction.
    ///
    /// The scan reads every candidate's signs/change under a single quote-cache
    /// lock, so it stays well inside the entry latency budget and never buys a
    /// falling premium.
    fn pick_strike_pref(
        &self,
        strat: &Strategy,
        exch: &str,
        expiry: &str,
        strikes: &[f64],
        idx: usize,
        ot: &str,
        settings: &Settings,
        provisional: bool,
    ) -> Option<usize> {
        // Warm the whole candidate walk in one pass so a pick can land on the
        // very next 100ms scanner tick instead of advancing 8 strikes per pass.
        const WARM_PER_CALL: usize = 128;
        let fastest = settings.fastest_rising;
        let sc = scrip::get()?;
        let order = Self::green_walk_order(strikes.len(), idx, &settings.strike_mode);
        if order.is_empty() {
            return None;
        }
        let seg = fno_segment(exch);
        let resolve_order = |order: &[usize]| -> Vec<(usize, i64)> {
            let mut v: Vec<(usize, i64)> = Vec::with_capacity(order.len());
            for i in order {
                if let Some(r) = sc.resolve(&strat.trading_symbol, expiry, strikes[*i], ot, &strat.exchange_segment) {
                    if r.security_id > 0 {
                        v.push((*i, r.security_id));
                    }
                }
            }
            v
        };
        let primary = resolve_order(&order);
        if primary.is_empty() {
            return None;
        }
        let (mut best, any_quoted, mut warm) = self.scan_strike_order(&primary, seg, fastest);
        // The selected side still has unquoted candidates: warm them and let the
        // next evaluation judge them instead of falling back early, so a rising
        // strike on the chosen side is never skipped.
        if best.is_none() && !warm.is_empty() {
            warm.truncate(WARM_PER_CALL);
            self.dhan.watch_options_now(&warm);
            // Readout (provisional): never leave the picked-strike list blank
            // while the +green quotes warm up. Show the nearest resolvable
            // contract in the chosen direction now; the strict entry path still
            // returns None until a genuinely rising premium is found.
            if provisional {
                return Some(primary[0].0);
            }
            return None;
        }
        // The ENTIRE selected side is quoted and every strike is minus -> try the
        // opposite ATM side, in the same leg, before giving up on the entry.
        if best.is_none() && settings.fallback_opposite_side && any_quoted {
            if let Some(omode) = Self::opposite_side_mode(&settings.strike_mode) {
                let oorder = Self::green_walk_order(strikes.len(), idx, omode);
                let olegs = resolve_order(&oorder);
                if !olegs.is_empty() {
                    let (obest, _, ow) = self.scan_strike_order(&olegs, seg, fastest);
                    best = obest;
                    warm.extend(ow);
                }
            }
        }
        if let Some((i, sid)) = best {
            // Make sure the chosen leg is streaming even if it was already cached.
            self.dhan.watch_options_now(&[(sid, seg.to_string())]);
            return Some(i);
        }
        if !warm.is_empty() {
            warm.truncate(WARM_PER_CALL);
            self.dhan.watch_options_now(&warm);
        }
        // Provisional readout: the selected side is fully quoted but none is
        // rising (and no opposite-side fallback applied). Show the nearest
        // contract so the direction's strike is visible; the entry stays gated.
        if provisional {
            return Some(primary[0].0);
        }
        None
    }

    /// Whether an option's own premium is consistent with the underlying: a long
    /// option can never be worth less than its intrinsic value, so a print below
    /// intrinsic means the option feed (or the underlying's) is stale/crossed and
    /// must not be filled. The 2% slack absorbs the underlying's own last-trade
    /// lag without letting a genuinely stale premium through. Fails open for
    /// non-options, unknown symbols or a missing underlying quote.
    fn option_premium_sane(&self, opt: &Strategy, premium: f64) -> bool {
        if premium <= 0.0 {
            return false;
        }
        let Some((strike, ot)) = option_strike_type(&opt.trading_symbol) else { return true };
        let Some(und) = underlying_strategy(opt) else { return true };
        let spot = self.ltp_of(und.security_id, &und.exchange_segment);
        if spot <= 0.0 {
            return true;
        }
        let intrinsic = if ot.eq_ignore_ascii_case("PE") {
            (strike - spot).max(0.0)
        } else {
            (spot - strike).max(0.0)
        };
        premium >= intrinsic * 0.98
    }

    /// Resolve a strategy to its option-premium contract and fetch that chart's
    /// candles (used by "run in premium" / dual-confirmation modes). `None` when
    /// the contract or its candles cannot be resolved.
    /// Fetch the underlying spot chart's candles for an option-instrument
    /// strategy so "Run Strategy In: Spot chart" evaluates on the underlying
    /// (`None` when the strategy is not an option or the underlying is unknown).
    async fn underlying_candles(&self, strat: &Strategy) -> Option<(Strategy, Vec<Candle>)> {
        let spot = underlying_strategy(strat)?;
        let c = self
            .live_candles(spot.security_id, &spot.exchange_segment, &spot.instrument, &spot.timeframe)
            .await
            .ok()?;
        if c.len() < 3 {
            return None;
        }
        Some((spot, c))
    }

    async fn premium_candles(&self, strat: &Strategy, settings: &Settings) -> Option<(Strategy, Vec<Candle>)> {
        // ATM/ITM/OTM strike selection needs the UNDERLYING spot. For an
        // option-instrument strategy `strat.security_id` is the option itself,
        // whose premium would pick the wrong strike, so resolve the underlying.
        let (spot_sid, spot_seg) = match underlying_strategy(strat) {
            Some(u) => (u.security_id, u.exchange_segment),
            None => (strat.security_id, strat.exchange_segment.clone()),
        };
        let mut spot = self.ltp_of(spot_sid, &spot_seg);
        if spot <= 0.0 && !self.paper {
            spot = self.fetch_ltp(spot_sid, &spot_seg).await;
        }
        if spot <= 0.0 {
            return None;
        }
        let res = self.resolve_option_strategy(strat, spot, settings, false)?;
        // Publish the resolved premium chart so the Running Strategies view can
        // show which chart this strategy's entry conditions are evaluated on.
        self.record_strat_leg(&strat.id, "run", &res);
        // Keep the resolved paper leg streaming so its premium ticks arrive over
        // the websocket (paper never pays a REST quote for it).
        if self.paper {
            self.dhan
                .subscribe_options(&[(res.security_id, res.exchange_segment.clone())])
                .await;
        }
        let c = self
            .live_candles(res.security_id, &res.exchange_segment, &res.instrument, &res.timeframe)
            .await
            .ok()?;
        if c.len() < 3 {
            return None;
        }
        Some((res, c))
    }

    /// Single-instrument LTP straight from Dhan's `/marketfeed/ltp`.
    async fn fetch_ltp(&self, sid: i64, exch: &str) -> f64 {
        let Some(client) = self.dhan.session_client().await else { return 0.0 };
        let mut req: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        req.entry(exch.to_string()).or_default().push(sid);
        self.dhan.dhan_throttle().await;
        if let Ok(resp) = client.market_feed_ltp(&req).await {
            for (_s, legs) in resp {
                for (_id, e) in legs {
                    if e.last_price > 0.0 {
                        return e.last_price;
                    }
                }
            }
        }
        0.0
    }

    /// Volatility-based AI levels: ATR from recent candles sets the stop at
    /// 1.5x ATR, the target at 3x ATR and the trail distance at 1x ATR.
    ///
    /// The ATR is cached for a minute and any fetch is hard-bounded, so a slow
    /// or throttled Dhan candle call can never hold an entry order back by
    /// seconds (the entry latency that made every fill chase the move).
    async fn ai_levels(&self, strat: &Strategy, side: &str, ltp: f64) -> Option<(f64, f64, f64)> {
        let key = format!(
            "{}:{}:{}:{}",
            strat.security_id,
            strat.exchange_segment.to_uppercase(),
            strat.instrument.to_uppercase(),
            strat.timeframe
        );
        let atr = if let Some(a) = self.cached_atr(&key, 60_000) {
            a
        } else {
            // Paper never REST-seeds a candle series; it reads the live websocket
            // series only, so paper execution stays completely REST-free.
            let candles = if self.paper {
                match self
                    .dhan
                    .market
                    .live_bars_for(strat.security_id, &strat.timeframe, LIVE_BAR_TTL)
                {
                    Some(c) if c.len() >= 15 => c,
                    _ => return None,
                }
            } else {
                let fut = self
                    .live_candles(strat.security_id, &strat.exchange_segment, &strat.instrument, &strat.timeframe);
                match tokio::time::timeout(Duration::from_millis(1500), fut).await {
                    Ok(Ok(c)) if c.len() >= 15 => c,
                    _ => return None,
                }
            };
            let settings: algo_core::model::Settings = Default::default();
            let out = algo_core::compute("atr", &candles, &settings);
            let a = out
                .first()
                .and_then(|s| s.data.last())
                .map(|p| p.value)
                .filter(|v| *v > 0.0)?;
            if let Ok(mut m) = self.atr_cache.lock() {
                m.insert(key, (now_ms(), a));
            }
            a
        };
        let is_buy = side == "BUY";
        let sl = if is_buy { ltp - 1.5 * atr } else { ltp + 1.5 * atr };
        let tp = if is_buy { ltp + 3.0 * atr } else { ltp - 3.0 * atr };
        Some((sl.max(0.05), tp.max(0.05), atr))
    }

    fn cached_atr(&self, key: &str, max_age_ms: i64) -> Option<f64> {
        let g = self.atr_cache.lock().ok()?;
        let (at, v) = g.get(key)?;
        if *v > 0.0 && now_ms() - *at <= max_age_ms {
            Some(*v)
        } else {
            None
        }
    }

    /// Paper-only simulated execution latency.
    ///
    /// Queues a qualifying entry and places it `delay_ms` after the condition
    /// first held (using the live LTP at that moment, like a real order that
    /// lands a little late). Disabling the feature means this is never called.
    /// While the order is in flight the strategy id stays in `paper_pending`,
    /// so the 100ms scanner cannot queue a duplicate; it is removed once the
    /// entry resolves. If the engine stops or is disarmed during the wait the
    /// queued order is dropped, matching the live tab's arm gate.
    fn queue_delayed_entry(&self, id: &str, strat: &Strategy, delay_ms: f64) {
        let due = now_ms() + delay_ms.max(0.0) as i64;
        {
            let Ok(mut m) = self.paper_pending.lock() else { return };
            if m.contains_key(id) {
                return;
            }
            m.insert(id.to_string(), due);
        }
        let me = self.clone();
        let id = id.to_string();
        let strat = strat.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0.0) as u64)).await;
            let ready = me
                .doc
                .lock()
                .map(|d| d.engine_on && d.armed)
                .unwrap_or(false);
            if ready {
                match me.open_entry(&strat, true).await {
                    Ok(()) => {
                        me.order_record();
                        me.log_throttled(
                            &format!("delay-entry:{}", strat.id),
                            5_000,
                            "info",
                            &format!(
                                "paper delayed ENTRY ok (+{:.0}ms) {} -> {}",
                                delay_ms, strat.name, strat.trading_symbol
                            ),
                        );
                    }
                    Err(e) => {
                        // The fresh-signal edge was consumed when the order was
                        // queued. If it never fills (margin block / rejection /
                        // no premium) release the edge so the still-true gate can
                        // re-trigger, instead of latching silent until the gate
                        // happens to reset and print a new signal. EXCEPTION:
                        // when "skip when margin unavailable" is ON, a margin
                        // block must NOT hand the edge back - doing so is exactly
                        // what let the same signal re-queue and fill minutes later
                        // at a worse price. Consume it instead.
                        if me.skip_margin_block_enabled() && e.starts_with("trade blocked:") {
                            me.consume_signal_for_margin(&strat.id, strat.security_id);
                            me.log_throttled(
                                &format!("marginskip:{}", strat.id),
                                10_000,
                                "warn",
                                &format!(
                                    "skip (margin unavailable, no late entry) {} -> {}: {e}",
                                    strat.name, strat.trading_symbol
                                ),
                            );
                        } else {
                            me.release_fresh_edge(&strat.id);
                            me.log_throttled(
                                &format!("delay-fail:{}", strat.id),
                                5_000,
                                "error",
                                &format!("delayed entry {} failed: {e}", strat.name),
                            );
                        }
                        if let Some(mut d) = me.doc() {
                            if let Some(s) = d.strategies.iter_mut().find(|s| s.id == strat.id) {
                                s.last_error = e.clone();
                            }
                        }
                    }
                }
            } else {
                // Dropped before it could fill: release the fresh edge too.
                me.release_fresh_edge(&strat.id);
                me.log_throttled(
                    &format!("delay-cancel:{}", strat.id),
                    5_000,
                    "info",
                    &format!("paper delayed entry {} dropped: engine stopped/disarmed", strat.name),
                );
            }
            if let Ok(mut m) = me.paper_pending.lock() {
                m.remove(&id);
            }
        });
    }

    /// Paper-only simulated exit latency.
    ///
    /// Queues a stop / trail-stop / target exit and books it `delay_ms` after
    /// the level first triggered, at the SAME `exit_px` the exit would have
    /// booked without the delay (stop -> stop level, trail -> trail level,
    /// target -> target). The delay only postpones the cut; it never re-prices
    /// it, so the paper P&L semantics stay identical to delay-off. While the
    /// exit is in flight the position id stays in `paper_exit_pending`, so the
    /// position manager cannot queue the same exit twice; it is removed once
    /// resolved. Disabling the feature means this is never called.
    fn queue_delayed_exit(&self, id: &str, reason: &str, exit_px: f64, delay_ms: f64) {
        let due = now_ms() + delay_ms.max(0.0) as i64;
        {
            let Ok(mut m) = self.paper_exit_pending.lock() else { return };
            if m.contains_key(id) {
                return;
            }
            m.insert(id.to_string(), due);
        }
        let me = self.clone();
        let id = id.to_string();
        let reason = reason.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms.max(0.0) as u64)).await;
            let open = me
                .doc()
                .map(|d| d.positions.iter().any(|p| js(p, "id") == id))
                .unwrap_or(false);
            if open {
                match me.close_position(&id, &reason, exit_px).await {
                    Ok(()) => me.log_throttled(
                        &format!("delay-exit:{id}"),
                        5_000,
                        "info",
                        &format!(
                            "paper delayed EXIT {reason} (+{:.0}ms) booked at {:.2}",
                            delay_ms, exit_px
                        ),
                    ),
                    Err(e) => me.log("error", &format!("delayed exit {id} failed: {e}")),
                }
            }
            if let Ok(mut m) = me.paper_exit_pending.lock() {
                m.remove(&id);
            }
        });
    }

    /// Spot forming-candle colour gate. Returns `Some(reason)` when an entry must
    /// be skipped right now because the spot chart's live forming candle is the
    /// wrong colour for this leg's direction: a bullish leg (Top Gainer) may enter
    /// only on a GREEN candle, a bearish leg (Top Loser) only on a RED one. Returns
    /// `None` when the gate is off, the order is the operator's manual order, the
    /// entry rule is not evaluated on the spot chart, or the required colour is
    /// present. A doji (close == open) is neither green nor red, so the entry is
    /// held until a coloured candle forms.
    async fn spot_candle_gate_skip(&self, settings: &Settings, strat: &Strategy) -> Option<String> {
        if !settings.spot_candle_gate || strat.manual {
            return None;
        }
        // Only when the entry rule runs on the spot chart. A premium-only or
        // futures-only run has no spot candle to read, so the gate is a no-op.
        let run_mode = run_mode_of(strat, settings);
        if run_mode != "spot" && run_mode != "both" {
            return None;
        }
        // The spot chart is the strategy's own chart for a cash/stock leg, and the
        // resolved underlying spot for an option leg.
        let spot = underlying_strategy(strat).unwrap_or_else(|| strat.clone());
        let candles = self
            .live_candles(spot.security_id, &spot.exchange_segment, &spot.instrument, &spot.timeframe)
            .await
            .ok()?;
        // Offset 0 (the live forming bar) is the last candle in the series.
        let last = candles.last()?;
        let bull = strategy_is_bull(strat);
        if !spot_candle_color_ok(bull, last.open, last.close) {
            return Some(if bull {
                "spot candle not green (Top Gainer needs green)".into()
            } else {
                "spot candle not red (Top Loser needs red)".into()
            });
        }
        None
    }

    async fn open_entry(&self, strat: &Strategy, guard_duplicates: bool) -> Result<(), String> {
        let (mut settings, method, auto_lots, cfg) = {
            let Some(d) = self.doc() else { return Err("state lock".into()) };
            (
                d.settings.clone(),
                d.method.clone(),
                d.auto_lots,
                d.order_cfg.clone(),
            )
        };
        // Real orders need the broker REST client; paper trades only need a live
        // quote (from the shared feed) and never touch Dhan's order APIs, so a
        // dropped broker session must not block a simulated entry.
        let (client, client_id) = if self.paper {
            (None, String::new())
        } else {
            (
                Some(self.dhan.session_client().await.ok_or("not connected to Dhan")?),
                self.dhan.session_client_id().await.ok_or("no client id")?,
            )
        };

        let mut strat = strat.clone();
        // Manual Order Placement card: the operator picked a concrete BUY/SELL
        // side. For an index that still has to resolve to an option contract, pin
        // the Option Type to the side's direction (BUY = CE, SELL = PE) so the
        // card's side and the resolved leg can never disagree.
        if strat.manual && (strat.instrument.eq_ignore_ascii_case("INDEX") || strat.exchange_segment.eq_ignore_ascii_case("IDX_I")) {
            settings.option_side = if strat.side.eq_ignore_ascii_case("SELL") { "PE".into() } else { "CE".into() };
        }
        let mut exch = if strat.exchange_segment.is_empty() { "NSE_FNO".to_string() } else { strat.exchange_segment.clone() };
        // Paper entries fill entirely from the shared live feed: never fall back
        // to a Dhan REST quote. A cold cache just means the subscription's first
        // tick has not landed yet, so wait for it briefly; only if it never
        // arrives does the scan retry on its next pass (paper stays REST-free).
        let mut ltp = self.ltp_of(strat.security_id, &exch);
        if ltp <= 0.0 && self.paper {
            self.dhan
                .subscribe_options(&[(strat.security_id, exch.clone())])
                .await;
            ltp = self.wait_feed_ltp(strat.security_id, &exch, 1_500).await;
        }
        if ltp <= 0.0 && !self.paper {
            ltp = self.fetch_ltp(strat.security_id, &exch).await;
        }
        if ltp <= 0.0 {
            return Err(if self.paper {
                "paper: no live feed LTP yet for entry".into()
            } else {
                "no LTP available".into()
            });
        }

        // Underlying/index strategies: resolve the option contract now from the
        // operator's strike preferences (ATM / ITM / OTM / strike mode).
        if strat.instrument.eq_ignore_ascii_case("INDEX") || strat.exchange_segment.eq_ignore_ascii_case("IDX_I") {
            let resolved = self.resolve_option_strategy(&strat, ltp, &settings, false);
            if let Some(res) = resolved {
                strat = res;
                exch = strat.exchange_segment.clone();
                // Stream the contract on the live websocket so paper gets its
                // premium tick without any REST call.
                self.dhan.subscribe_options(&[(strat.security_id, exch.clone())]).await;
                let al = self.ltp_of(strat.security_id, &exch);
                ltp = if al > 0.0 {
                    al
                } else if self.paper {
                    self.wait_feed_ltp(strat.security_id, &exch, 1_500).await
                } else {
                    self.fetch_ltp(strat.security_id, &exch).await
                };
                if ltp <= 0.0 {
                    return Err(if self.paper {
                        "paper: waiting for live feed LTP on resolved option".into()
                    } else {
                        "no LTP for resolved option".into()
                    });
                }
            }
        }
        // Phantom-profit guard: a long option is never worth less than its
        // intrinsic value, so a premium below intrinsic means the option's feed
        // print (or the underlying's) is stale or crossed. Booking an entry on
        // such a print is exactly what produced fake deep-ITM profits - filled at
        // a stale-low premium, then exited at the true higher one when the next
        // print landed. Skip the entry instead of trading on an inconsistent LTP.
        if option_strike_type(&strat.trading_symbol).is_some() && !self.option_premium_sane(&strat, ltp) {
            return Err("stale option LTP (premium below intrinsic); entry skipped".into());
        }
        // Execution-instant candle re-check: the candle can flip between the scan
        // and the fill (especially for the 200ms delayed paper entry), so the same
        // spot-candle rule is enforced here too - Top Gainer needs a green forming
        // candle, Top Loser a red one. Returns an error so the caller releases the
        // signal and retries on a later scan instead of consuming it.
        if let Some(reason) = self.spot_candle_gate_skip(&settings, &strat).await {
            return Err(format!("candle gate: {reason}"));
        }
        // Same-contract guard: never open a second entry on an option contract
        // that already has a running position. Checked here (after the contract is
        // fully resolved) so it covers index strategies too, and skipped for
        // Manual Order Placement (operator-driven) and for the explicit
        // "repeat while condition holds" stacking mode. When "one trade per signal"
        // governs this scanner leg it defers to the signal-scoped rule below, which
        // permits a genuinely new signal to re-enter the same strike.
        let once_signal_rules = guard_duplicates && settings.once_per_signal && strat.synthetic;
        if !once_signal_rules && self.duplicate_entry_blocked(&settings, strat.security_id, guard_duplicates) {
            return Err(format!(
                "duplicate: {} {} already running - second entry on the same strike skipped",
                strat.trading_symbol, strat.instrument
            ));
        }
        // "One trade per signal": the leg has already traded this exact contract in
        // the current pulse, and the pulse has not ended since, so hold the entry.
        // Engine-only (Manual Order Placement is exempt), matching the scan-loop
        // pre-check.
        if once_signal_rules && self.pulse_blocked(&strat.id, strat.security_id) {
            return Err(format!(
                "once-per-signal: {} already traded on this strike - waiting for a fresh filter signal",
                strat.trading_symbol
            ));
        }
        // Publish the exact contract this entry executes on (option premium for
        // index/F&O, or the strategy's own spot) for the Running Strategies view.
        self.record_strat_leg(&strat.id, "trade", &strat);

        let lot = if strat.lot > 0.0 {
            strat.lot
        } else if settings.lot_size > 0.0 {
            settings.lot_size
        } else {
            scrip::get()
                .and_then(|s| s.lot_for(&strat.trading_symbol, &strat.exchange_segment).map(|(l, _)| l))
                .unwrap_or(1.0)
        };
        let mut qty = (settings.lots.max(1.0) * lot).round() as i64;
        if auto_lots {
            // Auto Lots = min(volume% lots, OI% lots, real Dhan margin lots). The
            // two liquidity caps are operator-set percentages; the smaller lot
            // count wins (e.g. volume 2% -> 20 lots, OI 2% -> 10 lots => 10 lots).
            // Margin uses the operator-set share of the live available balance
            // (Engine controls "Margin to use"), never a fake default. When no
            // signal exists the manual lots are kept.
            let (_, oi, volume) = self.quote_fields(strat.security_id, &exch);
            let avail = self.margin_budget();
            if let Some(n) = calc_auto_lots(
                lot,
                ltp,
                oi,
                volume,
                avail,
                settings.auto_lot_oi_pct,
                settings.auto_lot_volume_pct,
            ) {
                qty = (n * lot).round() as i64;
            }
        }
        if qty <= 0 {
            return Err("computed quantity is zero".into());
        }

        // AI Smart margin gate: the next trade is blocked + warned when its
        // required margin (qty x price) exceeds the budget still available after
        // the running trades. Disabled when no budget is configured.
        if let Some(msg) = self.margin_block_for_qty(qty, ltp) {
            self.log("warn", &msg);
            return Err(msg);
        }

        let side = if strat.side.eq_ignore_ascii_case("SELL") { "SELL" } else { "BUY" };
        let underlying = underlying_of(&strat.trading_symbol, &exch);
        let freeze = scrip::freeze_qty(strat.security_id, &underlying);
        let cfg = cfg.get(&method).cloned().unwrap_or(json!({}));
        let corr = corr_id(&strat.id);

        // AI stop/target from recent volatility (ATR) when any AI risk mode is on;
        // `levels_for` then applies the exact precedence: RR > Manual Trail TP >
        // Manual TP % > AI TP, and Manual SL / Trail SL floor the stop.
        // Only fetch the ATR when `levels_for` will actually consume it, so a
        // manual SL / Trail SL entry never pays a candle round-trip before the
        // order goes out.
        let need_ai_sl = (settings.ai_sl || settings.sl_auto) && !settings.manual_sl && !settings.manual_trail_sl;
        let need_ai_trail = settings.ai_trail_tp && !settings.manual_trail_sl && !settings.manual_trail_tp;
        let need_ai_tp = (settings.ai_tp_pct || settings.ai_trail_tp) && !settings.manual_trail_tp;
        let (ai_sl, ai_tp, ai_trail) = if need_ai_sl || need_ai_trail || need_ai_tp {
            self.ai_levels(&strat, side, ltp).await.unwrap_or((0.0, 0.0, 0.0))
        } else {
            (0.0, 0.0, 0.0)
        };
        let (sl, tp, trail, trail_tp_pct, point_trail) =
            levels_for(&settings, &cfg, side, ltp, ai_sl, ai_tp, ai_trail);

        let mut order_ids: Vec<String> = Vec::new();
        let mut super_orders: Vec<String> = Vec::new();
        let mut broker = false;
        let mut filled = 0i64;

        // Paper mode: never touch the broker. The entry is simulated at the live
        // LTP with adverse slippage, rejected with the configured probability,
        // and - when filled - the full quantity is booked at the slipped price.
        // Every risk level (SL/TP/trail) is still computed above and enforced
        // locally by `manage_positions`.
        let is_buy_entry = side.eq_ignore_ascii_case("BUY");
        // F&O limit entry (paper only, BUY only - the algo never sells): mirror
        // how Dhan sends an order - above the market so the order is always
        // marketable and fills immediately, exactly like a market order.
        // `fno_limit_px` is 0 when the toggle is off or the side is a sell, and
        // it is never set below the entry.
        let mut fno_limit_px = 0.0;
        if self.paper && settings.fno_limit_order && is_buy_entry {
            fno_limit_px = fno_limit_buy_price(ltp);
        }
        let paper_fill = if self.paper {
            // A marketable limit is certain to fill, so the F&O mode never
            // simulates a rejection; the plain market path keeps the configured
            // rejection probability.
            let rej_pct = settings.paper_reject_pct.clamp(0.0, 100.0);
            if fno_limit_px <= 0.0
                && rej_pct > 0.0
                && paper_rand01(paper_seed(strat.security_id)) < rej_pct / 100.0
            {
                let msg = format!("paper entry rejected (simulated {rej_pct}% rejection rate)");
                self.log("warn", &msg);
                return Err(msg);
            }
            filled = qty;
            paper_fill_price(ltp, is_buy_entry, settings.paper_slippage_bps)
        } else {
            0.0
        };
        let exec_price = if self.paper { paper_fill } else { ltp };
        if !self.paper {
        let client = client.as_ref().ok_or("not connected to Dhan")?;
        match method.as_str() {
            "super" => {
                // Operator toggles are authoritative at the broker too. Dhan
                // requires a positive target/stop leg, so a disabled target is
                // parked far away instead of being invented from the method
                // config (the "target keeps firing after I disabled it" bug).
                // The Take-Profit target now comes only from RR / AI TP /
                // Manual Trail TP; the manual "Take Profit %" card is gone.
                let target = tp;
                let target = if target > 0.0 { target } else { price_off(ltp, side, 2.0) };
                // Stop leg: the resolved overall/AI SL first, then an explicitly
                // configured manual stop. With no SL configured a far protective
                // price is used so the leg never acts as a surprise tight stop.
                let stop = if sl > 0.0 {
                    sl
                } else if settings.manual_sl && cfg_f(&cfg, "stopLossPrice", 0.0) > 0.0 {
                    cfg_f(&cfg, "stopLossPrice", 0.0)
                } else if settings.manual_sl && cfg_f(&cfg, "slPct", 0.0) > 0.0 {
                    price_off(ltp, side, -cfg_f(&cfg, "slPct", 0.0) / 100.0)
                } else {
                    price_off(ltp, side, -0.5)
                };

                // "Super ko iceberg jaisa": when Auto Order Slicing is ticked,
                // split the total into exchange-freeze sized child super orders
                // (each keeps its own native target + stop leg).
                let auto_slice = cfg.get("autoSlice").and_then(|v| v.as_bool()).unwrap_or(false)
                    || cfg_f(&cfg, "autoSlice", 0.0) > 0.0;
                let slice_ov = cfg_f(&cfg, "sliceQty", 0.0) as i64;
                let mut chunk = if slice_ov > 0 { slice_ov } else if freeze > 0.0 { freeze as i64 } else { 0 };
                if !auto_slice || chunk <= 0 || chunk >= qty {
                    chunk = qty;
                }
                let super_px = {
                    let p = cfg_f(&cfg, "price", 0.0);
                    if p > 0.0 { p } else { round2(ltp) }
                };
                let mut remaining = qty;
                while remaining > 0 {
                    let piece = remaining.min(chunk).max(1);
                    remaining -= piece;
                    let body = json!({
                        "dhanClientId": client_id,
                        "transactionType": side,
                        "exchangeSegment": exch,
                        "productType": product_from(cfg_str(&cfg, "product", "INTRADAY")),
                        "orderType": order_type_from(cfg_str(&cfg, "orderType", "MARKET")),
                        "securityId": strat.security_id.to_string(),
                        "quantity": piece,
                        "price": super_px,
                        "targetPrice": round2(target.max(0.05)),
                        "stopLossPrice": round2(stop.max(0.05)),
                        "trailingJump": point_trail,
                        "correlationId": corr,
                    });
                    self.dhan.dhan_order_throttle().await;
                    let resp = client.place_super_order(&body).await.map_err(|e| e.to_string())?;
                    let oid = resp.get("orderId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    if !oid.is_empty() {
                        super_orders.push(oid);
                    }
                }
                broker = true;
                filled = qty;
            }
            "forever" => {
                let otype = cfg_str(&cfg, "orderType", "LIMIT");
                let px = cfg_f(&cfg, "price", 0.0);
                let px = if px > 0.0 { px } else { cfg_f(&cfg, "limitPrice", round2(ltp)) };
                let body = json!({
                    "dhanClientId": client_id,
                    "transactionType": side,
                    "exchangeSegment": exch,
                    "productType": product_from(cfg_str(&cfg, "product", "CNC")),
                    "orderType": otype,
                    "securityId": strat.security_id.to_string(),
                    "quantity": qty,
                    "price": px,
                    "triggerPrice": cfg_f(&cfg, "triggerPrice", 0.0),
                    "validity": cfg_str(&cfg, "validity", "DAY"),
                    "correlationId": corr,
                });
                self.dhan.dhan_order_throttle().await;
                let resp = client.place_forever(&body).await.map_err(|e| e.to_string())?;
                let oid = resp.get("orderId").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !oid.is_empty() {
                    order_ids.push(oid);
                }
            }
            _ => {
                let otype = cfg_str(&cfg, "orderType", "MARKET");
                let mut px = cfg_opt_f(&cfg, "price").or_else(|| cfg_opt_f(&cfg, "limitPrice"));
                let mut trig = cfg_opt_f(&cfg, "triggerPrice").or_else(|| cfg_opt_f(&cfg, "limitPrice"));
                if otype.eq_ignore_ascii_case("MARKET") {
                    px = None;
                }
                if otype.eq_ignore_ascii_case("LIMIT") && px.is_none() {
                    px = Some(round2(ltp));
                }
                if otype.eq_ignore_ascii_case("SL") || otype.eq_ignore_ascii_case("SL-M") {
                    if trig.is_none() {
                        trig = px;
                    }
                }
                let disclosed = cfg_opt_f(&cfg, "disclosedQty").map(|v| v as i64).filter(|v| *v > 0);
                let req = OrderRequest {
                    dhan_client_id: client_id,
                    correlation_id: Some(corr),
                    transaction_type: txn(side),
                    exchange_segment: seg_from(&exch),
                    product_type: product_from(cfg_str(&cfg, "product", "INTRADAY")),
                    order_type: order_type_from(otype.clone()),
                    validity: validity_from(cfg_str(&cfg, "validity", "DAY")),
                    security_id: strat.security_id.to_string(),
                    quantity: qty,
                    disclosed_quantity: disclosed,
                    price: px,
                    trigger_price: trig,
                    after_market_order: None,
                    amo_time: None,
                    bo_profit_value: None,
                    bo_stop_loss_value: None,
                };
                self.dhan.dhan_order_throttle().await;
                if method == "slice" || (freeze > 0.0 && qty as f64 > freeze) {
                    let resps = client.slice_order(&req).await.map_err(|e| e.to_string())?;
                    for r in resps {
                        order_ids.push(r.order_id.clone());
                    }
                } else {
                    let r = client.place_order(&req).await.map_err(|e| e.to_string())?;
                    order_ids.push(r.order_id);
                }
            }
        }
        }

        self.dhan
            .subscribe_options(&[(strat.security_id, exch.clone())])
            .await;

        let is_nifty_pick = self
            .nifty_pick_underlyings()
            .iter()
            .any(|u| u.eq_ignore_ascii_case(&underlying));
        let pos_id = gen_id("rtpos");
        let (entry_chg, entry_chg_pct) = self.quote_change(strat.security_id, &exch);
        let mut position = json!({
            "id": pos_id,
            "strategyId": strat.id,
            "strategyName": strat.name,
            "method": method,
            "securityId": strat.security_id,
            "exchangeSegment": exch,
            "instrument": strat.instrument,
            "tradingSymbol": strat.trading_symbol,
            "underlying": underlying,
            "side": side,
            "qty": qty,
            "lots": if lot > 0.0 { (qty as f64 / lot).round() } else { 0.0 },
            "lotSize": lot,
            "filledQty": filled,
            "entry": round2(exec_price),
            "fillPrice": round2(exec_price),
            "ltp": round2(exec_price),
            "sl": round2(sl),
            "overallSl": round2(sl),
            "tp": round2(tp),
            "trail": round2(trail),
            "pointTrail": round2(point_trail),
            "trailTp": round2(trail_tp_pct),
            "peakProfit": 0.0,
            "best": round2(exec_price),
            "orderIds": order_ids,
            "superOrders": super_orders,
            "broker": broker,
            "reconciled": false,
            "niftyPick": is_nifty_pick,
            "niftyDir": if is_nifty_pick { self.nifty_dir.load(Ordering::Relaxed) } else { 0 },
            "status": "running",
            "orderType": if fno_limit_px > 0.0 { "FNO_LIMIT" } else { "MARKET" },
            "limitPrice": fno_limit_px,
            "openedAt": now_ms(),
            "log": [{
                "t": now_ms(),
                "msg": if fno_limit_px > 0.0 {
                    format!("{method} {side} {qty} @ {exec_price} (F&O LIMIT {fno_limit_px})")
                } else {
                    format!("{method} {side} {qty} @ {exec_price}")
                },
            }],
        });
        // Entry-time premium change (sign + %), captured here so the executed-trade
        // list can show which sign the "+green premium" pick actually bought.
        position["ltpChange"] = json!(entry_chg);
        position["ltpChangePct"] = json!(entry_chg_pct);
        let placed = self.et_placed.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(mut d) = self.doc() {
            d.positions.push(position);
            let pending = self.et_pending.lock().ok().and_then(|mut m| m.remove(&strat.id));
            if let Some((met_at, at_meet)) = pending {
                d.entry_timing.insert(
                    0,
                    json!({
                        "strategyId": strat.id,
                        "strategyName": strat.name,
                        "side": side,
                        "metAt": met_at,
                        "entryAt": now_ms(),
                        "delayMs": (now_ms() - met_at).max(0),
                        "ordersWindow": (placed - 1) - at_meet,
                        "status": "entered",
                    }),
                );
                if d.entry_timing.len() > 60 {
                    d.entry_timing.truncate(60);
                }
            }
        }
        // Remember this strike as traded for the current signal pulse, so no
        // further entry is placed on it until the leg's gate resets and meets
        // again (a fresh signal). Only meaningful while the feature is on.
        if settings.once_per_signal && strat.synthetic {
            self.pulse_mark(&strat.id, strat.security_id);
        }
        self.save();
        self.log("info", &format!("opened {} {} {} @ {:.2}", strat.name, side, qty, ltp));
        Ok(())
    }

    async fn close_position(&self, id: &str, reason: &str, exit_price: f64) -> Result<(), String> {
        self.close_position_ex(id, reason, exit_price, None).await
    }

    /// Close a trade on the broker (`recovered = None`) or book an exit the broker
    /// already settled while we were disconnected (`recovered = Some(exit ids)`):
    /// the recovered path sends no new order and never leaves the row "pending".
    /// Both paths share the whole bookkeeping tail.
    async fn close_position_ex(
        &self,
        id: &str,
        reason: &str,
        exit_price: f64,
        recovered: Option<Vec<String>>,
    ) -> Result<(), String> {
        let (pos, settings, cfg_map, method) = {
            let Some(mut d) = self.doc() else { return Err("state lock".into()) };
            let Some(idx) = d.positions.iter().position(|p| js(p, "id") == id) else {
                return Ok(());
            };
            // Atomically claim the position so the 50ms guardian, the broker
            // reconciliation and an operator square-off can never fire two exit
            // orders for the same trade.
            if js(&d.positions[idx], "status").eq_ignore_ascii_case("closing") {
                return Ok(());
            }
            d.positions[idx]["status"] = json!("closing");
            let p = d.positions[idx].clone();
            (p, d.settings.clone(), d.order_cfg.clone(), d.method.clone())
        };
        let m = js(&pos, "method");
        let method = if m.is_empty() { method } else { m };
        let side = js(&pos, "side");
        let qty = ji(&pos, "qty");
        let sec_id = ji(&pos, "securityId");
        let exch = js(&pos, "exchangeSegment");
        let entry = jf(&pos, "fillPrice").max(jf(&pos, "entry"));
        // Never book a fake near-zero exit when the live mark is missing. The
        // option feed can briefly drop a strike (illiquid / unsubscribed), and
        // `paper_fill_price` floors a 0 mark to 0.05 - which would fake a 100%
        // loss on a fully healthy position. Fall back to the last stored LTP,
        // then to the entry price.
        let exit_price = if exit_price > 0.0 {
            exit_price
        } else {
            let stored = jf(&pos, "ltp");
            if stored > 0.0 {
                stored
            } else {
                entry
            }
        };
        let broker = jb(&pos, "broker");
        let cfg = cfg_map.get(&method).cloned().unwrap_or(json!({}));
        let is_buy = side == "BUY";
        // Paper exits fill at the mark with adverse slippage. The exit order is
        // the opposite side of the position (a BUY position sells, a SELL
        // position buys), so pass `!is_buy` to get the adverse direction.
        let exit_price = if self.paper {
            paper_fill_price(exit_price, !is_buy, settings.paper_slippage_bps)
        } else {
            exit_price
        };
        let pnl = if is_buy { (exit_price - entry) * qty as f64 } else { (entry - exit_price) * qty as f64 };

        // Send the exit order. Square-off by the operator is always allowed,
        // even if the engine was disarmed after the entry. Paper mode books the
        // exit locally at `exit_price` and never touches the broker.
        //
        // A recovered exit is the exception: the broker already flattened this
        // trade while the feed was down (the native Super-Order stop leg fired,
        // or it was closed manually), so we book it as-is and send nothing - a
        // new order here would flip the account into the opposite position.
        let mut exit_order_ids: Vec<String> = Vec::new();
        if let Some(recovered) = recovered {
            exit_order_ids = recovered;
        } else if !self.paper {
        if let Some(client) = self.dhan.session_client().await {
            let client_id = self.dhan.session_client_id().await.unwrap_or_default();
            if broker {
                // Cancel both protective legs, then flatten any residual entry.
                for so in jarr(&pos, "superOrders") {
                    if let Some(oid) = so.as_str() {
                        self.dhan.dhan_order_throttle().await;
                        let _ = client.cancel_super_order(oid).await;
                    }
                }
            }
            let exit_side = if is_buy { "SELL" } else { "BUY" };
            let req = OrderRequest {
                dhan_client_id: client_id,
                correlation_id: Some(corr_id(&format!("exit{id}"))),
                transaction_type: txn(exit_side),
                exchange_segment: seg_from(&exch),
                product_type: product_from(cfg_str(&cfg, "product", "INTRADAY")),
                order_type: OrderType::Market,
                validity: Validity::Day,
                security_id: sec_id.to_string(),
                quantity: qty,
                disclosed_quantity: None,
                price: None,
                trigger_price: None,
                after_market_order: None,
                amo_time: None,
                bo_profit_value: None,
                bo_stop_loss_value: None,
            };
            self.dhan.dhan_order_throttle().await;
            let underlying = js(&pos, "underlying");
            let freeze = scrip::freeze_qty(sec_id, &underlying);
            if freeze > 0.0 && qty as f64 > freeze {
                if let Ok(rs) = client.slice_order(&req).await {
                    for r in rs {
                        exit_order_ids.push(r.order_id);
                    }
                }
            } else if let Ok(r) = client.place_order(&req).await {
                exit_order_ids.push(r.order_id);
            }
        }
        }

        // Paper trades book an estimated round-trip charge so the wallet and the
        // realized total reflect net P&L exactly like the Closed Trades table —
        // unless the shared "Deduct Dhan charges" toggle is off, in which case the
        // trade is banked gross (netPnl stays null, no charges). Real trades
        // settle charges broker-side, so their net equals the gross.
        let instrument = js(&pos, "instrument");
        let trading_symbol = js(&pos, "tradingSymbol");
        let tc = if self.paper && settings.broker_charges {
            compute_charges_for_trade(
                entry,
                exit_price,
                qty as f64,
                &side,
                &instrument,
                &trading_symbol,
                pnl,
            )
        } else {
            None
        };
        let charges = tc.as_ref().map(|c| c.total).unwrap_or(0.0);
        let net_pnl = if self.paper {
            if settings.broker_charges {
                Some(round2(pnl - charges))
            } else {
                None
            }
        } else {
            Some(round2(pnl))
        };

        let mut closed = json!({
            "id": gen_id("rtclosed"),
            "positionId": id,
            "strategyId": js(&pos, "strategyId"),
            "strategyName": js(&pos, "strategyName"),
            "method": method,
            "securityId": sec_id,
            "exchangeSegment": exch,
            "instrument": instrument,
            "tradingSymbol": trading_symbol,
            "side": side,
            "qty": qty,
            "lots": ji(&pos, "lots"),
            "lotSize": jf(&pos, "lotSize"),
            "entry": round2(entry),
            "exit": round2(exit_price),
            "pnl": round2(pnl),
            "charges": charges,
            "netPnl": net_pnl,
            "chargesSegment": tc.as_ref().map(|c| c.segment).unwrap_or(""),
            "chargesEntry": tc.as_ref().map(|c| c.entry.to_json()).unwrap_or(serde_json::Value::Null),
            "chargesExit": tc.as_ref().map(|c| c.exit.to_json()).unwrap_or(serde_json::Value::Null),
            "reason": reason,
            "exitOrderIds": exit_order_ids,
            "exitPending": !exit_order_ids.is_empty(),
            "openedAt": ji(&pos, "openedAt"),
            "closedAt": now_ms(),
        });
        // Carry the entry-time premium change onto the closed record so the
        // executed-trade list keeps showing the sign the trade was taken on.
        closed["ltpChange"] = json!(jf(&pos, "ltpChange"));
        closed["ltpChangePct"] = json!(jf(&pos, "ltpChangePct"));
        if let Some(mut d) = self.doc() {
            d.positions.retain(|p| js(p, "id") != id);
            d.closed.push(closed);
        }
        self.save();
        self.log("info", &format!("closed {id} {reason} pnl {pnl:.2}"));
        Ok(())
    }

    async fn square_off_all(&self, reason: &str) {
        // Snapshot the broker's net positions (and the engine's own signed qty per
        // security) BEFORE flattening the engine book, so any leftover that the
        // engine does not account for - a manual Dhan trade, or the uncovered part
        // of a security the engine also holds - can be squared off too.
        let engine_signed: HashMap<i64, i64> = self
            .doc()
            .map(|d| {
                d.positions
                    .iter()
                    .filter_map(|p| {
                        let sid = ji(p, "securityId");
                        if sid <= 0 {
                            return None;
                        }
                        let qty = ji(p, "qty");
                        let signed = if js(p, "side") == "BUY" { qty } else { -qty };
                        Some((sid, signed))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let broker_snapshot = self
            .broker_positions
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();

        let ids: Vec<String> = {
            let Some(d) = self.doc() else { return };
            d.positions.iter().map(|p| js(p, "id")).collect()
        };
        for id in ids {
            let ltp = self.position_ltp(&id);
            let _ = self.close_position(&id, reason, ltp).await;
        }
        self.square_off_broker_residuals(&broker_snapshot, &engine_signed, reason)
            .await;
    }

    /// Flatten broker positions the engine does not account for (manual Dhan
    /// trades and any residual net qty on a security the engine also holds).
    async fn square_off_broker_residuals(
        &self,
        broker: &[Value],
        engine_signed: &HashMap<i64, i64>,
        reason: &str,
    ) {
        // Paper trading has no broker positions to flatten.
        if self.paper {
            return;
        }
        let Some(client) = self.dhan.session_client().await else {
            return;
        };
        let client_id = self.dhan.session_client_id().await.unwrap_or_default();
        for p in broker {
            let sid = ji(p, "securityId");
            if sid <= 0 {
                continue;
            }
            let net = ji(p, "netQty");
            let residual = net - engine_signed.get(&sid).copied().unwrap_or(0);
            if residual == 0 {
                continue;
            }
            let seg = js(p, "exchangeSegment");
            let product = js(p, "productType");
            let side = if residual > 0 { "SELL" } else { "BUY" };
            let qty = residual.abs();
            let req = OrderRequest {
                dhan_client_id: client_id.clone(),
                correlation_id: Some(corr_id(&format!("sq{sid}"))),
                transaction_type: txn(side),
                exchange_segment: seg_from(&seg),
                product_type: product_from(&product),
                order_type: OrderType::Market,
                validity: Validity::Day,
                security_id: sid.to_string(),
                quantity: qty,
                disclosed_quantity: None,
                price: None,
                trigger_price: None,
                after_market_order: None,
                amo_time: None,
                bo_profit_value: None,
                bo_stop_loss_value: None,
            };
            self.dhan.dhan_order_throttle().await;
            let underlying = js(p, "tradingSymbol");
            let freeze = scrip::freeze_qty(sid, &underlying);
            let sent = if freeze > 0.0 && qty as f64 > freeze {
                client.slice_order(&req).await.map(|_| ()).map_err(|e| e.to_string())
            } else {
                client.place_order(&req).await.map(|_| ()).map_err(|e| e.to_string())
            };
            match sent {
                Ok(()) => self.log(
                    "warn",
                    &format!("square-off {} {} {} ({}) manual/residual", side, qty, js(p, "tradingSymbol"), reason),
                ),
                Err(e) => self.log(
                    "error",
                    &format!("square-off {} {} failed: {e}", js(p, "tradingSymbol"), sid),
                ),
            }
        }
    }
}

impl RtDoc {
    fn strategy_time_ok(&self) -> bool {
        let now = ist_minutes();
        let sessions = &self.settings.trade_sessions;
        // A live session list is the single authority for entry timing (the UI
        // disables the old single gate while sessions exist), so DON'T AND the
        // legacy Start/No-trade envelope in - sessions replace it entirely.
        if sessions_active(sessions) {
            return sessions_gate_ok(sessions, now);
        }
        time_gate_ok(
            self.settings.start_after_enabled,
            &self.settings.start_after,
            self.settings.no_trade_after_enabled,
            &self.settings.no_trade_after,
            now,
        )
    }
}

/// True when the operator has at least one enabled, well-formed session window.
fn sessions_active(sessions: &[TradeSession]) -> bool {
    sessions.iter().any(|s| {
        if !s.enabled {
            return false;
        }
        match (hhmm_to_minutes(&s.start), hhmm_to_minutes(&s.end)) {
            (Some(a), Some(b)) => a < b,
            _ => false,
        }
    })
}

/// Multi-session intraday gate. When the operator has added sessions, a new
/// entry is only allowed while the current IST minute falls inside at least one
/// ENABLED, well-formed session (inclusive start/end). Disabled, malformed or
/// reversed (start >= end) sessions are ignored; an empty/all-invalid list means
/// "no extra window restriction" so a stray entry can never freeze the engine.
fn sessions_gate_ok(sessions: &[TradeSession], now: i64) -> bool {
    let mut any = false;
    for s in sessions {
        if !s.enabled {
            continue;
        }
        let (Some(a), Some(b)) = (hhmm_to_minutes(&s.start), hhmm_to_minutes(&s.end)) else {
            continue;
        };
        if a >= b {
            continue;
        }
        any = true;
        if now >= a && now <= b {
            return true;
        }
    }
    !any
}

/// Trade-time gate (NSE session), matching the old engine:
///  - "Start trading after" blocks entries before the start minute.
///  - "No trade after" blocks new entries after the cutoff.
/// A malformed time string is treated as "no gate" so a typo can never freeze
/// the engine. Position management / exits are never gated by this.
fn time_gate_ok(start_enabled: bool, start: &str, no_trade_enabled: bool, no_trade: &str, now: i64) -> bool {
    if start_enabled {
        if let Some(m) = hhmm_to_minutes(start) {
            if now < m {
                return false;
            }
        }
    }
    if no_trade_enabled {
        if let Some(m) = hhmm_to_minutes(no_trade) {
            if now > m {
                return false;
            }
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Strategy evaluation
// ---------------------------------------------------------------------------

/// Engine-control timeframe checkboxes (`1 min` / `5 min`), defaulting to 5min
/// when neither is ticked so the engine always has a runnable timeframe.
fn enabled_engine_tfs(settings: &Settings) -> Vec<&'static str> {
    let mut v = Vec::new();
    if settings.tf_1min {
        v.push("1min");
    }
    if settings.tf_5min {
        v.push("5min");
    }
    if v.is_empty() {
        v.push("5min");
    }
    v
}

fn normalize_engine_tf(tf: &str) -> Option<&'static str> {
    let t = tf.trim().to_lowercase().replace(' ', "");
    match t.as_str() {
        "1min" | "1m" => Some("1min"),
        "5min" | "5m" => Some("5min"),
        _ => None,
    }
}

/// Old `pickTimeframe`: a strategy's own 1/5-min timeframe wins over the engine
/// checkboxes; turning on "Use AST settings (own SL/trail/timeframe)" flips it so
/// the engine's ticked timeframe list drives every strategy.
fn engine_tf(settings: &Settings, strat: &Strategy) -> String {
    let enabled = enabled_engine_tfs(settings);
    if settings.use_own_settings {
        return enabled[0].to_string();
    }
    if let Some(own) = normalize_engine_tf(&strat.timeframe) {
        return own.to_string();
    }
    enabled[0].to_string()
}

/// Old `mtfPair`: the lower ticked timeframe triggers entry, the higher one must
/// confirm the trend. `None` unless at least two timeframes are ticked.
fn mtf_pair(settings: &Settings) -> Option<(String, String)> {
    let enabled = enabled_engine_tfs(settings);
    if enabled.len() < 2 {
        return None;
    }
    let entry = if *enabled.last().unwrap() == "5min" { "1min" } else { enabled[0] };
    let trend = if enabled.contains(&"5min") { "5min" } else { *enabled.last().unwrap() };
    if entry == trend {
        None
    } else {
        Some((entry.to_string(), trend.to_string()))
    }
}

/// NIFTY index security id: the instrument the trend-following engine reads the
/// selected straight-line indicators on.
const NIFTY_SEC: i64 = 13;

/// NIFTY trend-following filter assignment (the straight-line -> side mapper).
///
/// Each selected straight-line indicator is computed on the index candles and
/// classified by its *line direction* - the latest plotted value against the one
/// before it: rising -> bullish, falling -> bearish, flat -> neutral. Bullish
/// ids are assigned to the Top Gainer side, bearish ids to the Top Loser side,
/// so a bullish line trades gainers and a bearish line trades losers. Returns
/// `(bullish_ids, bearish_ids, net)` where `net = bullish - bearish`.
fn nifty_indicator_assignment(candles: &[Candle], conf: &[String], slope_len: i64, color: bool) -> (Vec<String>, Vec<String>, i64) {
    let st = algo_core::model::Settings::default();
    let window = nifty_slope_len(slope_len);
    let mut bull: Vec<String> = Vec::new();
    let mut bear: Vec<String> = Vec::new();
    for id in conf {
        let out = algo_core::compute(id, candles, &st);
        // Two reads:
        // * normal (color = false): the direction of the line's LATEST segment
        //   (newest point vs the one before it) - the slope the chart draws on the
        //   right edge. Reading a net window of older legs could report the stale
        //   side after a turn.
        // * color (color = true): the COLOUR the chart draws on the line's last
        //   closed candle - green = bullish, red = bearish. This is the colour the
        //   operator sees, read as-is (never derived from the value slope here).
        // `None` / `Flat` (line not resolved yet or flat) contributes nothing
        // instead of forcing a side, in both modes.
        let trend = if color {
            out.get(sl_output_index(id)).and_then(|s| color_read(s, 0))
        } else {
            out.first().and_then(|s| straight_line_trend(s, 0, window))
        };
        match trend {
            Some(SlTrend::Bull) => bull.push(id.clone()),
            Some(SlTrend::Bear) => bear.push(id.clone()),
            _ => {}
        }
    }
    let net = bull.len() as i64 - bear.len() as i64;
    (bull, bear, net)
}

/// The two colours the chart's straight-line overlays use for a bullish (rising)
/// leg and a bearish (falling) leg. These are the defaults of the indicator
/// `upColor` / `downColor` settings, and `algo_core` repaints the whole
/// straight-line family with exactly these before returning it, so the colour read
/// here is the colour the operator sees on the chart.
const SL_GREEN: &str = "#26a69a";
const SL_RED: &str = "#ef5350";

/// Read the COLOUR (not the value) of a straight-line series at its newest drawn
/// point: green -> `Bull`, red -> `Bear`. The renderer paints each segment with
/// its END point's colour (`draw_line_series`), so the segment the chart draws
/// across the latest closed candle carries the newest point's colour - exactly
/// the colour the operator sees on the line right now. `offset` skips that many
/// newest points.
///
/// A fixed `len - 2` index is wrong here: most straight-line series are SPARSE
/// (one point per vertex / pivot, not one per candle), so `len - 2` can be many
/// candles - even days - old and read the PREVIOUS leg's colour. The newest point
/// is the only one guaranteed to sit on the segment covering the latest candle.
/// Any other / missing colour reads `None` (non-blocking), so an un-drawn or
/// differently-coloured line never fabricates a side.
fn color_read(ser: &algo_core::model::SeriesOut, offset: usize) -> Option<SlTrend> {
    let n = ser.data.len();
    let i = n.checked_sub(offset + 1)?;
    let col = ser.data[i].color.as_deref()?.trim().to_ascii_lowercase();
    if col == SL_GREEN {
        Some(SlTrend::Bull)
    } else if col == SL_RED {
        Some(SlTrend::Bear)
    } else {
        None
    }
}

/// Output-series index that carries the drawn (coloured) line for a straight-line
/// indicator id. Most indicators draw on series 0; zigzag and combo master draw
/// the trendline on series 1.
fn sl_output_index(id: &str) -> usize {
    match id {
        "zzline" | "trendmaster" => 1,
        _ => 0,
    }
}

/// Map a "Straight Line Indicator Color Detection" filter token to the VISIBLE
/// coloured overlay id the chart draws (and its output index). This is
/// deliberately NOT the hidden "…state" id the plain `Sl*` direction filters read:
/// those hidden series carry no green/red colour, while the visible overlay does.
fn sl_color_series(base: &str) -> Option<(&'static str, usize)> {
    Some(match base {
        "ElliottWave" => ("wavefib", 0),
        "SupplyDemand" => ("supplydemand", 0),
        "PriceAction" => ("pastruct", 0),
        "ZigZag" => ("zzline", 1),
        "ComboMaster" => ("trendmaster", 1),
        "PaneConsensus" => ("panemaster", 0),
        "AutoTrendline" => ("autotrend", 0),
        "Pitchfork" => ("pitchfork", 0),
        "TrendProjection" => ("projline", 0),
        "GannFan" => ("gannfan", 0),
        "FibFan" => ("fibfan", 0),
        "Srema" => ("srema", 0),
        "Vl" => ("vl", 0),
        "Consensus" => ("slconsensus", 0),
        "Support" => ("supline", 0),
        "Resistance" => ("resline", 0),
        _ => return None,
    })
}

/// Colour read for one visible straight-line overlay at the given bar: green
/// passes the bull side, red passes the bear side. A missing/other colour is
/// non-blocking (`None`), matching the un-drawn-line behaviour of the `Sl*` gates.
fn color_line_dir(id: &str, idx: usize, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let out = ind_series(id, candles, &algo_core::model::Settings::default());
    let s = out.get(idx)?;
    match color_read(s, offset)? {
        SlTrend::Bull => Some(bull),
        SlTrend::Bear => Some(!bull),
        SlTrend::Flat => Some(true),
    }
}

/// Straight-line trend window length. At least 3 points so the net move has more
/// than a single step to average over; values above the available history clamp
/// on read.
fn nifty_slope_len(v: i64) -> usize {
    v.clamp(3, 200) as usize
}

/// Trend read of one straight-line series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlTrend {
    /// The drawn line is rising.
    Bull,
    /// The drawn line is falling.
    Bear,
    /// The line is flat; upstream keeps the previous side.
    Flat,
}

/// Read the CURRENT trend of a straight-line series: the direction of its latest
/// drawn segment (`newest point - previous point`), i.e. the slope the chart paints
/// on the right edge and the segment the newest colour belongs to.
///
/// A straight-line overlay is a chain of segments through its vertices (pivots for
/// zigzag / auto-trendline, structure points for price-action lines). The previous
/// read used the NET move across the last `window` points, which averages several
/// older legs together: a line that has clearly turned up (its latest segment
/// rising) could still net down or flat through the older legs and report the
/// WRONG side - exactly why NIFTY-trend stayed on PE through a bullish turn. The
/// latest segment is the line's actual, visible trend.
///
/// A step within half an average step (the recent noise floor) reads `Flat`, so a
/// genuinely flat line holds the previous side rather than flickering. `None` =
/// still warming up (fewer than 3 points / non-finite): warm-up must never
/// fabricate a direction.
fn straight_line_trend(ser: &algo_core::model::SeriesOut, offset: usize, window: usize) -> Option<SlTrend> {
    let end = ser.data.len().checked_sub(offset)?;
    if end < 3 {
        return None;
    }
    let newest = ser.data[end - 1].value;
    let previous = ser.data[end - 2].value;
    if !newest.is_finite() || !previous.is_finite() {
        return None;
    }
    let step = newest - previous;
    // Noise floor: half an average absolute step over the recent window.
    let need = window.max(3);
    let start = end.saturating_sub(need + 1);
    let vals: Vec<f64> = (start..end).map(|i| ser.data[i].value).collect();
    if !vals.iter().all(|v| v.is_finite()) {
        return None;
    }
    let sum_step: f64 = vals.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
    let mean_step = sum_step / (vals.len() - 1) as f64;
    let eps = mean_step * 0.5;
    if step > eps {
        Some(SlTrend::Bull)
    } else if step < -eps {
        Some(SlTrend::Bear)
    } else {
        Some(SlTrend::Flat)
    }
}

/// NIFTY trend timeframe: the timeframe the operator ticked for the engine
/// charts (`1 min` / `5 min`). Both ticked = `both`, i.e. the two must agree
/// before the trend is allowed to switch. Falls back to 5min when neither is
/// ticked, matching the engine default.
fn nifty_timeframe(settings: &Settings) -> String {
    let tfs = enabled_engine_tfs(settings);
    if tfs.len() >= 2 {
        "both".to_string()
    } else {
        tfs[0].to_string()
    }
}

fn nifty_dir_label(d: i64) -> &'static str {
    match d {
        1 => "BULLISH(CE)",
        -1 => "BEARISH(PE)",
        _ => "NEUTRAL",
    }
}

fn series_value(s: &algo_core::model::SeriesOut, from_end: usize) -> Option<f64> {    let n = s.data.len();
    if n <= from_end {
        return None;
    }
    let p = &s.data[n - 1 - from_end];
    Some(p.value)
}

fn conditions_met(conds: &[Condition], candles: &[Candle], offset: usize) -> bool {
    condition_detail(conds, candles, offset).0
}

/// Evaluate every condition and return `(all_pass, per-condition readout)`.
/// The readout powers the AI Smart Trading "Live Data Pool" panel so the
/// operator can see the exact indicator value and PASS/FAIL the engine used.
fn condition_detail(conds: &[Condition], candles: &[Candle], offset: usize) -> (bool, Vec<Value>) {
    // One memo scope for the whole condition list: a strategy that references
    // the same indicator in several conditions derives it once.
    let _sc = IndScope::enter();
    let mut all = !conds.is_empty();
    let mut rows = Vec::with_capacity(conds.len());
    for c in conds {
        let settings = if c.settings.is_object() {
            c.settings.as_object().cloned().unwrap_or_default().into_iter().collect()
        } else {
            Default::default()
        };
        let out = ind_series(&c.indicator, candles, &settings);
        let series = match c.source.parse::<usize>() {
            Ok(i) => out.get(i),
            Err(_) => out.first(),
        };
        let last = series.and_then(|s| series_value(s, offset));
        let prev = series.and_then(|s| series_value(s, offset + 1));
        let ok = match c.op.as_str() {
            "gt" => last.map(|l| l > c.value).unwrap_or(false),
            "gte" => last.map(|l| l >= c.value).unwrap_or(false),
            "lt" => last.map(|l| l < c.value).unwrap_or(false),
            "lte" => last.map(|l| l <= c.value).unwrap_or(false),
            "cross_up" => last.zip(prev).map(|(l, p)| p <= c.value && l > c.value).unwrap_or(false),
            "cross_down" => last.zip(prev).map(|(l, p)| p >= c.value && l < c.value).unwrap_or(false),
            "rising" => last.zip(prev).map(|(l, p)| l > p).unwrap_or(false),
            "falling" => last.zip(prev).map(|(l, p)| l < p).unwrap_or(false),
            _ => false,
        };
        if !ok {
            all = false;
        }
        rows.push(json!({
            "indicator": c.indicator,
            "source": c.source,
            "op": c.op,
            "target": c.value,
            "value": last,
            "prev": prev,
            "pass": ok,
        }));
    }
    (all, rows)
}

// ---------------------------------------------------------------------------
// Global indicator-filter gate (AI Smart Trading "Indicator filters")
//
// The old engine exposes ~148 Bullish/Bearish filter toggles that act as entry
// gates on top of a strategy's own conditions. They are persisted in
// `settings.filters`; here each enabled toggle on the strategy's side is
// evaluated against the same candle series. Unimplemented / research-stream
// toggles are treated as non-blocking so a missing indicator can never silently
// freeze live trading.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Ultrafast indicator memo layer (scoped + persistent)
// ---------------------------------------------------------------------------
// A single gate pass can evaluate 100+ filters, and each EMA/BB/Supertrend
// filter re-derives the very same indicator series from the same candle slice.
// Two layers collapse that:
//
//   * A *scoped* per-thread cache (`IndScope`) dedupes within one synchronous
//     pass (gate + conditions + dir-guard + fresh-meet). Keyed by slice pointer,
//     so it needs no content hash and dies with the pass.
//   * A *persistent* content-addressed cache (`IND_GLOBAL`) survives across
//     strategy legs and across engine ticks. Two slices with identical contents
//     (different Vecs, different strategies) share one derivation. The key holds
//     a full fingerprint of the candles, so a changed bar can never read a stale
//     series.
//
// Net effect: the engine derives each (candle-content, indicator, settings)
// combination once, instead of once per filter per strategy per tick.
type IndCacheKey = (usize, usize, u64, u64);
type IndGlobalKey = (u64, u64, u64);

struct IndScopeState {
    cache: HashMap<IndCacheKey, Arc<Vec<algo_core::model::SeriesOut>>>,
    fps: HashMap<(usize, usize), u64>,
}

thread_local! {
    static IND_CACHE: RefCell<Option<IndScopeState>> = const { RefCell::new(None) };
}

static IND_GLOBAL: OnceLock<Mutex<HashMap<IndGlobalKey, Arc<Vec<algo_core::model::SeriesOut>>>>> = OnceLock::new();
const IND_GLOBAL_CAP: usize = 8192;

/// How long a tick-maintained live candle series is trusted before the next
/// scan re-seeds it from REST. Kept a few seconds so a subscribed feed keeps the
/// hot path REST-free while an unsubscribe/quiet symbol still self-heals.
const LIVE_BAR_TTL: Duration = Duration::from_secs(2);

/// Fixed cadence at which the colour detection is re-read. Both the NIFTY-trend
/// colour confirmation and the `SlColor*` indicator-filter gate scan the line
/// colour on this interval instead of on every tick.
pub const COLOR_SCAN_MS: i64 = 1_000;

/// How long a leg's gate must stay falsely BEFORE it counts as a new signal. A
/// single stale / flickering scan must not end the current signal pulse, or the
/// "one trade per signal" rule would re-arm and repeat entries on the same
/// signal. Only a continuous failure of at least this long ends the signal and
/// lets the next meet begin a fresh one.
pub const SIGNAL_END_DEBOUNCE_MS: i64 = 3_000;

/// How long a newly-read NIFTY direction must persist before it replaces the
/// committed side. The assigned straight-line lines are read on every market tick
/// (20 ms) off the live forming bar, so a single wiggle - one line stepping back
/// across a pivot while the other sits flat - can net the opposite sign for a
/// few milliseconds. Committing that flicker flipped the leg/strikes CE<->PE
/// dozens of times a minute, so no side ever stayed armed long enough to trade.
/// A genuine turn persists well beyond this window and still flips.
pub const NIFTY_FLIP_CONFIRM_MS: i64 = 1_500;

/// A sampled colour-filter decision plus when it was taken.
#[derive(Clone)]
struct ColorSample {
    at_ms: i64,
    val: Option<bool>,
}

/// Candle-series identity for the colour sample cache: a filter key is unique per
/// instrument series (first bar time + length pins the series), so the same 5s
/// sample is reused across the strategies that share the candles.
type ColorSampleKey = (String, i64, usize, usize);

fn color_samples() -> &'static Mutex<HashMap<ColorSampleKey, ColorSample>> {
    static M: OnceLock<Mutex<HashMap<ColorSampleKey, ColorSample>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::with_capacity(256)))
}

/// True for the dedicated "Straight Line Indicator Color Detection" gate keys.
fn is_color_filter(k: &str) -> bool {
    k.starts_with("BullSlColor") || k.starts_with("BearSlColor")
}

/// True for ANY straight-line indicator filter the strict line gate reads:
///   * the "Straight Line Indicators" DIRECTION rows (`Sl*`, and the Volume line
///     `Vl`), which fire on the line's trend/slope, and
///   * the "Straight Line Indicator Color Detection" rows (`SlColor*`), which
///     fire on the line's green/red colour.
/// Both live in the Bullish/Bearish filter lists, so the gate covers whichever
/// of them the operator ticked. Non-line filters (arrows, OI trend, Pbg, EMA
/// families, ...) are excluded.
fn is_straight_line_filter(k: &str) -> bool {
    let base = k.strip_prefix("Bull").or_else(|| k.strip_prefix("Bear")).unwrap_or(k);
    base.starts_with("Sl") || base == "Vl"
}

/// Evaluate a colour filter at most once per `COLOR_SCAN_MS`, so the algo reads
/// the assigned straight-line overlays' colour on the fixed cadence rather than
/// re-deriving it on every tick. `None` (line not drawn yet) is cached too, so an
/// un-drawn line is not re-checked until the next scan. The pure `filter_eval`
/// path (used by the colour unit tests) is unaffected.
fn color_filter_sampled(
    k: &str,
    candles: &[Candle],
    offset: usize,
    cons: ConsensusCfg,
    sup: PivotTrendCfg,
    res: PivotTrendCfg,
) -> Option<bool> {
    if candles.is_empty() {
        return filter_eval_inner(k, candles, offset, cons, sup, res);
    }
    let key: ColorSampleKey = (k.to_string(), candles[0].time, candles.len(), offset);
    let now = now_ms();
    if let Ok(g) = color_samples().lock() {
        if let Some(s) = g.get(&key) {
            if now - s.at_ms < COLOR_SCAN_MS {
                return s.val;
            }
        }
    }
    let val = filter_eval_inner(k, candles, offset, cons, sup, res);
    if let Ok(mut g) = color_samples().lock() {
        if g.len() > 4096 {
            g.clear();
        }
        g.insert(key, ColorSample { at_ms: now, val });
    }
    val
}

/// Evaluate one gate filter, routing the colour-detection keys through the fixed
/// colour scan so the indicator-filter colour detection shares the same cadence as
/// the NIFTY trend read. Every other filter stays tick-native.
#[inline]
fn gate_filter_eval(
    k: &str,
    candles: &[Candle],
    offset: usize,
    cons: ConsensusCfg,
    sup: PivotTrendCfg,
    res: PivotTrendCfg,
) -> Option<bool> {
    if is_color_filter(k) {
        color_filter_sampled(k, candles, offset, cons, sup, res)
    } else {
        filter_eval_inner(k, candles, offset, cons, sup, res)
    }
}

fn ind_global() -> &'static Mutex<HashMap<IndGlobalKey, Arc<Vec<algo_core::model::SeriesOut>>>> {
    IND_GLOBAL.get_or_init(|| Mutex::new(HashMap::with_capacity(512)))
}

#[inline]
fn global_get(key: &IndGlobalKey) -> Option<Arc<Vec<algo_core::model::SeriesOut>>> {
    ind_global().lock().ok().and_then(|g| g.get(key).cloned())
}

#[inline]
fn global_put(key: IndGlobalKey, v: Arc<Vec<algo_core::model::SeriesOut>>) {
    if let Ok(mut g) = ind_global().lock() {
        if g.len() >= IND_GLOBAL_CAP {
            g.clear();
        }
        g.insert(key, v);
    }
}

#[cfg(test)]
fn reset_global_ind_cache() {
    if let Ok(mut g) = ind_global().lock() {
        g.clear();
    }
}

/// RAII scope. Nested scopes share the outermost cache; only the outermost
/// drop clears it, so a helper that opens its own scope can never wipe a
/// caller's memo mid-pass.
struct IndScope {
    outer: bool,
}

impl IndScope {
    #[inline]
    fn enter() -> Self {
        let outer = IND_CACHE.with(|c| {
            let mut slot = c.borrow_mut();
            if slot.is_some() {
                false
            } else {
                *slot = Some(IndScopeState {
                    cache: HashMap::with_capacity(96),
                    fps: HashMap::with_capacity(4),
                });
                true
            }
        });
        IndScope { outer }
    }
}

impl Drop for IndScope {
    #[inline]
    fn drop(&mut self) {
        if self.outer {
            IND_CACHE.with(|c| *c.borrow_mut() = None);
        }
    }
}

#[inline]
fn hash_str(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[inline]
fn hash_settings(s: &algo_core::model::Settings) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in s.iter() {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    h.finish()
}

/// Full content fingerprint of a candle slice (the persistent cache key's data
/// component). `to_bits` keeps the hash stable for NaN/-0.0 forms.
fn candle_fingerprint(candles: &[Candle]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    candles.len().hash(&mut h);
    for c in candles {
        c.time.hash(&mut h);
        c.open.to_bits().hash(&mut h);
        c.high.to_bits().hash(&mut h);
        c.low.to_bits().hash(&mut h);
        c.close.to_bits().hash(&mut h);
        c.volume.to_bits().hash(&mut h);
    }
    h.finish()
}

/// Single choke point for the (expensive) indicator derivation, so the memo
/// layer can be instrumented in tests without touching every caller.
#[inline]
fn do_compute(id: &str, candles: &[Candle], settings: &algo_core::model::Settings) -> Vec<algo_core::model::SeriesOut> {
    #[cfg(test)]
    IND_COMPUTE_CALLS.with(|c| c.set(c.get() + 1));
    algo_core::compute(id, candles, settings)
}

#[cfg(test)]
thread_local! {
    static IND_COMPUTE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn ind_compute_calls() -> usize {
    IND_COMPUTE_CALLS.with(|c| c.get())
}

#[cfg(test)]
fn reset_ind_compute_calls() {
    IND_COMPUTE_CALLS.with(|c| c.set(0));
}

/// Memoised `algo_core::compute`: scoped cache -> persistent content cache ->
/// real derivation. Outside an `IndScope` it is still globally shared, so every
/// caller stays correct and still benefits from cross-strategy reuse.
#[inline]
fn ind_series(id: &str, candles: &[Candle], settings: &algo_core::model::Settings) -> Arc<Vec<algo_core::model::SeriesOut>> {
    let idh = hash_str(id);
    let seth = hash_settings(settings);
    let ptr = candles.as_ptr() as usize;
    let len = candles.len();
    let skey: IndCacheKey = (ptr, len, idh, seth);
    IND_CACHE.with(|c| {
        let mut slot = c.borrow_mut();
        match slot.as_mut() {
            Some(st) => {
                if let Some(hit) = st.cache.get(&skey) {
                    return hit.clone();
                }
                let fp = match st.fps.get(&(ptr, len)) {
                    Some(fp) => *fp,
                    None => {
                        let fp = candle_fingerprint(candles);
                        st.fps.insert((ptr, len), fp);
                        fp
                    }
                };
                let gkey: IndGlobalKey = (fp, idh, seth);
                if let Some(hit) = global_get(&gkey) {
                    st.cache.insert(skey, hit.clone());
                    return hit;
                }
                let v = Arc::new(do_compute(id, candles, settings));
                global_put(gkey, v.clone());
                st.cache.insert(skey, v.clone());
                v
            }
            None => {
                let fp = candle_fingerprint(candles);
                let gkey: IndGlobalKey = (fp, idh, seth);
                if let Some(hit) = global_get(&gkey) {
                    return hit;
                }
                let v = Arc::new(do_compute(id, candles, settings));
                global_put(gkey, v.clone());
                v
            }
        }
    })
}

fn ind_at(id: &str, candles: &[Candle], params: &[(&str, f64)], offset: usize) -> Option<f64> {
    let st: algo_core::model::Settings = params.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    let out = ind_series(id, candles, &st);
    out.first().and_then(|s| series_value(s, offset))
}

/// Value of the `idx`-th output series of an indicator (v0 = 0).
fn ind_n(id: &str, candles: &[Candle], params: &[(&str, f64)], idx: usize, offset: usize) -> Option<f64> {
    let st: algo_core::model::Settings = params.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    let out = ind_series(id, candles, &st);
    out.get(idx).and_then(|s| series_value(s, offset))
}

fn st_val(factor: f64, candles: &[Candle], offset: usize) -> Option<f64> {
    ind_at("supertrend", candles, &[("atrPeriod", 10.0), ("factor", factor)], offset)
}

/// EMA(1) crossed above/below an indicator line.
fn cross_close_ind(id: &str, params: &[(&str, f64)], candles: &[Candle], offset: usize, up: bool) -> Option<bool> {
    let c = ema1(candles, offset)?;
    let cp = ema1(candles, offset + 1)?;
    let v = ind_at(id, candles, params, offset)?;
    let vp = ind_at(id, candles, params, offset + 1)?;
    Some(if up { cp <= vp && c > v } else { cp >= vp && c < v })
}

/// Series A crossed above/below series B (same indicator, two settings).
fn cross_ind_ind(id: &str, pa: &[(&str, f64)], pb: &[(&str, f64)], candles: &[Candle], offset: usize, up: bool) -> Option<bool> {
    let a = ind_at(id, candles, pa, offset)?;
    let ap = ind_at(id, candles, pa, offset + 1)?;
    let b = ind_at(id, candles, pb, offset)?;
    let bp = ind_at(id, candles, pb, offset + 1)?;
    Some(if up { ap <= bp && a > b } else { ap >= bp && a < b })
}

/// Bollinger middle band (v1) and Price-Channel midpoint ((v0+v1)/2).
fn bb_mid(candles: &[Candle], offset: usize) -> Option<f64> {
    ind_n("bb", candles, &[("length", 20.0), ("mult", 2.0)], 1, offset)
}
fn pc_mid(candles: &[Candle], offset: usize) -> Option<f64> {
    let h = ind_n("pc", candles, &[("length", 20.0)], 0, offset)?;
    let l = ind_n("pc", candles, &[("length", 20.0)], 1, offset)?;
    Some((h + l) / 2.0)
}

fn close_at(candles: &[Candle], offset: usize) -> Option<f64> {
    let n = candles.len();
    if n <= offset {
        None
    } else {
        Some(candles[n - 1 - offset].close)
    }
}

/// The candlestick value used by the crossover helpers, expressed as EMA(1).
///
/// A 1-period EMA is, by definition, the current bar, so the "EMA1" in
/// "EMA21 crossed below EMA1" / "VWAP cross below EMA1" is just the candle.
/// Only the cross-above/cross-below helpers read this; level, trend and gap
/// confirmations keep using the raw candle close/body instead.
fn ema1(candles: &[Candle], offset: usize) -> Option<f64> {
    close_at(candles, offset)
}

fn ema_val(n: i64, candles: &[Candle], offset: usize) -> Option<f64> {
    if n <= 1 {
        // EMA(1) is the candlestick (bar value).
        ema1(candles, offset)
    } else {
        ind_at("ema", candles, &[("length", n as f64)], offset)
    }
}

fn parse_pair(s: &str) -> Option<(i64, i64)> {
    let mut it = s.split('_');
    let a = it.next()?.parse().ok()?;
    let b = it.next()?.parse().ok()?;
    Some((a, b))
}

/// Candlestick rising/falling between the live bar and the previous bar.
fn rising_close(candles: &[Candle], offset: usize) -> bool {
    matches!((close_at(candles, offset), close_at(candles, offset + 1)), (Some(a), Some(b)) if a > b)
}
fn falling_close(candles: &[Candle], offset: usize) -> bool {
    matches!((close_at(candles, offset), close_at(candles, offset + 1)), (Some(a), Some(b)) if a < b)
}

fn cross_close_ema(n: i64, candles: &[Candle], offset: usize, up: bool) -> Option<bool> {
    let c = ema1(candles, offset)?;
    let cp = ema1(candles, offset + 1)?;
    let e = ema_val(n, candles, offset)?;
    let ep = ema_val(n, candles, offset + 1)?;
    Some(if up { cp <= ep && c > e } else { cp >= ep && c < e })
}

fn cross_close_vwap(candles: &[Candle], offset: usize, up: bool) -> Option<bool> {
    let c = ema1(candles, offset)?;
    let cp = ema1(candles, offset + 1)?;
    let v = ind_at("vwap", candles, &[], offset)?;
    let vp = ind_at("vwap", candles, &[], offset + 1)?;
    Some(if up { cp <= vp && c > v } else { cp >= vp && c < v })
}

/// Gap direction from the raw candle body: a bullish gap opens above the prior
/// bar's high, a bearish gap opens below the prior bar's low.
fn gap_dir(candles: &[Candle], offset: usize, up: bool) -> Option<bool> {
    let n = candles.len();
    if n <= offset + 1 {
        return None;
    }
    let cur = &candles[n - 1 - offset];
    let prev = &candles[n - 2 - offset];
    Some(if up { cur.open > prev.high } else { cur.open < prev.low })
}

fn line_dir(id: &str, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let a = ind_at(id, candles, &[], offset)?;
    let b = ind_at(id, candles, &[], offset + 1)?;
    Some(if bull { a > b } else { a < b })
}

/// Direction of a specific output series of an indicator (rising = bull).
fn series_dir(id: &str, params: &[(&str, f64)], idx: usize, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let a = ind_n(id, candles, params, idx, offset)?;
    let b = ind_n(id, candles, params, idx, offset + 1)?;
    Some(if bull { a > b } else { a < b })
}

/// Level comparison between two output series of one indicator (main vs signal).
fn series_pair(id: &str, params: &[(&str, f64)], ia: usize, ib: usize, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let a = ind_n(id, candles, params, ia, offset)?;
    let b = ind_n(id, candles, params, ib, offset)?;
    Some(if bull { a > b } else { a < b })
}

/// OI Trend gate (old AST `oitrend` indicator): direction of the candle-driven
/// hysteresis regime line (fast 9 / slow 21 / enter 0.45 / exit 0.12). Bullish
/// tolerates a rising line, bearish a falling line. This is the same `regime_line`
/// the chart's OI Trend overlay draws, so the gate matches what the operator sees.
fn oit_dir(bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let reg = algo_core::oi_trend::regime_line(candles, &algo_core::oi_trend::RegimeOpts::default());
    let n = reg.data.len();
    if n <= offset + 1 {
        return None;
    }
    let a = reg.data[n - 1 - offset].value;
    let b = reg.data[n - 2 - offset].value;
    Some(if bull { a > b } else { a < b })
}

/// Candlestick close vs one output series of an indicator (overlay Meet level).
fn close_vs_series(id: &str, params: &[(&str, f64)], idx: usize, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let c = close_at(candles, offset)?;
    let v = ind_n(id, candles, params, idx, offset)?;
    Some(if bull { c > v } else { c < v })
}

/// Overlay/structural indicator id + output index for the OBR (direction) and
/// Straight-Line families.
fn overlay_series(base: &str) -> Option<(&'static str, usize)> {
    Some(match base {
        "Hma" => ("hma", 0),
        "Tenkan" => ("ichimoku", 0),
        "Kijun" => ("ichimoku", 1),
        "SenkouA" => ("ichimoku", 2),
        "Keltner" => ("keltner", 1),
        "Donchian" => ("donchian", 1),
        "TrendCore" => ("vlcore", 0),
        _ => return None,
    })
}

fn sl_series(base: &str) -> Option<(&'static str, usize)> {
    Some(match base {
        "ElliottWave" | "SupplyDemand" => ("ewtrend", 0),
        "PriceAction" => ("patrend", 0),
        "ZigZag" => ("zzline", 1),
        "ComboMaster" => ("trendmaster", 1),
        "PaneConsensus" => ("panemaster", 0),
        "AutoTrendline" => ("autotrend", 0),
        "Pitchfork" => ("pitchfork", 0),
        "TrendProjection" => ("projline", 0),
        "GannFan" => ("gant", 0),
        "FibFan" => ("fibt", 0),
        "Srema" => ("sremat", 0),
        "Support" => ("supline", 0),
        "Resistance" => ("resline", 0),
        _ => return None,
    })
}

/// Straight Line Consensus gate: majority vote of the twelve straight-line
/// indicators (see `algo_core::indicators::sl_consensus_dir`). `offset` bars
/// back; a bar before the consensus resolves returns `None` so the caller treats
/// it as non-blocking, exactly like the other straight-line gates.
#[derive(Clone, Copy)]
struct ConsensusCfg {
    need: i32,
    confirm: usize,
    strength: f64,
}

impl Default for ConsensusCfg {
    fn default() -> Self {
        // Chart-indicator defaults (structural gating on).
        ConsensusCfg { need: 2, confirm: 5, strength: 5.0 }
    }
}

/// Indicator-filter settings for the Consensus filter rows. Defaults are 0 so
/// that the filter flips the instant the vote flips (no confirm wait, no
/// structural gate), exactly as configured in the filter settings panel.
fn consensus_cfg(settings: &Settings) -> ConsensusCfg {
    let need = settings.sc_min_agree.round() as i32;
    let confirm = settings.sc_confirm.round().max(0.0) as usize;
    let strength = settings.sc_strength;
    ConsensusCfg { need, confirm, strength }
}

/// Indicator-filter settings for the Support / Resistance Trendline filter rows.
/// Mirrors the geometry inputs of the `supline` / `resline` chart indicators so
/// the fitted line the gate reads is exactly the line the panel describes.
#[derive(Clone, Copy)]
struct PivotTrendCfg {
    strength: f64,
    atr_period: f64,
    min_pct: f64,
    tol_mult: f64,
    look: f64,
    fwd: f64,
    full_span: bool,
}

impl Default for PivotTrendCfg {
    fn default() -> Self {
        // Chart-indicator defaults (matches the registry `supline` / `resline`).
        PivotTrendCfg {
            strength: 5.0,
            atr_period: 14.0,
            min_pct: 0.05,
            tol_mult: 0.5,
            look: 12.0,
            fwd: 10.0,
            full_span: false,
        }
    }
}

impl PivotTrendCfg {
    fn to_settings(self) -> algo_core::model::Settings {
        let mut st = algo_core::model::Settings::new();
        st.insert("strength".into(), json!(self.strength));
        st.insert("atrPeriod".into(), json!(self.atr_period));
        st.insert("minPct".into(), json!(self.min_pct));
        st.insert("tolMult".into(), json!(self.tol_mult));
        st.insert("look".into(), json!(self.look));
        st.insert("fwd".into(), json!(self.fwd));
        st.insert("fullSpan".into(), json!(self.full_span));
        st
    }
}

fn support_trend_cfg(settings: &Settings) -> PivotTrendCfg {
    PivotTrendCfg {
        strength: settings.sup_strength,
        atr_period: settings.sup_atr_period,
        min_pct: settings.sup_min_pct,
        tol_mult: settings.sup_tol_mult,
        look: settings.sup_look,
        fwd: settings.sup_fwd,
        full_span: settings.sup_full_span,
    }
}

fn resistance_trend_cfg(settings: &Settings) -> PivotTrendCfg {
    PivotTrendCfg {
        strength: settings.res_strength,
        atr_period: settings.res_atr_period,
        min_pct: settings.res_min_pct,
        tol_mult: settings.res_tol_mult,
        look: settings.res_look,
        fwd: settings.res_fwd,
        full_span: settings.res_full_span,
    }
}

/// Direction of a trendline (`supline` / `resline`) fitted with the filter's own
/// geometry settings: `Some(true)` = rising, `Some(false)` = falling. `None` when
/// the line is not drawn yet, so the caller treats it as non-blocking exactly
/// like the other straight-line gates.
fn trend_line_dir(id: &str, cfg: PivotTrendCfg, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let st = cfg.to_settings();
    let out = ind_series(id, candles, &st);
    let s = out.first()?;
    let a = series_value(s, offset)?;
    let b = series_value(s, offset + 1)?;
    Some(if bull { a > b } else { a < b })
}

fn sl_consensus_at(candles: &[Candle], offset: usize, sc: ConsensusCfg) -> Option<i32> {
    let dirs = algo_core::indicators::sl_consensus_dir(candles, sc.need, sc.confirm, sc.strength);
    let n = dirs.len();
    if n <= offset {
        return None;
    }
    Some(dirs[n - 1 - offset])
}

fn sl_consensus_ok(bull: bool, candles: &[Candle], offset: usize, sc: ConsensusCfg) -> bool {
    match sl_consensus_at(candles, offset, sc) {
        Some(d) => {
            if bull {
                d >= 0
            } else {
                d <= 0
            }
        }
        None => true,
    }
}

fn sl_consensus_flip(bull: bool, candles: &[Candle], offset: usize, sc: ConsensusCfg) -> bool {
    let now = match sl_consensus_at(candles, offset, sc) {
        Some(d) => d,
        None => return true,
    };
    let prev = sl_consensus_at(candles, offset + 1, sc);
    let want = if bull { 1 } else { -1 };
    now == want && prev != Some(want)
}

/// Fresh trend-start arrow for one line: `true` only on the bar where the line's
/// direction just flipped to the requested side (bullish = rising, bearish =
/// falling). Uses the exact direction the AST "Straight Line" trend filters read,
/// so the arrow a trader sees on the chart is the arrow this gate detects.
///
/// A line that is not drawn yet (no resolved pivots / warm-up) is non-blocking,
/// exactly like the Straight Line trend filters, so an unresolvable indicator can
/// never freeze live entries.
fn arrow_flip(id: &str, idx: usize, bull: bool, candles: &[Candle], offset: usize) -> bool {
    match series_dir(id, &[], idx, bull, candles, offset) {
        Some(now) => {
            let prev = series_dir(id, &[], idx, bull, candles, offset + 1);
            now && prev != Some(true)
        }
        None => true,
    }
}

/// `Arrow*` gate key (prefix already stripped): a fresh up/down arrow on the
/// selected indicator. `Oit` uses the OI-Trend regime line, `TrendCore` the
/// Trend Core overlay, `Vl` the Volume Line; everything else maps through the
/// straight-line catalogue.
fn arrow_gate(rest: &str, bull: bool, candles: &[Candle], offset: usize, sc: ConsensusCfg) -> Option<bool> {
    if rest == "Consensus" {
        return Some(sl_consensus_flip(bull, candles, offset, sc));
    }
    if rest == "Oit" {
        return Some(match oit_dir(bull, candles, offset) {
            Some(now) => {
                let prev = oit_dir(bull, candles, offset + 1);
                now && prev != Some(true)
            }
            None => true,
        });
    }
    // Straight-line fits recompute to a constant slope, so a per-bar slope
    // change never fires and their arrow filter could never trade. Detect the
    // trend-start on the ATR zigzag structure instead - the same structure the
    // chart arrow for these indicators is drawn from.
    if matches!(rest, "Srema" | "AutoTrendline" | "Pitchfork" | "TrendProjection" | "GannFan" | "FibFan" | "Support" | "Resistance") {
        return algo_core::indicators::trend_flip_at(candles, offset, bull, 5.0);
    }
    let (id, idx) = if rest == "TrendCore" {
        overlay_series("TrendCore")?
    } else if rest == "Vl" {
        ("vl", 0)
    } else {
        sl_series(rest)?
    };
    Some(arrow_flip(id, idx, bull, candles, offset))
}

fn ema_family(base: &str, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    if let Some(rest) = base.strip_prefix("Meet") {
        let r = rest.strip_prefix("Ema")?;
        let (a, b) = parse_pair(r)?;
        let va = ema_val(a, candles, offset)?;
        let vb = ema_val(b, candles, offset)?;
        return Some(if bull { va > vb } else { va < vb });
    }
    if let Some(rest) = base.strip_prefix("EmaTrend") {
        let n: i64 = rest.parse().ok()?;
        let va = ema_val(n, candles, offset)?;
        let vp = ema_val(n, candles, offset + 1)?;
        return Some(if bull { va > vp } else { va < vp });
    }
    if let Some(rest) = base.strip_prefix("Ema") {
        if let Some((a, b)) = parse_pair(rest) {
            let fa = ema_val(a, candles, offset)?;
            let fap = ema_val(a, candles, offset + 1)?;
            let sb = ema_val(b, candles, offset)?;
            let sbp = ema_val(b, candles, offset + 1)?;
            return Some(if bull { fap <= sbp && fa > sb } else { fap >= sbp && fa < sb });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Canonical Pane-indicator sets for the global Pane filters
//
// The old engine scoped "Pane crossover" / "Pane lines all" to the pane
// indicators referenced by the running strategy's template. A single global
// filter set has no template, so the engine evaluates the canonical pane
// momentum pairs the UI documents ("Multi-Line Momentum Gap" family). A pair
// whose series is not computable yet (warm-up / too little history) is skipped;
// when no pair is computable the filter is treated as satisfied (non-blocking),
// exactly like the old engine's empty template-pane list.
// ---------------------------------------------------------------------------

/// (indicator id, main line index, signal line index) pane pairs.
const PANE_PAIRS: &[(&str, usize, usize)] = &[
    ("macd", 0, 1),
    ("ppo", 0, 1),
    ("smiio", 0, 1),
    ("stochrsi", 0, 1),
    ("aroon", 0, 1),
    ("vortex", 0, 1),
    ("adx", 1, 2),
];

/// Single-line pane indicators walked line-by-line by the "pane lines all"
/// direction filter, on top of every line of `PANE_PAIRS`.
const PANE_SINGLE_LINES: &[&str] = &[
    "ao", "smiio", "dpo", "mfi", "uo", "williamsR", "bbpct", "pvt", "ad", "smf",
    "cmf", "cci", "sqzmom", "rsi", "obv", "fisher", "tsi",
];

fn pane_out(id: &str, candles: &[Candle]) -> Arc<Vec<algo_core::model::SeriesOut>> {
    ind_series(id, candles, &algo_core::model::Settings::default())
}

fn pane_out_p(id: &str, params: &[(&str, f64)], candles: &[Candle]) -> Arc<Vec<algo_core::model::SeriesOut>> {
    let st: algo_core::model::Settings = params.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
    ind_series(id, candles, &st)
}

fn pane_pair_cross(id: &str, ia: usize, ib: usize, up: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let out = pane_out(id, candles);
    let a = out.get(ia).and_then(|s| series_value(s, offset))?;
    let ap = out.get(ia).and_then(|s| series_value(s, offset + 1))?;
    let b = out.get(ib).and_then(|s| series_value(s, offset))?;
    let bp = out.get(ib).and_then(|s| series_value(s, offset + 1))?;
    Some(if up { ap <= bp && a > b } else { ap >= bp && a < b })
}

fn pane_pair_level(id: &str, ia: usize, ib: usize, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let out = pane_out(id, candles);
    let a = out.get(ia).and_then(|s| series_value(s, offset))?;
    let b = out.get(ib).and_then(|s| series_value(s, offset))?;
    Some(if bull { a > b } else { a < b })
}

/// Multi-Line Momentum Gap generic level-hold (old `pbgBull`/`pbgBear`): the
/// main line sits above the signal, the main line is not falling and the signed
/// gap (main - signal) is not shrinking; exact mirror for bear.
fn pbg_hold(out: &[algo_core::model::SeriesOut], vk: usize, ck: usize, bull: bool, offset: usize) -> Option<bool> {
    let m = series_value(out.get(vk)?, offset)?;
    let s0 = series_value(out.get(ck)?, offset)?;
    let vp = series_value(out.get(vk)?, offset + 1)?;
    let s1 = series_value(out.get(ck)?, offset + 1)?;
    if !(m.is_finite() && s0.is_finite() && vp.is_finite() && s1.is_finite()) {
        return Some(true);
    }
    let gap = m - s0;
    let pgap = vp - s1;
    Some(if bull { gap > 0.0 && m >= vp && gap >= pgap } else { gap < 0.0 && m <= vp && gap <= pgap })
}

/// SMI Ergodic special "pair vs Histogram" mode (old `pbgBull`/`pbgBear` with
/// `pair: v2`, user-confirmed orientation): the bull row fires while SMI and its
/// signal both sit below the histogram, both point downward and the histogram
/// itself is falling; the bear row is the exact opposite.
fn pbg_smiio(out: &[algo_core::model::SeriesOut], bull: bool, offset: usize) -> Option<bool> {
    let v = series_value(out.get(0)?, offset)?;
    let s = series_value(out.get(1)?, offset)?;
    let h = series_value(out.get(2)?, offset)?;
    let vp = series_value(out.get(0)?, offset + 1)?;
    let sp = series_value(out.get(1)?, offset + 1)?;
    let hp = series_value(out.get(2)?, offset + 1)?;
    if !(v.is_finite() && s.is_finite() && h.is_finite() && vp.is_finite() && sp.is_finite() && hp.is_finite()) {
        return Some(true);
    }
    Some(if bull {
        v < h && s < h && v < vp && s < sp && h < hp
    } else {
        v > h && s > h && v > vp && s > sp && h > hp
    })
}

/// Vortex custom gap gate: VI+/- are a competing-direction duo, so the trend
/// signal requires BOTH lines committed (level hold, no fresh cross needed).
fn pbg_vortex(out: &[algo_core::model::SeriesOut], bull: bool, offset: usize) -> Option<bool> {
    let vi = series_value(out.get(0)?, offset)?;
    let vm = series_value(out.get(1)?, offset)?;
    let vip = series_value(out.get(0)?, offset + 1)?;
    let vmp = series_value(out.get(1)?, offset + 1)?;
    if !(vi.is_finite() && vm.is_finite() && vip.is_finite() && vmp.is_finite()) {
        return Some(true);
    }
    Some(if bull { vi > vm && vi > vip && vm < vmp } else { vi < vm && vi < vip && vm > vmp })
}

/// ADX custom gate: the ADX gauge must be trending, +DI/-DI must hold the trend
/// side and the +DI/-DI gap must be widening.
fn pbg_adx(out: &[algo_core::model::SeriesOut], bull: bool, offset: usize) -> Option<bool> {
    let a = series_value(out.get(0)?, offset)?;
    let p = series_value(out.get(1)?, offset)?;
    let m = series_value(out.get(2)?, offset)?;
    let ap = series_value(out.get(0)?, offset + 1)?;
    let pp = series_value(out.get(1)?, offset + 1)?;
    let mp = series_value(out.get(2)?, offset + 1)?;
    if !(a.is_finite() && p.is_finite() && m.is_finite() && ap.is_finite() && pp.is_finite() && mp.is_finite()) {
        return Some(true);
    }
    let gap = (p - m).abs();
    let pgap = (pp - mp).abs();
    Some(if bull {
        a > ap && p > m && p >= pp && m <= mp && gap >= pgap
    } else {
        a < ap && p < m && p <= pp && m >= mp && gap >= pgap
    })
}

/// One output series of an indicator as a plain value vector (NaN for warm-up).
fn indicator_values(id: &str, params: &[(&str, f64)], candles: &[Candle]) -> Vec<f64> {
    pane_out_p(id, params, candles)
        .first()
        .map(|s| s.data.iter().map(|p| p.value).collect())
        .unwrap_or_default()
}

enum SigKind {
    Ema(usize),
    Sma(usize),
}

/// Rolling EMA/SMA over a value vector (used for the single-line pane
/// indicators that the old engine exposed a signal line for: RSI/OBV/Fisher/TSI).
fn rolling_signal(vals: &[f64], kind: &SigKind) -> Vec<f64> {
    let n = vals.len();
    let mut out = vec![f64::NAN; n];
    match kind {
        SigKind::Ema(len) => {
            let len = *len;
            if len == 0 {
                return out;
            }
            let k = 2.0 / (len as f64 + 1.0);
            let mut prev = f64::NAN;
            let mut sum = 0.0;
            let mut cnt = 0usize;
            for i in 0..n {
                let v = vals[i];
                if v.is_nan() {
                    continue;
                }
                if prev.is_nan() {
                    sum += v;
                    cnt += 1;
                    if cnt >= len {
                        prev = sum / len as f64;
                        out[i] = prev;
                    }
                } else {
                    prev = v * k + prev * (1.0 - k);
                    out[i] = prev;
                }
            }
        }
        SigKind::Sma(len) => {
            let len = *len;
            if len == 0 {
                return out;
            }
            let mut q: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
            let mut sum = 0.0;
            for i in 0..n {
                let v = vals[i];
                q.push_back(v);
                if !v.is_nan() {
                    sum += v;
                }
                if q.len() > len {
                    if let Some(old) = q.pop_front() {
                        if !old.is_nan() {
                            sum -= old;
                        }
                    }
                }
                if q.len() == len && q.iter().all(|x| !x.is_nan()) {
                    out[i] = sum / len as f64;
                }
            }
        }
    }
    out
}

/// Level comparison of a single-line pane indicator's main line against a
/// rolling signal (old PB_CROSS_LEVEL gates for RSI/OBV/Fisher/TSI).
fn level_vs_signal(id: &str, params: &[(&str, f64)], kind: SigKind, bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let vals = indicator_values(id, params, candles);
    let n = vals.len();
    if n <= offset {
        return None;
    }
    let sig = rolling_signal(&vals, &kind);
    let v = vals[n - 1 - offset];
    let s = sig[n - 1 - offset];
    if v.is_nan() || s.is_nan() {
        return Some(true);
    }
    Some(if bull { v > s } else { v < s })
}

/// Every pane line agrees with the requested direction. Uncomputable lines are
/// skipped; the result is satisfied only when every computable line agrees.
fn pane_all_dirs(bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let mut computed = false;
    for (id, ia, ib) in PANE_PAIRS {
        let out = pane_out(id, candles);
        for idx in [*ia, *ib] {
            if let Some((a, b)) = out
                .get(idx)
                .and_then(|s| series_value(s, offset))
                .zip(out.get(idx).and_then(|s| series_value(s, offset + 1)))
            {
                computed = true;
                if (a > b) != bull {
                    return Some(false);
                }
            }
        }
    }
    for id in PANE_SINGLE_LINES {
        let out = pane_out(id, candles);
        for s in out.iter() {
            if let Some((a, b)) = series_value(s, offset).zip(series_value(s, offset + 1)) {
                computed = true;
                if (a > b) != bull {
                    return Some(false);
                }
            }
        }
    }
    let _ = computed;
    Some(true)
}

/// Auto Support/Resistance per-bar gap (old `autoSRGapSeries`): replay the
/// ATR-scaled ZigZag and return the signed gap from close to the live support
/// (bull) / resistance (bear) line, then report whether that gap has widened
/// over the recent window (>=4 usable points, last > first).
fn asr_gap_rising(bull: bool, candles: &[Candle], offset: usize) -> Option<bool> {
    let n = candles.len();
    if n < 4 {
        return None;
    }
    let atr_per = 14usize;
    let atr_mult = 2.0_f64;
    let min_pct = 0.15_f64;
    let mut atr: Vec<Option<f64>> = vec![None; n];
    let mut sum = 0.0_f64;
    for i in 0..n {
        let c = &candles[i];
        let tr = if i == 0 {
            c.high - c.low
        } else {
            let pc = candles[i - 1].close;
            (c.high - c.low).max((c.high - pc).abs()).max((c.low - pc).abs())
        };
        if i < atr_per {
            sum += tr;
            if i == atr_per - 1 {
                atr[i] = Some(sum / atr_per as f64);
            }
        } else {
            atr[i] = Some((atr[i - 1].unwrap_or(tr) * (atr_per as f64 - 1.0) + tr) / atr_per as f64);
        }
    }
    let th = |i: usize| -> f64 {
        let a = atr[i].unwrap_or(0.0) * atr_mult;
        let p = candles[i].close.abs() * (min_pct / 100.0);
        a.max(p)
    };
    let mut gap: Vec<Option<f64>> = vec![None; n];
    let mut dir = 1i32;
    let mut ext = candles[0].high;
    let mut last_pivot: Option<f64> = None;
    let mut run_min = candles[0].low;
    let mut run_max = candles[0].high;
    for i in 0..n {
        let c = &candles[i];
        if c.low < run_min {
            run_min = c.low;
        }
        if c.high > run_max {
            run_max = c.high;
        }
        if i > 0 {
            let t = th(i);
            if dir >= 0 {
                if c.high > ext {
                    ext = c.high;
                }
                if c.low <= ext - t {
                    last_pivot = Some(ext);
                    dir = -1;
                    ext = c.low;
                }
            } else {
                if c.low < ext {
                    ext = c.low;
                }
                if c.high >= ext + t {
                    last_pivot = Some(ext);
                    dir = 1;
                    ext = c.high;
                }
            }
        }
        let (sup, res) = if dir >= 0 {
            (last_pivot.unwrap_or(run_min), ext)
        } else {
            (ext, last_pivot.unwrap_or(run_max))
        };
        gap[i] = Some(if bull { c.close - sup } else { res - c.close });
    }
    let i = n - 1 - offset;
    let j0 = i.saturating_sub(6);
    let mut vals: Vec<f64> = Vec::new();
    for g in gap.iter().take(i + 1).skip(j0) {
        if let Some(v) = g {
            if v.is_finite() {
                vals.push(*v);
            }
        }
    }
    if vals.len() < 4 {
        return None;
    }
    Some(vals[vals.len() - 1] > vals[0])
}

/// Evaluate one filter toggle. `None` means "no implementation / research
/// stream" and is treated as non-blocking by the gate.
fn filter_eval(key: &str, candles: &[Candle], offset: usize) -> Option<bool> {
    filter_eval_inner(key, candles, offset, ConsensusCfg::default(), PivotTrendCfg::default(), PivotTrendCfg::default())
}

fn filter_eval_inner(
    key: &str,
    candles: &[Candle],
    offset: usize,
    sc: ConsensusCfg,
    sup: PivotTrendCfg,
    res: PivotTrendCfg,
) -> Option<bool> {
    let (bull, base) = if let Some(b) = key.strip_prefix("Bull") {
        (true, b)
    } else if let Some(b) = key.strip_prefix("Bear") {
        (false, b)
    } else {
        (true, key)
    };
    match base {
        "IncUp" => Some(rising_close(candles, offset)),
        "IncDown" => Some(falling_close(candles, offset)),
        "IncUpAll" => Some(ema_val(9, candles, offset)? > ema_val(21, candles, offset)? && ema_val(21, candles, offset)? > ema_val(35, candles, offset)?),
        "IncDownAll" => Some(ema_val(9, candles, offset)? < ema_val(21, candles, offset)? && ema_val(21, candles, offset)? < ema_val(35, candles, offset)?),
        "GapUp" => gap_dir(candles, offset, true),
        "GapDown" => gap_dir(candles, offset, false),
        "PaneCrossUp" | "PaneCrossDown" => {
            // Every canonical pane pair's main line crossing its signal line.
            let up = base == "PaneCrossUp";
            let mut computed = false;
            for (id, ia, ib) in PANE_PAIRS {
                if let Some(ok) = pane_pair_cross(id, *ia, *ib, up, candles, offset) {
                    computed = true;
                    if !ok {
                        return Some(false);
                    }
                }
            }
            // No pane pair could be computed: uncomputable, not a pass.
            if computed { Some(true) } else { None }
        }
        "PaneIncUpAll" => pane_all_dirs(true, candles, offset),
        "PaneIncDownAll" => pane_all_dirs(false, candles, offset),
        "MeetPaneCross" => {
            // Level form of the pane crossover: main line still above (bull) /
            // below (bear) the signal line for every canonical pane pair.
            let mut computed = false;
            for (id, ia, ib) in PANE_PAIRS {
                if let Some(ok) = pane_pair_level(id, *ia, *ib, bull, candles, offset) {
                    computed = true;
                    if !ok {
                        return Some(false);
                    }
                }
            }
            if computed { Some(true) } else { None }
        }
        // "Crossed-above indicator pair still above (level)": the primary overlay
        // line (HMA 20) above its doubled-settings twin (HMA 40).
        "MeetCross" => {
            let fast = ind_at("hma", candles, &[("length", 20.0)], offset)?;
            let slow = ind_at("hma", candles, &[("length", 40.0)], offset)?;
            Some(if bull { fast > slow } else { fast < slow })
        }
        "Asr" => asr_gap_rising(bull, candles, offset),
        // OI Trend is driven by the live option-chain OI series (the old engine's
        // `oitrend` overlay), which the candle-only filter gate has no access to.
        // It is wired as a non-blocking scope row here instead of a fabricated
        // candle proxy, so enabling it can never freeze live trading.
        "Oit" => oit_dir(bull, candles, offset),
        // Research-stream scope selectors (old `hasStreamFlags`): they choose
        // which research group the engine runs on a side, they are not candle
        // entry gates. `filter_gate` excludes them from the gate set and the
        // engine applies `stream_allows()` to scope the run set by strategy
        // group, so evaluating them here is inert (kept Some for the UI's
        // "every filter computes" contract).
        "Candle" | "Elliott" | "Indicator" | "Pane" | "Symmetry" | "Structure" | "Atr" => {
            Some(true)
        }
        "CrossUp" => cross_close_ema(9, candles, offset, true),
        "CrossDown" => cross_close_ema(9, candles, offset, false),
        "GtUp" | "GtDown" => close_at(candles, offset).zip(ema_val(9, candles, offset)).map(|(c, e)| c > e),
        "LtUp" | "LtDown" => close_at(candles, offset).zip(ema_val(9, candles, offset)).map(|(c, e)| c < e),
        "BbwInc" => line_dir("bbw", true, candles, offset),
        "Smf" => {
            let a = ind_n("smf", candles, &[("length", 14.0), ("signalLen", 9.0), ("volLen", 20.0)], 0, offset)?;
            let b = ind_n("smf", candles, &[("length", 14.0), ("signalLen", 9.0), ("volLen", 20.0)], 1, offset)?;
            Some(if bull { a > b } else { a < b })
        }
        "VwapCloseCrossAbove" => cross_close_vwap(candles, offset, true),
        "VwapCloseCrossBelow" => cross_close_vwap(candles, offset, false),
        "MeetCloseVwap" => close_at(candles, offset).zip(ind_at("vwap", candles, &[], offset)).map(|(c, v)| if bull { c > v } else { c < v }),
        "PbrAo" => line_dir("ao", bull, candles, offset),
        "PbrSmiio" => line_dir("smiio", bull, candles, offset),
        "PbrDpo" => line_dir("dpo", bull, candles, offset),
        "PbrMfi" => line_dir("mfi", bull, candles, offset),
        "PbrUo" => line_dir("uo", bull, candles, offset),
        "PbrWilliamsR" => line_dir("williamsR", bull, candles, offset),
        "PbrBbpct" => line_dir("bbpct", bull, candles, offset),
        "PbrPvt" => line_dir("pvt", bull, candles, offset),
        "PbrAd" => line_dir("ad", bull, candles, offset),
        "PbrSmf" => line_dir("smf", bull, candles, offset),
        "PbrCmf" => line_dir("cmf", bull, candles, offset),
        "PbrCci" => line_dir("cci", bull, candles, offset),
        "PbrSqzmom" => line_dir("sqzmom", bull, candles, offset),
        // Force Index is a non-blocking confirmer in the old engine (never
        // gates a trade on its own).
        "PbrElderforce" => Some(true),
        "PbrBbwUp" => line_dir("bbw", true, candles, offset),
        "PbrAtrUp" => line_dir("atr", true, candles, offset),
        "PbrVoloscUp" => line_dir("volosc", true, candles, offset),
        "St1CloseCrossAbove" => cross_close_ind("supertrend", &[("atrPeriod", 10.0), ("factor", 1.0)], candles, offset, true),
        "St1CloseCrossBelow" => cross_close_ind("supertrend", &[("atrPeriod", 10.0), ("factor", 1.0)], candles, offset, false),
        "MeetCloseSt" => close_at(candles, offset).zip(st_val(1.0, candles, offset)).map(|(c, s)| if bull { c > s } else { c < s }),
        "BbCrossAbove" | "BbCrossBelow" => {
            let up = base == "BbCrossAbove";
            let cross = cross_close_ind("bb", &[("length", 20.0), ("mult", 2.0)], candles, offset, up)?;
            let a = bb_mid(candles, offset)?;
            let b = bb_mid(candles, offset + 1)?;
            Some(cross && (if up { a > b } else { a < b }))
        }
        "PcCrossAbove" | "PcCrossBelow" => {
            let up = base == "PcCrossAbove";
            let m = pc_mid(candles, offset)?;
            let mp = pc_mid(candles, offset + 1)?;
            let c = ema1(candles, offset)?;
            let cp = ema1(candles, offset + 1)?;
            let cross = if up { cp <= mp && c > m } else { cp >= mp && c < m };
            Some(cross && (if up { m > mp } else { m < mp }))
        }
        "MeetCloseBb" => close_at(candles, offset).zip(bb_mid(candles, offset)).map(|(c, m)| if bull { c > m } else { c < m }),
        "MeetClosePc" => close_at(candles, offset).zip(pc_mid(candles, offset)).map(|(c, m)| if bull { c > m } else { c < m }),
        "MeetVl" => {
            let a = ind_n("vl", candles, &[("length", 14.0), ("signalLen", 9.0), ("volLen", 20.0)], 0, offset)?;
            let b = ind_n("vl", candles, &[("length", 14.0), ("signalLen", 9.0), ("volLen", 20.0)], 1, offset)?;
            Some(if bull { a > b } else { a < b })
        }
        _ => {
            // Fresh up/down arrow on a straight-line / overlay indicator.
            if let Some(rest) = base.strip_prefix("Arrow") {
                return arrow_gate(rest, bull, candles, offset, sc);
            }
            if let Some(rest) = base.strip_prefix("St10_") {
                if let Some((lo, hi)) = parse_pair(rest) {
                    return cross_ind_ind(
                        "supertrend",
                        &[("atrPeriod", 10.0), ("factor", lo as f64)],
                        &[("atrPeriod", 10.0), ("factor", hi as f64)],
                        candles,
                        offset,
                        bull,
                    );
                }
            }
            if let Some(rest) = base.strip_prefix("MeetSt10_") {
                if let Some((lo, hi)) = parse_pair(rest) {
                    let a = st_val(lo as f64, candles, offset)?;
                    let b = st_val(hi as f64, candles, offset)?;
                    return Some(if bull { a > b } else { a < b });
                }
            }
            // Overlay Indicator Behaviour (direction mirror) and Overlay Meet.
            if let Some(rest) = base.strip_prefix("Obr") {
                if let Some((id, idx)) = overlay_series(rest) {
                    return series_dir(id, &[], idx, bull, candles, offset);
                }
                return None;
            }
            if let Some(rest) = base.strip_prefix("MeetOvl") {
                if let Some((id, idx)) = overlay_series(rest) {
                    return close_vs_series(id, &[], idx, bull, candles, offset);
                }
                return None;
            }
            // Straight Line Indicator Color Detection: read the COLOUR the chart
            // draws on the line's last CLOSED candle (green = bullish, red =
            // bearish). The colour id is the VISIBLE overlay the operator sees -
            // not the hidden "…state" id the plain Sl* filters read, which carries
            // no green/red colour. `None` (un-drawn / differently-coloured line) is
            // non-blocking, matching the Sl* gates.
            if let Some(rest) = base.strip_prefix("SlColor") {
                return match sl_color_series(rest) {
                    Some((id, idx)) => Some(color_line_dir(id, idx, bull, candles, offset).unwrap_or(true)),
                    None => None,
                };
            }
            // Straight Line Indicators (single directional overlay lines).
            // A structural line that is not drawn yet (no resolved pivots) is
            // non-blocking, matching the old engine's "uncomputable => neutral".
            if let Some(rest) = base.strip_prefix("Sl") {
                if rest == "Consensus" {
                    return Some(sl_consensus_ok(bull, candles, offset, sc));
                }
                // Support / Resistance Trendline filters read the panel's own
                // geometry settings, so the fitted line the gate sees is the
                // line the operator configured.
                if rest == "Support" {
                    return Some(trend_line_dir("supline", sup, bull, candles, offset).unwrap_or(true));
                }
                if rest == "Resistance" {
                    return Some(trend_line_dir("resline", res, bull, candles, offset).unwrap_or(true));
                }
                if let Some((id, idx)) = sl_series(rest) {
                    return Some(series_dir(id, &[], idx, bull, candles, offset).unwrap_or(true));
                }
                return None;
            }
            if base == "Vl" {
                return line_dir("vl", bull, candles, offset);
            }
            // Multi-Line Momentum Gap (main vs signal level).
            if let Some(rest) = base.strip_prefix("Pbg") {
                return match rest {
                    "Macd" => series_pair("macd", &[], 0, 1, bull, candles, offset),
                    "Ppo" => series_pair("ppo", &[], 0, 1, bull, candles, offset),
                    "Stochrsi" => series_pair("stochrsi", &[], 0, 1, bull, candles, offset),
                    "Tsi" => level_vs_signal("tsi", &[], SigKind::Ema(13), bull, candles, offset),
                    "Rsi" => level_vs_signal("rsi", &[], SigKind::Ema(9), bull, candles, offset),
                    "Obv" => level_vs_signal("obv", &[], SigKind::Sma(30), bull, candles, offset),
                    "Fisher" => level_vs_signal("fisher", &[], SigKind::Ema(3), bull, candles, offset),
                    "Smiio" => pbg_smiio(&pane_out("smiio", candles), bull, offset),
                    "Smf" => pbg_hold(&pane_out("smf", candles), 0, 1, bull, offset),
                    "Aroon" => pbg_hold(&pane_out_p("aroon", &[("length", 25.0)], candles), 0, 1, bull, offset),
                    "Vortex" => pbg_vortex(&pane_out("vortex", candles), bull, offset),
                    "Adx" => pbg_adx(&pane_out("adx", candles), bull, offset),
                    _ => None,
                };
            }
            ema_family(base, bull, candles, offset)
        }
    }
}

fn strategy_is_bull(strat: &Strategy) -> bool {
    let c = strat.category.to_uppercase();
    if c == "BULLISH" {
        true
    } else if c == "BEARISH" {
        false
    } else {
        strat.side.eq_ignore_ascii_case("BUY")
    }
}

/// Spot forming-candle colour rule for the `spot_candle_gate`: a bullish leg
/// (Top Gainer) requires a GREEN candle (`close > open`), a bearish leg (Top
/// Loser) a RED one (`close < open`). A doji (`close == open`) is neither, so it
/// fails both directions and the entry is held until a coloured candle forms.
fn spot_candle_color_ok(bull: bool, open: f64, close: f64) -> bool {
    if bull {
        close > open
    } else {
        close < open
    }
}

/// True when the option leg must follow each strategy's own stock/filter side
/// instead of a global scanner direction ("Filter-side routing" - bullish
/// filters trade CE, bearish filters PE). When on, no NIFTY lock / Top-Movers
/// auto side / Run-in / Overall direction may flip the leg.
fn per_strategy_side(settings: &Settings) -> bool {
    settings.filter_side_route
}

/// Option side forced by per-strategy routing: bullish strategies trade CE,
/// bearish ones PE. `None` when neither feature is on, so callers keep their
/// normal multi-source resolution.
fn routed_option_side(settings: &Settings, strat: &Strategy) -> Option<&'static str> {
    // Operator-assigned leg (Top Movers "Indices" / "Commodities"): an explicitly
    // assigned CE/PE is authoritative and wins over every other direction source -
    // the NIFTY straight-line lock, the "Run Strategy In" override and the auto
    // scanner bias - for that instrument only. It also fixes the indicator-filter
    // side (CE = bullish filters, PE = bearish filters).
    if let Some(side) = assigned_leg(settings, strat) {
        return Some(side);
    }
    if per_strategy_side(settings) {
        Some(if strategy_is_bull(strat) { "CE" } else { "PE" })
    } else {
        None
    }
}

/// Parse the CE/PE leg out of one leg-assignment list (`movers_index_legs` /
/// `commodity_legs`). `None` when `sid` has no valid assignment.
fn leg_pick(list: &[MoversIndexLeg], sid: i64) -> Option<&'static str> {
    list.iter()
        .find(|l| l.security_id == sid)
        .and_then(|l| match l.side.trim().to_uppercase().as_str() {
            "CE" => Some("CE"),
            "PE" => Some("PE"),
            _ => None,
        })
}

/// The CE/PE leg the operator explicitly assigned to a Top Movers index
/// (`movers_index_legs`). `None` when the index has no valid assignment, in
/// which case it is skipped - index trading needs the operator's own trend
/// call, never an automatic side.
fn movers_index_leg(settings: &Settings, sid: i64) -> Option<&'static str> {
    leg_pick(&settings.movers_index_legs, sid)
}

/// The CE/PE leg the operator explicitly assigned to a commodity
/// (`commodity_legs`). `None` when the commodity has no valid assignment, in
/// which case it keeps the global scanner side.
fn commodity_leg(settings: &Settings, sid: i64) -> Option<&'static str> {
    leg_pick(&settings.commodity_legs, sid)
}

/// The operator-assigned CE/PE leg for a scanner index or commodity strategy,
/// whichever it is. `None` for F&O strategies and for any instrument with no
/// assignment. The assigned leg is authoritative for that instrument: it fixes
/// the executed contract AND selects which indicator filters apply.
fn assigned_leg(settings: &Settings, strat: &Strategy) -> Option<&'static str> {
    match strat_category(strat) {
        StratCat::Index => movers_index_leg(settings, strat.security_id),
        StratCat::Comm => commodity_leg(settings, strat.security_id),
        StratCat::Fno => None,
    }
}

/// Bullish/bearish side an operator index contributes to the scanner universe:
/// `Some(true)` = CE, `Some(false)` = PE, `None` = nothing (no leg assigned, so
/// the index is skipped).
fn index_target_side(settings: &Settings, sid: i64) -> Option<bool> {
    movers_index_leg(settings, sid).map(|s| s == "CE")
}

/// Bullish/bearish side an operator commodity contributes to the scanner
/// universe when it has an assigned leg: `Some(true)` = CE, `Some(false)` = PE,
/// `None` = no assignment (the commodity then follows the global side).
fn commodity_target_side(settings: &Settings, sid: i64) -> Option<bool> {
    commodity_leg(settings, sid).map(|s| s == "CE")
}

fn filter_is_bull(k: &str) -> bool {
    if k.starts_with("Bull") {
        true
    } else if k.starts_with("Bear") {
        false
    } else {
        k.to_uppercase().contains("UP")
    }
}
fn filter_is_bear(k: &str) -> bool {
    if k.starts_with("Bear") {
        true
    } else if k.starts_with("Bull") {
        false
    } else {
        k.to_uppercase().contains("DOWN")
    }
}

/// Which sides the scanner may build strategies for. A side that has no
/// matching filters can never be gated, so it must not be allowed; and when the
/// engine has committed to one trade side (explicit "Run Strategy In" override,
/// else the live scanner direction) only that side may run. This keeps the side
/// the filters gate identical to the side the option leg executes, so a bearish
/// filter can never fire a CE entry (and vice versa).
fn side_allowed(bull_side: bool, bear_side: bool, active: Option<&str>) -> (bool, bool) {
    let allow_bull = bull_side && active.map(|s| s == "CE").unwrap_or(true);
    let allow_bear = bear_side && active.map(|s| s == "PE").unwrap_or(true);
    (allow_bull, allow_bear)
}

/// "Arrow Detection" toggle (`BullArrow*` / `BearArrow*`). These are collapsed
/// into one OR-ed trigger per side inside the gate, so a single fresh arrow can
/// combine with the ordinary filters instead of every arrow needing to align.
fn is_arrow_flag(k: &str) -> bool {
    k.strip_prefix("Bull")
        .or_else(|| k.strip_prefix("Bear"))
        .map(|b| b.starts_with("Arrow"))
        .unwrap_or(false)
}

/// Research-stream scoping flags (old AST `STREAM_FLAG_GROUPS`). These are not
/// entry gates - they select which research group the engine runs on a side -
/// so they must be excluded from the indicator-filter gate.
fn stream_flag_group(k: &str) -> Option<&'static str> {
    match k {
        k if k == "BullCandle" || k == "BearCandle" => Some("candlestick"),
        k if k == "BullElliott" || k == "BearElliott" => Some("elliott"),
        k if k == "BullIndicator" || k == "BearIndicator" => Some("indicator"),
        k if k == "BullPane" || k == "BearPane" => Some("pane"),
        k if k == "BullSymmetry" || k == "BearSymmetry" => Some("symmetry"),
        k if k == "BullStructure" || k == "BearStructure" => Some("structure"),
        k if k == "BullAtr" || k == "BearAtr" => Some("atr"),
        _ => None,
    }
}

fn is_stream_flag(k: &str) -> bool {
    stream_flag_group(k).is_some()
}

/// Per-side research groups ticked in the "Research stream" rows. Returns an
/// empty vector for a side when nothing is ticked on it. Mirrors the old
/// `activeFilterGroups()`.
fn active_filter_groups(settings: &Settings) -> (Vec<&'static str>, Vec<&'static str>) {
    let mut bull = Vec::new();
    let mut bear = Vec::new();
    for (k, v) in settings.filters.iter() {
        if !*v {
            continue;
        }
        let Some(g) = stream_flag_group(k) else { continue };
        if k.starts_with("Bull") {
            bull.push(g);
        } else {
            bear.push(g);
        }
    }
    (bull, bear)
}

/// A strategy's research group (old `groupOfStrategy`). Empty / "other" always
/// runs regardless of the stream scoping.
fn strategy_group(s: &Strategy) -> &str {
    let g = s.group.trim();
    if g.is_empty() {
        "other"
    } else {
        g
    }
}

/// Stream scoping: when any stream flag is ticked on a side, only strategies on
/// that side whose group is in the ticked list run. "other" always runs.
fn stream_allows(settings: &Settings, strat: &Strategy) -> bool {
    let (bull, bear) = active_filter_groups(settings);
    if bull.is_empty() && bear.is_empty() {
        return true;
    }
    let g = strategy_group(strat);
    if g == "other" {
        return true;
    }
    let want = if strategy_is_bull(strat) { &bull } else { &bear };
    want.iter().any(|w| *w == g)
}

/// Facts behind a `filter_gate` decision, surfaced in the (throttled) gate-fail
/// diagnostics. Cheap to build (no allocation) so the hot path can ignore it.
#[derive(Clone, Copy, Default)]
struct GateFacts {
    pass: usize,
    total: usize,
    opposite: usize,
    opp_total: usize,
    strict: bool,
    brain: bool,
    veto: bool,
}

/// Indicator-filter gate for one side. Every enabled entry-gate mechanism is
/// combined with a strict AND, so they can all run together and none silently
/// disables another:
///   * "All together (strict AND)" -> every enabled filter on the strategy's
///     side must pass. This is gated ONLY by the checkbox; the Indicator-filters
///     run mode does NOT by itself force strict AND.
///   * AI Brain AUTO (score + conflict veto) -> the weighted own-side confluence
///     must also reach the threshold, and a strongly-opposite filter set (at
///     least half of that side's enabled filters agreeing) vetoes the entry.
/// Without strict AND the gate falls back to a majority of the enabled filters
/// (subject to Brain when it is enabled).
fn filter_gate(settings: &Settings, strat: &Strategy, candles: &[Candle], offset: usize) -> bool {
    filter_gate_facts(settings, strat, candles, offset).0
}

fn filter_gate_facts(settings: &Settings, strat: &Strategy, candles: &[Candle], offset: usize) -> (bool, GateFacts) {
    // Ultrafast path: a single memo scope spans the whole pass, so running the
    // 100+ side filters derives each shared indicator (EMA 9/21/35/..., BB, ST,
    // pane lines) once instead of once per filter. This is the hot path of the
    // engine's entry decision.
    let _sc = IndScope::enter();
    // "Premium chart only (run + trade)": every strategy is analysed on its
    // option-premium chart. A premium only rises while its own leg is in favour,
    // so the entry is always gated by the BULLISH filter set - even when the
    // operator routed the trade to the PE side. In this mode the bearish filters
    // are not applied at all (and cannot veto through the opposite-side check).
    let bull = settings.premium_only || strategy_is_bull(strat);
    let cons = consensus_cfg(settings);
    let sup = support_trend_cfg(settings);
    let res = resistance_trend_cfg(settings);
    let side_keys: Vec<&String> = settings
        .filters
        .iter()
        .filter(|(k, v)| **v && !is_stream_flag(k) && (if bull { filter_is_bull(k) } else { filter_is_bear(k) }))
        .map(|(k, _)| k)
        .collect();
    // Arrow Detection filters are a trigger group, not independent confirmations:
    // several arrows would almost never print on the same tick, so requiring them
    // all (strict AND) would make an entry impossible. Collapse the enabled arrow
    // filters for this side into ONE OR-ed condition ("any fresh arrow fires"),
    // then combine that single condition with the ordinary filters below.
    let arrow_keys: Vec<&String> = side_keys.iter().copied().filter(|k| is_arrow_flag(k)).collect();
    let norm_keys: Vec<&String> = side_keys.iter().copied().filter(|k| !is_arrow_flag(k)).collect();
    let arrow_pass = if arrow_keys.is_empty() {
        None
    } else {
        // Fail-closed: an arrow that cannot be computed yet does not count as a
        // trigger (previously `unwrap_or(true)` let an uncomputed arrow fire).
        Some(arrow_keys.iter().any(|k| filter_eval_inner(k, candles, offset, cons, sup, res).unwrap_or(false)))
    };
    let mut f = GateFacts {
        total: norm_keys.len() + usize::from(arrow_pass.is_some()),
        ..Default::default()
    };
    for k in &norm_keys {
        if gate_filter_eval(k, candles, offset, cons, sup, res).unwrap_or(false) {
            f.pass += 1;
        }
    }
    if arrow_pass == Some(true) {
        f.pass += 1;
    }
    // Premium-chart mode ignores the bearish filter set entirely, so it must not
    // be counted as opposite-side agreement (that would veto every PE entry).
    if !settings.premium_only {
        for (k, v) in settings.filters.iter() {
            if !*v || is_stream_flag(k) {
                continue;
            }
            let opp = if bull { filter_is_bear(k) } else { filter_is_bull(k) };
            if !opp {
                continue;
            }
            f.opp_total += 1;
            if gate_filter_eval(k, candles, offset, cons, sup, res) == Some(true) {
                f.opposite += 1;
            }
        }
    }
    // Opposite-side confirmation veto. The chart only "reads the other way" when
    // the OPPOSITE side's enabled filters agree STRICTLY more than this trade
    // side's own filters (relative veto, the default): opposite must lead by at
    // least one filter. A tie - or this side leading - never vetoes, so a
    // symmetric Bull+Bear filter set can no longer deadlock into "no trade at
    // all" on both sides. Mode `off` disables the veto entirely and lets the
    // own-side strict / majority / Brain gate stand alone.
    let opposite_veto_on = !settings.opposite_veto.eq_ignore_ascii_case("off");
    if opposite_veto_on && f.opp_total > 0 && f.opposite > f.pass {
        f.veto = true;
        return (false, f);
    }
    // "All together (strict AND)" requires every enabled filter on the
    // strategy's side to pass.
    f.strict = settings.all_in_one;
    if f.strict && f.pass != f.total {
        return (false, f);
    }
    // AI Brain AUTO layers on top of (never instead of) the strict/majority
    // result: the weighted confluence must clear the threshold.
    f.brain = settings.brain_mode.eq_ignore_ascii_case("auto");
    if f.brain {
        if f.total > 0 && (f.pass as f64 / f.total as f64 * 100.0) < settings.brain_threshold as f64 {
            return (false, f);
        }
        return (true, f);
    }
    if f.strict {
        return (true, f);
    }
    if f.total == 0 {
        // No same-side filter is ticked. Non-blocking only when the operator has
        // enabled no filters at all; if opposite filters are enabled there is no
        // same-side confirmation, so the entry is blocked.
        return (f.opp_total == 0, f);
    }
    // Not strict AND: a majority of the enabled filters must still agree.
    (f.pass * 2 > f.total, f)
}

/// One-line breakdown of a `filter_gate` decision for the gate-fail log, so the
/// operator can see exactly which mechanism (strict AND / brain threshold /
/// conflict veto / majority) allowed or blocked the entry.
fn filter_gate_explain(settings: &Settings, strat: &Strategy, candles: &[Candle], offset: usize) -> String {
    let (ok, f) = filter_gate_facts(settings, strat, candles, offset);
    let score = if f.total > 0 { f.pass as f64 / f.total as f64 * 100.0 } else { 100.0 };
    format!(
        "ok={ok} pass={}/{} score={:.0}% strict={} brain={} veto={} opp={}/{} thr={}",
        f.pass, f.total, score, f.strict, f.brain, f.veto, f.opposite, f.opp_total, settings.brain_threshold
    )
}

/// Strict straight-line gate (Indicator-filters mode, scanner / NIFTY-trend legs
/// only). When `strict_line_color` is ON, the leg must be confirmed by EVERY
/// straight-line filter ticked on its OWN side, from BOTH sections of the
/// Bullish/Bearish lists:
///   * "Straight Line Indicators" - the line DIRECTION rows (`BullSl*` / `BearSl*`
///     and the Volume line `BullVl` / `BearVl`) must trend the leg's way.
///   * "Straight Line Indicator Color Detection" - the line COLOUR rows
///     (`BullSlColor*` / `BearSlColor*`) must read GREEN on the CE side, RED on
///     the PE side.
/// A ticked line reading the opposite way vetoes the entry outright, so a
/// majority / AI-Brain gate can no longer let one dissenting line slip a leg
/// through. Returns `true` (non-blocking) when the feature is off, when the
/// strategy is not a scanner leg, or when no line filter is ticked on that side.
fn strict_line_gate_ok(settings: &Settings, strat: &Strategy, candles: &[Candle], offset: usize) -> bool {
    if !settings.strict_line_color || !strat.synthetic {
        return true;
    }
    // NIFTY Trend Following owns the opposite-side block for its universe: the
    // assigned NIFTY confirmation indicators decide which side may run (via the
    // `niftyNetStrict` / `niftyAllAgreeStrict` gates, which drop the opposite
    // side at scanner level). The manually ticked straight-line filters are the
    // entry (majority) rule only, so their all-agree gate is skipped here while
    // NIFTY trend is on - otherwise three ticked lines would need to agree on
    // every stock and block essentially every trade.
    if settings.nifty_trend_on {
        return true;
    }
    let _sc = IndScope::enter();
    let bull = settings.premium_only || strategy_is_bull(strat);
    let cons = consensus_cfg(settings);
    let sup = support_trend_cfg(settings);
    let res = resistance_trend_cfg(settings);
    for (k, v) in settings.filters.iter() {
        if !*v || is_stream_flag(k) || !is_straight_line_filter(k) {
            continue;
        }
        let on_side = if bull { filter_is_bull(k) } else { filter_is_bear(k) };
        if !on_side {
            continue;
        }
        if gate_filter_eval(k, candles, offset, cons, sup, res) == Some(false) {
            return false;
        }
    }
    true
}

/// Direction Guard: block the entry when price action + EMA9/21 + EMA21/35 +
/// Supertrend(10,3) + close-vs-VWAP give a clear opposite majority (>=3 votes).
fn direction_opposite(settings: &Settings, strat: &Strategy, candles: &[Candle], offset: usize) -> bool {
    if !settings.dir_guard {
        return false;
    }
    let _sc = IndScope::enter();
    let bull = strategy_is_bull(strat);
    let mut opp = 0usize;
    if let (Some(a), Some(b)) = (ema_val(9, candles, offset), ema_val(21, candles, offset)) {
        if (a > b) != bull {
            opp += 1;
        }
    }
    if let (Some(a), Some(b)) = (ema_val(21, candles, offset), ema_val(35, candles, offset)) {
        if (a > b) != bull {
            opp += 1;
        }
    }
    if let (Some(c), Some(st)) = (close_at(candles, offset), st_val(1.0, candles, offset)) {
        if (c > st) != bull {
            opp += 1;
        }
    }
    if let (Some(c), Some(v)) = (close_at(candles, offset), ind_at("vwap", candles, &[], offset)) {
        if (c > v) != bull {
            opp += 1;
        }
    }
    opp >= 3
}

// ---------------------------------------------------------------------------
// "Strategy should be run in" / "Trade should be executed in" routing
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum StratCat {
    Index,
    Fno,
    Comm,
}

fn category_for(seg: &str, inst: &str) -> StratCat {
    let seg = seg.to_uppercase();
    let inst = inst.to_uppercase();
    if inst == "INDEX" || inst == "OPTIDX" || seg == "IDX_I" {
        StratCat::Index
    } else if seg.contains("COMM") || seg.contains("NCD") || inst == "FUTCOM" || inst == "OPTFUT" {
        StratCat::Comm
    } else {
        StratCat::Fno
    }
}

fn strat_category(strat: &Strategy) -> StratCat {
    category_for(&strat.exchange_segment, &strat.instrument)
}

/// True when the strategy already points at a resolved option contract
/// (`OPTIDX` / `OPTSTK` / `OPTFUT`, or any `*_FNO` exchange segment).
fn is_option_leg(s: &Strategy) -> bool {
    let inst = s.instrument.to_uppercase();
    inst.starts_with("OPT") || s.exchange_segment.to_uppercase().ends_with("_FNO")
}

/// Chart the strategy conditions are evaluated on: `spot` | `premium` | `both`.
fn run_mode_of(strat: &Strategy, s: &Settings) -> String {
    if s.premium_only {
        return "premium".into();
    }
    match strat_category(strat) {
        StratCat::Index => s.run_index.clone(),
        StratCat::Fno => s.run_fno.clone(),
        StratCat::Comm => s.run_comm.clone(),
    }
}

/// Chart the order executes on: **always the option-premium contract**, for
/// indices, F&O stocks AND commodities alike (operator directive: "trade should
/// always execute on the premium chart"). The resolved premium leg is chosen by
/// the run/eval path; `trade_mode_of` only labels that intent for the UI.
fn trade_mode_of(_strat: &Strategy, _s: &Settings) -> String {
    "premium".into()
}

/// Parse the strike and option type out of a resolved option contract symbol
/// such as `POLICYBZR-Sep2026-1900-PE` (the last two `-` separated fields).
/// Returns `None` for anything that is not a CE/PE option leg.
fn option_strike_type(symbol: &str) -> Option<(f64, &str)> {
    let mut it = symbol.rsplit('-');
    let ot = it.next()?;
    if !(ot.eq_ignore_ascii_case("CE") || ot.eq_ignore_ascii_case("PE")) {
        return None;
    }
    let strike: f64 = it.next()?.trim().parse().ok()?;
    Some((strike, ot))
}

/// Map an option leg (`OPTIDX`/`OPTSTK`/`OPTFUT`, or any `*_FNO` segment) back
/// to its underlying spot instrument. "Run Strategy In: Spot chart" must
/// evaluate the entry conditions on the underlying spot chart, not the option's
/// own premium chart, so the run path calls this before fetching candles.
/// Returns `None` when the strategy is not an option leg or the underlying is
/// not in the app catalog / scrip master.
fn underlying_strategy(strat: &Strategy) -> Option<Strategy> {
    if !is_option_leg(strat) {
        return None;
    }
    let prefix = scrip::fno_underlying(&strat.trading_symbol);
    if prefix.is_empty() {
        return None;
    }
    // Indices + equities from the app catalog (name -> F&O prefix match).
    for (sid, _exch) in crate::market::securities() {
        let Some((name, seg, inst)) = crate::market::symbol_meta(sid) else { continue };
        if scrip::fno_underlying(&name) != prefix {
            continue;
        }
        if inst.eq_ignore_ascii_case("INDEX") {
            let mut out = strat.clone();
            out.security_id = sid;
            out.exchange_segment = "IDX_I".into();
            out.instrument = "INDEX".into();
            out.trading_symbol = name;
            return Some(out);
        }
        if seg.eq_ignore_ascii_case("NSE_EQ") || seg.eq_ignore_ascii_case("BSE_EQ") {
            let mut out = strat.clone();
            out.security_id = sid;
            out.exchange_segment = seg;
            out.instrument = inst;
            out.trading_symbol = name;
            return Some(out);
        }
    }
    // MCX/NCDEX commodities: the spot/underlying chart is the near-month FUTCOM.
    if let Some(sc) = scrip::get() {
        for c in sc.commodity_futures() {
            if scrip::fno_underlying(&c.name) == prefix {
                let mut out = strat.clone();
                out.security_id = c.security_id;
                out.exchange_segment = "MCX_COMM".into();
                out.instrument = "FUTCOM".into();
                out.trading_symbol = c.trading_symbol;
                return Some(out);
            }
        }
    }
    None
}

/// Run mode for a raw `(segment, instrument)` pair (scanner picks carry the
/// underlying, not a full `Strategy`). Mirrors `run_mode_of`.
fn run_mode_for(seg: &str, inst: &str, s: &Settings) -> String {
    if s.premium_only {
        return "premium".into();
    }
    let strat = Strategy {
        exchange_segment: seg.to_string(),
        instrument: inst.to_string(),
        ..Default::default()
    };
    match strat_category(&strat) {
        StratCat::Index => s.run_index.clone(),
        StratCat::Fno => s.run_fno.clone(),
        StratCat::Comm => s.run_comm.clone(),
    }
}

/// Dhan exchange segment for a resolved option contract. MCX commodity options
/// (OPTFUT) live in `MCX_COMM`, exactly like their futures, so the old
/// `NSE_FNO else` fallback would send a broken commodity order.
fn fno_segment(exch: &str) -> &'static str {
    match exch.to_uppercase().as_str() {
        "BSE" => "BSE_FNO",
        "MCX" => "MCX_COMM",
        "NCDEX" => "NCD_FNO",
        _ => "NSE_FNO",
    }
}

/// True when the operator selected BOTH Top-Mover legs: a positive Top Gainers
/// count AND a positive Top Losers count. Selecting both is an explicit call to
/// work the bullish (CE) and bearish (PE) legs together, so the Top-Movers auto
/// bias must not narrow the engine onto a single side.
fn movers_both_legs(settings: &Settings) -> bool {
    settings.movers_on && settings.movers_gainers > 0 && settings.movers_losers > 0
}

/// Auto CE/PE bias for the Top Movers scanner (old AST `autoRunInSide` parity).
/// Classifies the ACTUAL traded universe - the operator's selected top gainers
/// / top losers - rather than whole-market breadth, so switching the selection
/// flips the auto side. Gainers -> +1 (CE), losers -> -1 (PE). When both legs
/// are selected the one with more live rows wins; an exact tie falls back to
/// market-wide breadth (`up`/`down`). NOTE: this bias is only applied when the
/// operator selected a single leg - see [`movers_both_legs`] / `auto_option_side`.
fn movers_auto_bias(want_g: bool, want_l: bool, pos: i64, neg: i64, up: i64, down: i64) -> i64 {
    if want_g && want_l {
        if pos > neg {
            1
        } else if neg > pos {
            -1
        } else if down > up {
            -1
        } else if up > down {
            1
        } else {
            0
        }
    } else if want_g {
        1
    } else if want_l {
        -1
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Order helpers
// ---------------------------------------------------------------------------

fn cfg_f(cfg: &Value, k: &str, def: f64) -> f64 {
    cfg.get(k).and_then(|x| x.as_f64()).unwrap_or(def)
}
fn cfg_opt_f(cfg: &Value, k: &str) -> Option<f64> {
    cfg.get(k).and_then(|x| x.as_f64()).filter(|v| *v > 0.0)
}
fn cfg_str(cfg: &Value, k: &str, def: &str) -> String {
    cfg.get(k).and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_else(|| def.to_string())
}

/// Auto-Lots math: `min(volume% lots, OI% lots, margin lots)`.
///
/// The old app capped liquidity participation at a fixed 1% of
/// `min(OI, volume)`. The new app splits that into two independent operator-set
/// percentages - one for traded volume, one for open interest - and takes the
/// *smaller* of the two lot counts, so whichever cap is more conservative wins.
/// Example: volume 2% -> 20 lots, OI 2% -> 10 lots, so 10 lots are bought.
/// Margin then caps by the funds available. Returns `None` (keep manual lots)
/// when no signal is usable.
fn calc_auto_lots(
    lot: f64,
    price: f64,
    oi: f64,
    volume: f64,
    avail: f64,
    oi_pct: f64,
    volume_pct: f64,
) -> Option<f64> {
    if lot <= 0.0 {
        return None;
    }
    let vol_lots = if volume > 0.0 && volume_pct > 0.0 {
        Some((volume * volume_pct / 100.0 / lot).floor())
    } else {
        None
    };
    let oi_lots = if oi > 0.0 && oi_pct > 0.0 {
        Some((oi * oi_pct / 100.0 / lot).floor())
    } else {
        None
    };
    // Whichever liquidity cap asks for the fewer lots wins.
    let mut lots = match (vol_lots, oi_lots) {
        (Some(v), Some(o)) => Some(v.min(o)),
        (Some(v), None) => Some(v),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    };
    if avail > 0.0 && price > 0.0 {
        let m = (avail * 0.95 / (lot * price)).floor();
        lots = Some(match lots {
            Some(l) => l.min(m),
            None => m,
        });
    }
    lots.filter(|l| *l > 0.0)
}

/// Resolve the entry stop/target/trail set with the old AST precedence:
/// Risk:Reward > Manual Trail TP > Manual TP % > AI TP for targets, and
/// Manual SL / Trail SL floor the stop. Returns
/// `(sl, tp, trail_pct, trail_tp_pct, point_trail_points)`.
fn levels_for(
    settings: &Settings,
    cfg: &Value,
    side: &str,
    ltp: f64,
    ai_sl: f64,
    ai_tp: f64,
    ai_trail: f64,
) -> (f64, f64, f64, f64, f64) {
    let is_buy = side == "BUY";
    // The operator's on/off checkboxes are authoritative: a leftover % value or
    // a stale method-config key must never arm a level the operator turned off.
    // This was the "I disabled Target/SL but it still fires" bug - the engine
    // read `manual_*_pct > 0` (and the config fallback) without checking the
    // matching tick.
    let manual_sl_pct = if settings.manual_sl {
        if settings.manual_sl_pct > 0.0 { settings.manual_sl_pct } else { cfg_f(cfg, "slPct", 0.0) }
    } else {
        0.0
    };
    // Overall SL (fixed % off entry) protects capital; Trail SL ratchets the
    // stop behind the peak but never below this floor.
    let sl = if manual_sl_pct > 0.0 {
        price_off(ltp, side, -manual_sl_pct / 100.0)
    } else if (settings.ai_sl || settings.sl_auto) && !settings.manual_sl && !settings.manual_trail_sl && ai_sl > 0.0 {
        ai_sl
    } else {
        0.0
    };
    // Manual Trail SL is gated by its own tick; when ticked without a % fall
    // back to the method config, then 1%.
    // Keep the trail as a PERCENTAGE (not a fixed price distance): the position
    // manager applies the old app profit-giveback formula
    // `stop = entry + (peak - entry) * (1 - trail/100)` so a trade in profit
    // always retains part of the run. Clamp to 0..100 (>=100 would place the
    // stop at/below entry and give the whole run back).
    let trail = if settings.manual_trail_sl {
        let pct = if settings.manual_trail_sl_pct > 0.0 {
            settings.manual_trail_sl_pct
        } else {
            cfg_f(cfg, "trailPct", 1.0)
        };
        pct.clamp(0.0, 100.0)
    } else if settings.ai_trail_tp && !settings.manual_trail_tp && ai_trail > 0.0 && ltp > 0.0 {
        // AI trail is an ATR distance; express it as a % of entry so the same
        // profit-giveback formula applies.
        (ai_trail / ltp * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    // Points trail (Dhan Super-Order style): a fixed price jump behind the best
    // price, applied broker-side on real orders and simulated in paper.
    let point_trail = if settings.manual_point_trail_sl {
        let pts = if settings.manual_point_trail_sl_points > 0.0 {
            settings.manual_point_trail_sl_points
        } else {
            cfg_f(cfg, "trailingJump", 0.0)
        };
        pts.max(0.0)
    } else {
        0.0
    };
    // Manual Trail TP: exit when running profit gives back this % from its peak.
    let trail_tp_pct = if settings.manual_trail_tp && settings.manual_trail_tp_pct > 0.0 {
        settings.manual_trail_tp_pct
    } else {
        0.0
    };
    // Take-profit precedence: RR > Manual Trail TP > AI TP. The manual
    // "Take Profit %" card was removed, so a saved method config `tpPct` can
    // no longer re-arm a hidden target.
    let mut tp = 0.0;
    if settings.rr_enabled {
        // Risk:Reward overrides manual/AI TP: target = SL distance x Target RR.
        let rr = if settings.rr_value > 0.0 { settings.rr_value } else { 2.0 };
        let risk = (ltp - sl).abs();
        if sl > 0.0 && risk > 0.0 {
            tp = if is_buy { ltp + risk * rr } else { ltp - risk * rr };
        }
    } else if trail_tp_pct <= 0.0
        && (settings.ai_tp_pct || settings.ai_trail_tp)
        && !settings.manual_trail_tp
        && ai_tp > 0.0
    {
        tp = ai_tp;
    }
    (sl.max(0.0), tp.max(0.0), trail, trail_tp_pct, point_trail)
}

/// Post-reconnect exposure the engine must adopt for one security from the
/// broker's signed net minus the engine's signed net:
///   * `0`   the two books agree (nothing to do),
///   * `0` also when the broker net is merely smaller than an engine leg - that
///     is a partial exit, handled by aligning the engine qty, not by adopting an
///     opposite position,
///   * otherwise the extra broker exposure (same direction), or the whole book
///     when the engine holds nothing on the security.
fn reconnect_delta(engine_signed: i64, broker_signed: i64) -> i64 {
    if engine_signed == 0 {
        broker_signed
    } else if (engine_signed > 0) == (broker_signed > 0) && broker_signed.abs() > engine_signed.abs() {
        broker_signed - engine_signed
    } else {
        0
    }
}

fn price_off(price: f64, side: &str, frac: f64) -> f64 {
    let d = price * frac;
    if side == "BUY" {
        price + d
    } else {
        price - d
    }
}

/// Percent-of-running-profit trailing stop.
///
/// `pct` is the share of the RUNNING LIVE PROFIT that may be given back. The
/// stop sits `pct`% of the peak profit behind that peak (converted from the
/// percent to a price distance, i.e. points), and only ratchets forward as the
/// peak grows. It is armed by profit itself, never by the entry price:
///
///   peak profit 1000, pct 10 -> give back 100, stop at 900 profit
///   peak profit 2000, pct 10 -> give back 200, stop at 1800 profit
///
/// A reversal to the stop cuts the trade. The giveback is exactly `pct` of the
/// running profit peak, so the trail arms on ANY profit (even a single rupee)
/// and only gives back its configured share of it - no fixed floor that would
/// otherwise stop out a small winner below entry.
fn profit_trail_stop(is_buy: bool, entry: f64, peak_profit: f64, pct: f64) -> f64 {
    let giveback = peak_profit * pct / 100.0;
    if is_buy {
        entry + peak_profit - giveback
    } else {
        entry - peak_profit + giveback
    }
}

/// Dhan Super-Order style points trail: a FIXED price jump behind the best
/// price. Like the native broker trail it ratchets forward only. Armed by the
/// running profit, so the jump always sits behind the peak, never off entry.
fn point_trail_stop(is_buy: bool, entry: f64, peak_profit: f64, points: f64) -> f64 {
    let jump = points.max(0.05);
    if is_buy {
        entry + peak_profit - jump
    } else {
        entry - peak_profit + jump
    }
}

/// Whether `a` is a tighter stop than `b` for the given side: a higher stop for
/// a BUY, a lower one for a SELL. A missing `b` is always beaten.
fn more_favourable(is_buy: bool, a: f64, b: f64) -> bool {
    if b <= 0.0 {
        true
    } else if is_buy {
        a > b + 1e-9
    } else {
        a < b - 1e-9
    }
}

/// Orders/sec budget: clamped to 1..30 (Dhan rejects bursts above the venue's
/// rate limit). The operator's number is the max orders sent per second.
fn order_per_sec_budget(ops: i64) -> i64 {
    ops.clamp(1, 30)
}

/// "Engine Scan" interval bounds. A 50ms floor keeps the scan from ever spinning
/// the loop, and the 24h ceiling matches the max a user can express in the UI.
const SCAN_INTERVAL_MIN_MS: i64 = 50;
const SCAN_INTERVAL_MAX_MS: i64 = 86_400_000;

/// Normalise the Engine Scan interval: anything below the floor (including a
/// stray 0/negative) becomes 100ms, and anything above 24h is clamped. Applied
/// on state load and on every settings save so the engine always sees a value it
/// can trust.
fn scan_interval_budget(ms: i64) -> i64 {
    if ms < SCAN_INTERVAL_MIN_MS {
        100
    } else {
        ms.min(SCAN_INTERVAL_MAX_MS)
    }
}

/// Normalise the Engine Scan bar-close timeframe to the two the UI offers. Any
/// unrecognised value falls back to `1min` so the engine never has to guess.
fn scan_bar_close_tf(tf: &str) -> &'static str {
    match tf.trim().to_lowercase().as_str() {
        "5min" | "5m" => "5min",
        _ => "1min",
    }
}

/// Milliseconds in one bar of the given Engine Scan bar-close timeframe.
fn scan_bar_close_step_ms(tf: &str) -> i64 {
    match scan_bar_close_tf(tf) {
        "5min" => 5 * 60_000,
        _ => 60_000,
    }
}

/// Index ids must be scanned under the `IDX_I` segment so their `IDX_I:<sid>`
/// quote key matches the feed cache. Injecting them as `NSE_EQ` silently drops
/// every index from the Top Movers / NIFTY-trend universes.
fn index_legs(ids: &[i64]) -> Vec<(i64, String)> {
    ids.iter()
        .filter(|id| **id > 0)
        .map(|id| (*id, "IDX_I".to_string()))
        .collect()
}

/// "Trades per strategy" gate, exactly like the old engine:
///  - Max trades off            -> unlimited.
///  - "AI auto trades" on       -> the fixed cap is ignored (AI decides), so
///                                 entries are unlimited too.
///  - Max trades on + count > 0 -> allow while `done < count`.
/// A non-positive count is treated as "not configured" and never blocks.
fn trade_limit_allows(trade_limit: bool, ai_trades: bool, count: i64, done: i64) -> bool {
    if !trade_limit || ai_trades || count <= 0 {
        return true;
    }
    done < count
}

fn underlying_of(symbol: &str, exch: &str) -> String {
    let u = scrip::fno_underlying(symbol);
    if !u.is_empty() {
        return u;
    }
    if exch.contains("BSE") {
        symbol.split(&[' ', '-'][..]).next().unwrap_or(symbol).to_uppercase()
    } else {
        symbol.to_uppercase()
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn ist_day_key() -> String {
    let secs = crate::market::now_secs() + 19800;
    let days = secs.div_euclid(86400);
    format!("{days}")
}

fn txn(side: &str) -> TransactionType {
    if side.eq_ignore_ascii_case("SELL") {
        TransactionType::Sell
    } else {
        TransactionType::Buy
    }
}

fn seg_from(exch: &str) -> ExchangeSegment {
    match exch.to_uppercase().as_str() {
        "IDX_I" => ExchangeSegment::IdxI,
        "NSE_EQ" => ExchangeSegment::NseEq,
        "NSE_FNO" => ExchangeSegment::NseFno,
        "NSE_CURRENCY" => ExchangeSegment::NseCurrency,
        "BSE_EQ" => ExchangeSegment::BseEq,
        "MCX_COMM" => ExchangeSegment::McxComm,
        "BSE_CURRENCY" => ExchangeSegment::BseCurrency,
        "BSE_FNO" => ExchangeSegment::BseFno,
        _ => ExchangeSegment::NseFno,
    }
}

fn product_from<S: AsRef<str>>(s: S) -> ProductType {
    match s.as_ref().to_uppercase().as_str() {
        "CNC" => ProductType::Cnc,
        "MARGIN" => ProductType::Margin,
        "MTF" => ProductType::Mtf,
        "CO" => ProductType::Co,
        "BO" => ProductType::Bo,
        _ => ProductType::Intraday,
    }
}

fn order_type_from<S: AsRef<str>>(s: S) -> OrderType {
    match s.as_ref().to_uppercase().as_str() {
        "LIMIT" => OrderType::Limit,
        "STOP_LOSS" => OrderType::StopLoss,
        "STOP_LOSS_MARKET" => OrderType::StopLossMarket,
        _ => OrderType::Market,
    }
}

fn validity_from<S: AsRef<str>>(s: S) -> Validity {
    if s.as_ref().eq_ignore_ascii_case("IOC") {
        Validity::Ioc
    } else {
        Validity::Day
    }
}

// ---------------------------------------------------------------------------
// HTTP API
// ---------------------------------------------------------------------------

/// Aggregate closed trades per strategy (Strategy Container stats).
fn container_stats(closed: &[Value], strategies: &[Strategy], charges_on: bool) -> Vec<Value> {
    let cat_of: HashMap<String, (String, String)> = strategies
        .iter()
        .map(|s| (s.id.clone(), (s.category.clone(), s.side.clone())))
        .collect();
    struct Agg {
        name: String,
        trades: i64,
        wins: i64,
        losses: i64,
        total: f64,
        gw: f64,
        gl: f64,
        last_day: i64,
        last_day_pnl: f64,
        days: std::collections::BTreeSet<i64>,
    }
    let mut m: BTreeMap<String, Agg> = BTreeMap::new();
    for c in closed {
        let id = js(c, "strategyId");
        if id.is_empty() {
            continue;
        }
        let pnl = closed_pnl(c, charges_on);
        let day = ji(c, "closedAt") / 86_400_000;
        let a = m.entry(id).or_insert_with(|| Agg {
            name: js(c, "strategyName"),
            trades: 0,
            wins: 0,
            losses: 0,
            total: 0.0,
            gw: 0.0,
            gl: 0.0,
            last_day: 0,
            last_day_pnl: 0.0,
            days: std::collections::BTreeSet::new(),
        });
        a.trades += 1;
        a.total += pnl;
        if pnl > 0.0 {
            a.wins += 1;
            a.gw += pnl;
        } else if pnl < 0.0 {
            a.losses += 1;
            a.gl += pnl;
        }
        a.days.insert(day);
        if day > a.last_day {
            a.last_day = day;
            a.last_day_pnl = pnl;
        }
    }
    let mut out: Vec<Value> = Vec::new();
    for (id, a) in m {
        let win_rate = if a.trades > 0 { a.wins as f64 / a.trades as f64 * 100.0 } else { 0.0 };
        let avg = if a.trades > 0 { a.total / a.trades as f64 } else { 0.0 };
        let pf = if a.gl.abs() > 0.0 {
            a.gw / a.gl.abs()
        } else if a.gw > 0.0 {
            999.0
        } else {
            0.0
        };
        let (category, side) = cat_of
            .get(&id)
            .map(|(c, s)| (c.clone(), s.clone()))
            .unwrap_or_default();
        out.push(json!({
            "strategyId": id,
            "strategyName": a.name,
            "category": category,
            "side": side,
            "trades": a.trades,
            "wins": a.wins,
            "losses": a.losses,
            "winRate": round2(win_rate),
            "totalNet": round2(a.total),
            "avgPerTrade": round2(avg),
            "profitFactor": round2(pf),
            "daysTraded": a.days.len(),
            "lastDay": a.last_day.to_string(),
            "lastDayPnl": round2(a.last_day_pnl),
        }));
    }
    out
}

/// Pick the best-performing strategies (and keep operator-saved rows) for
/// the Final Strategy list. Honors the operator's removed/excluded keys.
fn final_scan(d: &RtDoc, closed: &[Value], charges_on: bool) -> Vec<Value> {
    let mut stats = container_stats(closed, &d.strategies, charges_on);
    let max_avg = stats.iter().map(|s| jf(s, "avgPerTrade").abs()).fold(0.0_f64, f64::max);
    let max_net = stats.iter().map(|s| jf(s, "totalNet").abs()).fold(0.0_f64, f64::max);
    for s in stats.iter_mut() {
        let wr = jf(s, "winRate");
        let avg = jf(s, "avgPerTrade").abs();
        let net = jf(s, "totalNet").abs();
        let score = wr * 0.4
            + if max_avg > 0.0 { avg / max_avg * 100.0 * 0.3 } else { 0.0 }
            + if max_net > 0.0 { net / max_net * 100.0 * 0.3 } else { 0.0 };
        if let Some(o) = s.as_object_mut() {
            o.insert("score".into(), json!(round2(score)));
        }
    }
    stats.sort_by(|a, b| {
        jf(b, "score")
            .partial_cmp(&jf(a, "score"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<Value> = Vec::new();
    for s in stats.into_iter().take(10) {
        let key = js(&s, "strategyId");
        if d.final_excluded.contains_key(&key) {
            continue;
        }
        let mut row = s.clone();
        row["key"] = json!(key);
        row["source"] = json!("real");
        row["manual"] = json!(false);
        out.push(row);
    }
    for f in &d.final_strategies {
        let k = js(f, "key");
        if jb(f, "manual")
            && !out.iter().any(|r| js(r, "key") == k)
            && !d.final_excluded.contains_key(&k)
        {
            out.push(f.clone());
        }
    }
    out
}

/// Enrich raw Dhan positions with LTP and P&L (old `/api/account` parity) so the
/// "Running Trades (Dhan open positions)" panel shows the same buy-avg / LTP /
/// P&L / P&L% columns as the Python app. LTP is taken from the live engine feed
/// by security id; when unavailable we fall back to Dhan's unrealizedProfit.
fn enrich_broker_positions(rows: &[Value], ltp_map: &HashMap<i64, f64>) -> Vec<Value> {
    rows.iter()
        .map(|p| {
            let sid = js(p, "securityId").parse::<i64>().unwrap_or(0);
            let qty = ji(p, "netQty");
            let buy_avg = jf(p, "buyAvg");
            let long = js(p, "positionType").eq_ignore_ascii_case("LONG");
            let mult = if long { 1.0 } else { -1.0 };
            let live_ltp = ltp_map.get(&sid).copied().filter(|v| *v > 0.0);
            let (ltp, pnl) = match live_ltp {
                Some(l) => (l, (l - buy_avg) * qty as f64 * mult),
                None => {
                    let pnl = jf(p, "unrealizedProfit");
                    let denom = qty as f64 * mult;
                    let ltp = if buy_avg > 0.0 && denom.abs() > 0.0 {
                        buy_avg + pnl / denom
                    } else {
                        0.0
                    };
                    (ltp, pnl)
                }
            };
            let base = (buy_avg * qty as f64).abs();
            let pnl_pct = if base > 0.0 { pnl / base * 100.0 } else { 0.0 };
            let mut v = p.clone();
            v["ltp"] = json!(round2(ltp));
            v["pnl"] = json!(round2(pnl));
            v["pnlPct"] = json!(round2(pnl_pct));
            v["long"] = json!(long);
            v
        })
        .collect()
}

fn snap_of(rt: &RealtimeState) -> Value {
    // The per-second snapshot must stay small: shipping the whole closed ledger
    // (thousands of rows, ~1MB) on a 1s poll starves slow/tunneled links and, with
    // the client's out-of-order guard, freezes the whole pane. Send only the
    // newest slice here plus the total count; the UI pulls the full ledger once
    // from `/closed` whenever that count changes.
    const CLOSED_SNAPSHOT_SLICE: usize = 100;

    // Resolve the real broker funds before taking the doc lock (it only touches
    // the funds cache). The paper wallet needs the doc snapshot and is derived
    // below while the lock is already held.
    let real_available = if rt.paper { 0.0 } else { rt.available_funds().unwrap_or(0.0) };
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    let ltp_map = rt.ltp.lock().map(|m| m.clone()).unwrap_or_default();
    let positions: Vec<Value> = d
        .positions
        .iter()
        .map(|p| {
            let sid = ji(p, "securityId");
            let entry = jf(p, "fillPrice").max(jf(p, "entry"));
            let qty = ji(p, "qty");
            let is_buy = js(p, "side") == "BUY";
            let ltp = ltp_map.get(&sid).copied().filter(|v| *v > 0.0).unwrap_or_else(|| jf(p, "ltp"));
            let ltp = if ltp > 0.0 { ltp } else { entry };
            let pnl = if is_buy { (ltp - entry) * qty as f64 } else { (entry - ltp) * qty as f64 };
            let mut v = p.clone();
            v["ltp"] = json!(round2(ltp));
            v["pnl"] = json!(round2(pnl));
            // Live premium change (sign + %) so the running list shows whether the
            // option's premium is currently plus or minus.
            let (chg, chg_pct) = rt.quote_change(sid, &js(p, "exchangeSegment"));
            v["ltpChange"] = json!(chg);
            v["ltpChangePct"] = json!(chg_pct);
            v
        })
        .collect();
    let closed_total = d.closed.len();
    let closed: Vec<Value> = d.closed.iter().rev().take(CLOSED_SNAPSHOT_SLICE).cloned().collect();
    let charges_on = rt.paper && d.settings.broker_charges;
    let smart = smart_stats(&d.closed, &positions, charges_on);
    let funds = rt.funds.lock().map(|f| f.clone()).unwrap_or(json!({}));
    let broker_positions_raw = rt.broker_positions.lock().map(|p| p.clone()).unwrap_or_default();
    let broker_positions = enrich_broker_positions(&broker_positions_raw, &ltp_map);
    let broker_holdings = rt.broker_holdings.lock().map(|h| h.clone()).unwrap_or_default();
    let movers = rt.movers_cache.lock().map(|g| g.1.clone()).unwrap_or(Value::Null);
    let picked_strikes = rt
        .picked_strikes
        .lock()
        .map(|g| g.1.clone())
        .unwrap_or_default();
    let final_list = final_scan(&d, &d.closed, charges_on);
    // NIFTY trend readout for the engine header: the direction the assigned
    // straight-line indicators produced and the bullish/bearish filter split,
    // plus a per-leg active flag so the "Run Strategy In" status can show both
    // legs at once when a bullish and a bearish line are both present.
    let nifty_bull = rt.nifty_bull_filters.lock().map(|g| g.clone()).unwrap_or_default();
    let nifty_bear = rt.nifty_bear_filters.lock().map(|g| g.clone()).unwrap_or_default();
    let nifty_bull_active = !nifty_bull.is_empty();
    let nifty_bear_active = !nifty_bear.is_empty();
    let nifty_trend = json!({
        "on": d.settings.nifty_trend_on,
        "tf": nifty_timeframe(&d.settings),
        "dir": rt.nifty_dir.load(Ordering::Relaxed),
        "bullFilters": nifty_bull,
        "bearFilters": nifty_bear,
        "bullActive": nifty_bull_active,
        "bearActive": nifty_bear_active,
        "bothActive": nifty_bull_active && nifty_bear_active,
    });
    // Surface the timeframe the engine will actually run each strategy on (the
    // 1min/5min checkboxes + Multi-TF confirm override), so the Running
    // Strategies view matches the live evaluation.
    let strat_legs = rt.strat_legs.lock().map(|m| m.clone()).unwrap_or_default();
    let strategies_view: Vec<Value> = d
        .strategies
        .iter()
        .map(|s| {
            let mut v = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
            let tf = match mtf_pair(&d.settings) {
                Some((entry, _)) => entry,
                None => engine_tf(&d.settings, s),
            };
            v["engineTf"] = json!(tf);
            // "Strategy should be run in" / "Trade should be executed in" routing
            // for this strategy, plus the premium contracts the engine last
            // resolved for each, so the Running Strategies view can show (and
            // open) the exact chart the strategy runs and trades on.
            let run_mode = run_mode_of(s, &d.settings);
            let trade_mode = trade_mode_of(s, &d.settings);
            v["runMode"] = json!(run_mode);
            v["tradeMode"] = json!(trade_mode);
            let cached = strat_legs.get(&s.id);
            // Run leg must match the run mode: "spot" publishes the underlying
            // spot chart (an option-instrument strategy maps back to its
            // underlying; everything else uses its own instrument), premium/both
            // reuse the engine-cached premium resolution.
            let mut run_leg = if run_mode == "spot" {
                let own = underlying_strategy(s).unwrap_or_else(|| s.clone());
                Some(json!({
                    "securityId": own.security_id,
                    "tradingSymbol": own.trading_symbol,
                    "segment": own.exchange_segment,
                    "instrument": own.instrument,
                }))
            } else {
                cached.and_then(|l| l.get("run")).cloned()
            };
            let mut trade_leg = cached.and_then(|l| l.get("trade")).cloned();
            // Before the engine's first premium evaluation/entry, resolve the
            // ATM contract synchronously (scrip master only, no REST) so the
            // view still names a concrete premium chart instead of "resolving".
            let wants_premium = |m: &str| m == "premium" || m == "both";
            if (run_leg.is_none() && wants_premium(&run_mode))
                || (trade_leg.is_none() && wants_premium(&trade_mode))
            {
                // ATM strike needs the underlying spot, not an option's own premium.
                let (spot_sid, spot_seg) = match underlying_strategy(s) {
                    Some(u) => (u.security_id, u.exchange_segment),
                    None => (s.security_id, s.exchange_segment.clone()),
                };
                let spot = rt.ltp_of(spot_sid, &spot_seg);
                // Manual Strike Select resolves from the scrip master alone, so it
                // can still name the contract before any live quote arrives.
                if spot > 0.0 || d.settings.manual_strikes_enabled {
                    if let Some(base) = rt.resolve_option_strategy(s, spot, &d.settings, true) {
                        let leg = json!({
                            "securityId": base.security_id,
                            "tradingSymbol": base.trading_symbol,
                            "segment": base.exchange_segment,
                            "instrument": base.instrument,
                        });
                        if run_leg.is_none() && wants_premium(&run_mode) {
                            run_leg = Some(leg.clone());
                        }
                        if trade_leg.is_none() && wants_premium(&trade_mode) {
                            trade_leg = Some(leg);
                        }
                    }
                }
            }
            if let Some(run) = run_leg {
                v["runLeg"] = run;
            }
            if let Some(trade) = trade_leg {
                v["tradeLeg"] = trade;
            }
            v
        })
        .collect();
    // Live AI-trader picks (old AST activeStrategies) so the Running view can
    // mirror the engine's exact run set.
    let ai_picks: Vec<String> = if d.settings.ai_pick { ai_pick_ids(&d) } else { Vec::new() };
    // Live Run-in / Auto side for the UI: the CE/PE the engine is forcing right
    // now and what decided it (NIFTY trend / top gainer-loser / manual). Computed
    // before `d.settings` is moved into the payload so the "Run Strategy In"
    // status shows the live side instead of only the static manual dropdown.
    let auto_side = rt.auto_option_side(&d.settings).map(|s| s.to_string());
    let active_run_in_side = rt.effective_run_in_side(&d.settings).map(|s| s.to_string());
    let margin_available = if rt.paper {
        // Budget basis = wallet BEFORE locked margin; `margin_info_from` then
        // subtracts the running trades' locked margin exactly once to show the
        // remaining available amount.
        paper_wallet_of(d.settings.paper_capital, &d.closed, d.settings.broker_charges)
    } else {
        real_available
    };
    json!({
        "ok": true,
        "settings": d.settings,
        "method": d.method,
        "orderCfg": d.order_cfg,
        "strategies": strategies_view,
        "positions": positions,
        "closed": closed,
        "closedCount": closed_total,
        "margin": rt.margin_info_from(d.settings.margin_amount, d.settings.margin_pct, &d.positions, margin_available),
        "armed": d.armed,
        "engineOn": d.engine_on,
        "autoLots": d.auto_lots,
        "logs": d.logs.iter().rev().take(200).cloned().collect::<Vec<Value>>(),
        "stats": smart,
        "selected": d.selected,
        "aiPicks": ai_picks,
        "staging": d.staging,
        "stagingAuto": d.staging_auto,
        "entryTiming": d.entry_timing,
        "final": final_list,
        "container": container_stats(&d.closed, &d.strategies, charges_on),
        "funds": funds,
        "brokerPositions": broker_positions,
        "holdings": broker_holdings,
        "movers": movers,
        "niftyPicks": rt.nifty_picks.lock().map(|g| g.1.clone()).unwrap_or(Value::Null),
        "niftyTrend": nifty_trend,
        "niftyFlip": rt.nifty_flip.lock().map(|g| g.clone()).unwrap_or(Value::Null),
        "autoSide": auto_side,
        "activeRunInSide": active_run_in_side,
        "strikes": picked_strikes,
    })
}

pub async fn snapshot(State(rt): State<RealtimeState>) -> impl IntoResponse {
    Json(snap_of(&rt))
}

/// Full closed-trade ledger (newest first), fetched on demand by the UI instead
/// of riding along on every 1s snapshot.
pub async fn closed_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let closed: Vec<Value> = rt
        .doc()
        .map(|d| d.closed.iter().rev().cloned().collect())
        .unwrap_or_default();
    let count = closed.len();
    Json(json!({ "ok": true, "closed": closed, "count": count }))
}

pub async fn settings_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let mut just_defaulted: Option<&'static str> = None;
    if let Some(mut d) = rt.doc() {
        let was_run_def = d.settings.run_in_default;
        let was_trade_def = d.settings.trade_in_default;
        let old_exclude = d.settings.scanner_exclude.clone();
        let mut cur = serde_json::to_value(&d.settings).unwrap_or(json!({}));
        if let (Some(map), Some(obj)) = (cur.as_object_mut(), v.as_object()) {
            for (k, val) in obj {
                map.insert(k.clone(), val.clone());
            }
        }
        if let Ok(s) = serde_json::from_value::<Settings>(cur) {
            d.settings = s;
        }
        // Removing/restoring a scanner pick must take effect on the very next
        // tick, not after the 15s movers scan cache expires: reset the cadence
        // clock so the next pass rebuilds the universe.
        if d.settings.scanner_exclude != old_exclude {
            rt.last_movers.store(0, Ordering::Relaxed);
        }
        // Normalise the order-rate guard: orders/sec never means "unset" (fall
        // back to 6) and stays under the Dhan ceiling.
        if d.settings.order_per_sec <= 0 {
            d.settings.order_per_sec = 6;
        }
        d.settings.order_per_sec = order_per_sec_budget(d.settings.order_per_sec);
        // Engine Scan interval: normalise on every save so the engine loop always
        // reads a value inside the floor/ceiling (100ms default for a stray 0).
        d.settings.scan_interval_ms = scan_interval_budget(d.settings.scan_interval_ms);
        d.settings.scan_bar_close_tf = scan_bar_close_tf(&d.settings.scan_bar_close_tf).to_string();
        // Opposite-side veto mode: accept only the two documented values so a
        // stray payload can never silently flip the entry gate ("relative" or
        // "off"; anything else falls back to the "relative" default).
        d.settings.opposite_veto = if d.settings.opposite_veto.eq_ignore_ascii_case("off") {
            "off".to_string()
        } else {
            "relative".to_string()
        };
        // "Make this default setting" markers (old AST runIn/tradeIn `default`
        // flags): the routing is always persisted, so surface the confirmation
        // the old engine logged instead of leaving the checkbox silently inert.
        if !was_run_def && d.settings.run_in_default {
            just_defaulted = Some("run-in");
        } else if !was_trade_def && d.settings.trade_in_default {
            just_defaulted = Some("trade-in");
        }
    }
    rt.save();
    if let Some(what) = just_defaulted {
        rt.log("info", &format!("{what} routing saved as default for all future tasks"));
    }
    Json(json!({ "ok": true }))
}

pub async fn method_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    if let Some(mut d) = rt.doc() {
        let m = js(&v, "method");
        if !m.is_empty() {
            d.method = m;
        }
        if let Some(cfg) = v.get("cfg") {
            let key = js(&v, "method");
            if !key.is_empty() {
                d.order_cfg.insert(key, cfg.clone());
            }
        }
    }
    rt.save();
    Json(json!({ "ok": true }))
}

pub async fn strategies_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    if let Ok(s) = serde_json::from_value::<Strategy>(v.clone()) {
        if let Some(mut d) = rt.doc() {
            if s.id.is_empty() {
                let mut s = s;
                s.id = gen_id("rtstrat");
                d.strategies.push(s);
            } else if let Some(e) = d.strategies.iter_mut().find(|x| x.id == s.id) {
                *e = s;
            } else {
                d.strategies.push(s);
            }
        }
    }
    rt.save();
    Json(snap_of(&rt))
}

pub async fn strategy_delete(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let id = js(&v, "id");
    if let Some(mut d) = rt.doc() {
        d.strategies.retain(|s| s.id != id);
        // Cascade so a deleted strategy cannot linger as a ticked run member,
        // a staged/final container entry or an entry-timing episode.
        d.selected.remove(&id);
        d.staging
            .retain(|s| js(s, "id") != id && js(s, "strategyId") != id);
        d.final_strategies
            .retain(|s| js(s, "id") != id && js(s, "strategyId") != id);
        d.final_excluded.remove(&id);
        d.entry_timing.retain(|e| js(e, "strategyId") != id);
    }
    if let Ok(mut m) = rt.last_sig.lock() {
        m.remove(&id);
    }
    if let Ok(mut m) = rt.et_pending.lock() {
        m.remove(&id);
    }
    rt.save();
    Json(json!({ "ok": true }))
}

pub async fn engine_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let on = jb(&v, "on");
    if let Some(mut d) = rt.doc() {
        d.engine_on = on;
        if !on {
            d.armed = false;
        }
    }
    rt.log("info", if on { "engine started" } else { "engine stopped (disarmed)" });
    rt.save();
    Json(json!({ "ok": true }))
}

pub async fn tick_post(State(rt): State<RealtimeState>) -> impl IntoResponse {
    rt.force_tick.store(true, Ordering::Relaxed);
    Json(json!({ "ok": true }))
}

pub async fn arm_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let on = jb(&v, "armed");
    // Real orders must have a live broker session. Paper trades are simulated
    // locally off the feed quote cache, so they arm even without a REST session.
    let connected = rt.dhan.is_connected().await;
    if on && !connected && !rt.paper {
        return Json(json!({ "ok": false, "error": "cannot arm: Dhan session not connected" }));
    }
    if let Some(mut d) = rt.doc() {
        d.armed = on;
        if on {
            d.engine_on = true;
        }
    }
    rt.log("warn", if on { "ENGINE ARMED" } else { "engine disarmed" });
    rt.save();
    Json(json!({ "ok": true, "armed": on }))
}

pub async fn auto_lots_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    if let Some(mut d) = rt.doc() {
        d.auto_lots = jb(&v, "autoLots");
    }
    rt.save();
    Json(json!({ "ok": true }))
}

pub async fn entry_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let armed = rt.doc().map(|d| d.armed).unwrap_or(false);
    if !armed {
        return Json(json!({ "ok": false, "error": "engine is disarmed" }));
    }
    let strat = serde_json::from_value::<Strategy>(v.clone()).ok();
    let id = js(&v, "strategyId");
    let strat = strat
        .filter(|s| !s.id.is_empty())
        .or_else(|| rt.doc().and_then(|d| d.strategies.iter().find(|s| s.id == id).cloned()));
    let Some(strat) = strat else {
        return Json(json!({ "ok": false, "error": "strategy not found" }));
    };
    match rt.open_entry(&strat, false).await {
        Ok(_) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

pub async fn close_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let id = js(&v, "id");
    let ltp = rt.position_ltp(&id);
    let price = if jf(&v, "price") > 0.0 { jf(&v, "price") } else { ltp };
    match rt.close_position(&id, "MANUAL", price).await {
        Ok(_) => Json(json!({ "ok": true })),
        Err(e) => Json(json!({ "ok": false, "error": e })),
    }
}

pub async fn square_off_post(State(rt): State<RealtimeState>, Json(_v): Json<Value>) -> impl IntoResponse {
    rt.square_off_all("MANUAL_SQUARE_OFF").await;
    Json(json!({ "ok": true }))
}

pub async fn reset_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let what = js(&v, "what");
    if let Some(mut d) = rt.doc() {
        match what.as_str() {
            "closed" => d.closed.clear(),
            "logs" => d.logs.clear(),
            // Smart P&L Reset (old app `AISmartTrading.resetPnl`): clear the whole
            // Smart summary - closed book + logs - so realized P&L, charges, win
            // rate and the trade counts restart from zero. In the paper engine the
            // virtual open positions are discarded too (nothing is banked). Real
            // broker positions are deliberately left untouched so a UI reset can
            // never orphan a live trade.
            "smart" => {
                d.closed.clear();
                d.logs.clear();
                if rt.paper {
                    d.positions.clear();
                }
            }
            // Full reset: clear the closed book, log and every saved strategy
            // plus their selection / container / timing traces. Open positions
            // are deliberately left untouched so real broker trades are never
            // orphaned by a UI reset.
            "all" => {
                d.closed.clear();
                d.logs.clear();
                d.strategies.clear();
                d.selected.clear();
                d.staging.clear();
                d.final_strategies.clear();
                d.final_excluded.clear();
                d.entry_timing.clear();
            }
            _ => {}
        }
    }
    if what == "all" {
        if let Ok(mut m) = rt.last_sig.lock() {
            m.clear();
        }
        if let Ok(mut m) = rt.atr_cache.lock() {
            m.clear();
        }
        if let Ok(mut m) = rt.et_pending.lock() {
            m.clear();
        }
    }
    rt.save();
    Json(json!({ "ok": true }))
}

pub async fn freeze_get(State(_rt): State<RealtimeState>, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> impl IntoResponse {
    let sid = q.get("securityId").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let underlying = q.get("underlying").cloned().unwrap_or_default();
    Json(json!({ "ok": true, "securityId": sid, "underlying": underlying, "freezeQty": scrip::freeze_qty(sid, &underlying) }))
}

/// Indian-style thousands grouping for a money amount (1,23,456 - matching the
/// browser's `toLocaleString("en-IN")` used elsewhere in the UI).
fn inr_group(v: f64) -> String {
    let s = format!("{:.0}", v.abs());
    if s.chars().count() <= 3 {
        return s;
    }
    let tail = &s[s.len() - 3..];
    let mut head = s[..s.len() - 3].to_string();
    let mut groups: Vec<String> = Vec::new();
    while head.len() > 2 {
        let split = head.len() - 2;
        groups.push(head[split..].to_string());
        head.truncate(split);
    }
    groups.push(head);
    groups.reverse();
    format!("{},{}", groups.join(","), tail)
}

fn add_chart_line(lines: &mut Vec<Value>, price: f64, color: &str, title: String, style: i64) {
    if !(price > 0.0) {
        return;
    }
    lines.push(json!({
        "price": round2(price),
        "color": color,
        "title": title,
        "lineWidth": 1.0,
        "lineStyle": style,
    }));
}

/// Pure builder for the trade-chart overlay levels (kept separate from the HTTP
/// handler so it can be unit tested without a live engine).
fn build_chart_lines(
    paper: bool,
    sid: i64,
    exch: &str,
    positions: &[Value],
    ltp_map: &HashMap<i64, f64>,
) -> (Vec<Value>, f64, i64) {
    let prefix = if paper { "PAPER" } else { "RT" };
    let mut lines: Vec<Value> = Vec::new();
    let mut total = 0.0f64;
    let mut count = 0i64;
    if sid <= 0 {
        return (lines, total, count);
    }
    for p in positions.iter() {
        if ji(p, "securityId") != sid {
            continue;
        }
        let seg = js(p, "exchangeSegment");
        if !exch.is_empty() && !seg.eq_ignore_ascii_case(exch) {
            continue;
        }
        let is_buy = !js(p, "side").eq_ignore_ascii_case("SELL");
        let entry = jf(p, "fillPrice").max(jf(p, "entry"));
        if !(entry > 0.0) {
            continue;
        }
        let qty = ji(p, "qty") as f64;
        let ltp = ltp_map
            .get(&sid)
            .copied()
            .filter(|v| *v > 0.0)
            .unwrap_or_else(|| jf(p, "ltp"));
        let mark = if ltp > 0.0 { ltp } else { 0.0 };
        let side = js(p, "side");
        let overall = jf(p, "overallSl");
        let tp = jf(p, "tp");
        let trail = jf(p, "trail");
        let point = jf(p, "pointTrail");
        let trail_tp = jf(p, "trailTp");
        let peak = jf(p, "peakProfit");
        let sl = jf(p, "sl");
        // Each mechanism's own price, computed exactly like manage_positions does,
        // so every level can be drawn as its own labelled box (Dhan style) while
        // the active stop line is titled after whichever mechanism owns it.
        let point_stop = if point > 0.0 && peak > 0.0 {
            Some(point_trail_stop(is_buy, entry, peak, point))
        } else {
            None
        };
        let trail_stop = if trail > 0.0 && peak > 0.0 {
            Some(profit_trail_stop(is_buy, entry, peak, trail))
        } else {
            None
        };
        add_chart_line(&mut lines, entry, "#b388ff", format!("{prefix} ENTRY {side} @ {entry:.2}"), 3);
        let mut money = 0.0;
        if mark > 0.0 {
            money = if is_buy { (mark - entry) * qty } else { (entry - mark) * qty };
            let pct = if entry > 0.0 && qty > 0.0 { money / (entry * qty) * 100.0 } else { 0.0 };
            let sign = if money >= 0.0 { "+" } else { "-" };
            let psign = if pct >= 0.0 { "+" } else { "" };
            add_chart_line(
                &mut lines,
                mark,
                "#00e5ff",
                format!(
                    "{prefix} LIVE P&L {sign}₹{} ({psign}{pct:.2}%) · {mark:.2}",
                    inr_group(money)
                ),
                1,
            );
        }
        // Tolerance for treating two stop levels as the same line: the engine
        // ratchets `sl` in its own tick, so a component can differ by a rounding
        // step from the active stop and must not draw a duplicate box.
        let sl_tol = (sl.abs() * 0.001).max(0.01);
        let mut stop_prices: Vec<f64> = Vec::new();
        let active_title = if point_stop.map(|v| (v - sl).abs() <= sl_tol).unwrap_or(false) {
            "POINT SL"
        } else if trail_stop.map(|v| (v - sl).abs() <= sl_tol).unwrap_or(false) {
            "TRAIL SL"
        } else if trail > 0.0 || point > 0.0 {
            "TRAIL SL"
        } else {
            "SL"
        };
        if sl > 0.0 {
            let mut draw = sl;
            if mark > 0.0 {
                if is_buy && draw > mark {
                    draw = mark;
                }
                if !is_buy && draw < mark {
                    draw = mark;
                }
            }
            add_chart_line(
                &mut lines,
                draw,
                if active_title == "SL" { "#ff5252" } else { "#ff6b6b" },
                format!("{prefix} {active_title} · {sl:.2}"),
                3,
            );
            stop_prices.push(draw);
        }
        let is_dup = |prices: &[f64], v: f64| prices.iter().any(|q| (q - v).abs() <= sl_tol);
        // Point-based (Dhan Super-Order) trail stop, boxed separately whenever it
        // is not already the active stop drawn above.
        if let Some(stop) = point_stop {
            if stop > 0.0 && !is_dup(&stop_prices, stop) {
                add_chart_line(&mut lines, stop, "#ff4dd2", format!("{prefix} POINT SL · {stop:.2}"), 3);
                stop_prices.push(stop);
            }
        }
        // Percent-of-running-profit trail stop, boxed separately when distinct.
        if let Some(stop) = trail_stop {
            if stop > 0.0 && !is_dup(&stop_prices, stop) {
                add_chart_line(&mut lines, stop, "#ff8a80", format!("{prefix} TRAIL SL · {stop:.2}"), 3);
                stop_prices.push(stop);
            }
        }
        // Entry-based overall-SL floor (capital protection). Drawn as its own
        // line only once the live stop has ratcheted away from it, matching the
        // old app's separate "OVERALL SL" line.
        if overall > 0.0 && !is_dup(&stop_prices, overall) {
            add_chart_line(&mut lines, overall, "#ff9100", format!("{prefix} OVERALL SL · {overall:.2}"), 4);
            stop_prices.push(overall);
        }
        if tp > 0.0 {
            add_chart_line(&mut lines, tp, "#00d4aa", format!("{prefix} TARGET · {tp:.2}"), 2);
        }
        // Manual Trail TP: exit level once the running profit gives back its
        // configured share from the peak, boxed on its own.
        if trail_tp > 0.0 && peak > 0.0 {
            let kept = peak * (1.0 - trail_tp / 100.0);
            let stop = if is_buy { entry + kept } else { entry - kept };
            if stop > 0.0 {
                add_chart_line(&mut lines, stop, "#26c6da", format!("{prefix} TRAIL TP · {stop:.2}"), 2);
            }
        }
        total += money;
        count += 1;
    }
    (lines, total, count)
}

/// Live chart-overlay levels for one instrument (old app's `syncTradeChartLines`
/// -> `IndChart.setTradeLines`): entry, running-P&L, stop-loss / trail-SL and
/// target of every OPEN position on the charted symbol. The levels come from the
/// engine's own already-ratcheted ledger, so the overlay moves with the candles
/// and can never drift from the Running Trades list.
pub async fn chart_lines_get(
    State(rt): State<RealtimeState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let sid = q.get("securityId").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let exch = q.get("exchangeSegment").cloned().unwrap_or_default();
    let (lines, total, count) = {
        let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
        let ltp_map = rt.ltp.lock().map(|m| m.clone()).unwrap_or_default();
        build_chart_lines(rt.paper, sid, &exch, &d.positions, &ltp_map)
    };
    Json(json!({
        "ok": true,
        "securityId": sid,
        "exchangeSegment": exch,
        "count": count,
        "pnl": round2(total),
        "lines": lines,
    }))
}


/// Live instrument readout for the Order Placement cards: real LTP/OI/volume
/// from the shared quote cache plus Dhan `/margincalculator` margin required for
/// the currently configured quantity, so Auto-Lots / margin displays are driven
/// by live market data instead of placeholders.
pub async fn instrument_get(
    State(rt): State<RealtimeState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let sec_id = q.get("securityId").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let exch = q.get("exchangeSegment").cloned().unwrap_or_default();
    let side = q.get("side").cloned().unwrap_or_else(|| "BUY".into());
    let product = q.get("productType").cloned().unwrap_or_else(|| "INTRADAY".into());
    let (ltp, oi, volume) = rt.quote_fields(sec_id, &exch);
    let price = q
        .get("price")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|p| *p > 0.0)
        .unwrap_or(ltp);
    let lot_from_scrip = q
        .get("symbol")
        .and_then(|s| scrip::get().and_then(|sc| sc.lot_for(s, &exch).map(|(l, _)| l)))
        .unwrap_or(0.0);
    let lot = q
        .get("lotSize")
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|l| *l > 0.0)
        .unwrap_or(lot_from_scrip);
    let qty = q
        .get("quantity")
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| {
            let lots = q.get("lots").and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            if lots > 0.0 && lot > 0.0 {
                Some((lots * lot).round() as i64)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let margin = if rt.paper { None } else { rt.margin_required(sec_id, &exch, &side, qty, price, &product).await };
    let margin_required = if rt.paper {
        // Paper margin model: full notional (qty x price), no broker round-trip.
        Some(round2(qty as f64 * price)).filter(|v| *v > 0.0)
    } else {
        margin.as_ref().map(|m| m.total_margin).filter(|v| *v > 0.0)
    };
    let available = margin
        .as_ref()
        .map(|m| m.available_balance)
        .filter(|v| *v > 0.0)
        .or_else(|| rt.available_funds());
    Json(json!({
        "ok": true,
        "securityId": sec_id,
        "exchangeSegment": exch,
        "ltp": ltp,
        "oi": oi,
        "volume": volume,
        "lotSize": lot,
        "price": price,
        "quantity": qty,
        "marginRequired": margin_required,
        "availableBalance": available,
    }))
}

pub async fn account_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    rt.refresh().await;
    let funds = rt.funds.lock().map(|f| f.clone()).unwrap_or(json!({}));
    let ltp_map = rt.ltp.lock().map(|m| m.clone()).unwrap_or_default();
    let positions = {
        let raw = rt.broker_positions.lock().map(|p| p.clone()).unwrap_or_default();
        enrich_broker_positions(&raw, &ltp_map)
    };
    let holdings = rt.broker_holdings.lock().map(|h| h.clone()).unwrap_or_default();
    Json(json!({ "ok": true, "funds": funds, "positions": positions, "holdings": holdings }))
}

/// Standalone Account tab view (`/api/account`), ported 1:1 from the old Flask
/// app's `/api/account` payload so the dedicated Account tab behaves exactly
/// like the Python one: four balance cards, the Open Positions table and the
/// Holdings table. Everything (funds / positions / holdings + LTP enrichment)
/// is derived in pure Rust from the live Dhan session; no Python, no pandas.
pub async fn account_overview(State(rt): State<RealtimeState>) -> (StatusCode, Json<Value>) {
    if !rt.dhan.is_connected().await {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "status": "error", "message": "Not connected to Dhan" })),
        );
    }
    rt.refresh().await;

    let funds = rt.funds.lock().map(|f| f.clone()).unwrap_or(json!({}));
    let ltp_map = rt.ltp.lock().map(|m| m.clone()).unwrap_or_default();
    let raw_positions = rt.broker_positions.lock().map(|p| p.clone()).unwrap_or_default();
    let raw_holdings = rt.broker_holdings.lock().map(|h| h.clone()).unwrap_or_default();

    (
        StatusCode::OK,
        Json(account_payload(&funds, &raw_positions, &raw_holdings, &ltp_map)),
    )
}

/// Build the old-app Account payload from the raw broker caches. Pure and
/// side-effect free so the shape stays unit-testable without a live session.
fn account_payload(
    funds: &Value,
    raw_positions: &[Value],
    raw_holdings: &[Value],
    ltp_map: &HashMap<i64, f64>,
) -> Value {
    // Broker positions, marked to the live LTP cache (same shape the running
    // engine already publishes). Day P&L is the sum of every open position.
    let enriched = enrich_broker_positions(raw_positions, ltp_map);
    let mut total_pnl = 0.0f64;
    let positions: Vec<Value> = enriched
        .iter()
        .map(|p| {
            let pnl = jf(p, "pnl");
            total_pnl += pnl;
            json!({
                "symbol": js(p, "tradingSymbol"),
                "security_id": js(p, "securityId"),
                "exchange": js(p, "exchangeSegment"),
                "qty": ji(p, "netQty"),
                "buy_avg": round2(jf(p, "buyAvg")),
                "ltp": round2(jf(p, "ltp")),
                "pnl": round2(pnl),
                "pnl_pct": round2(jf(p, "pnlPct")),
                "type": js(p, "positionType"),
                "product": js(p, "productType"),
            })
        })
        .collect();

    // Delivery holdings. LTP preference: broker-reported last price, then the
    // shared live LTP cache, then avg cost (so a closed market shows 0 P&L
    // instead of a fake loss against a 0 price).
    let holdings: Vec<Value> = raw_holdings
        .iter()
        .map(|h| {
            let sid = js(h, "securityId").parse::<i64>().unwrap_or(0);
            let qty = ji(h, "totalQty");
            let buy_avg = jf(h, "avgCostPrice");
            let reported = jf(h, "lastPrice");
            let ltp = if reported > 0.0 {
                reported
            } else {
                ltp_map.get(&sid).copied().filter(|v| *v > 0.0).unwrap_or(buy_avg)
            };
            let pnl = (ltp - buy_avg) * qty as f64;
            let base = (buy_avg * qty as f64).abs();
            let pnl_pct = if base > 0.0 { pnl / base * 100.0 } else { 0.0 };
            json!({
                "symbol": js(h, "tradingSymbol"),
                "exchange": js(h, "exchange"),
                "qty": qty,
                "buy_avg": round2(buy_avg),
                "ltp": round2(ltp),
                "pnl": round2(pnl),
                "pnl_pct": round2(pnl_pct),
                "isin": js(h, "isin"),
            })
        })
        .collect();

    let available = funds
        .get("availabelBalance")
        .and_then(|v| v.as_f64())
        .or_else(|| funds.get("availableBalance").and_then(|v| v.as_f64()))
        .unwrap_or(0.0);
    let used_margin = jf(funds, "utilizedAmount");
    let collateral = jf(funds, "collateralAmount");
    let opening = jf(funds, "sodLimit").max(jf(funds, "openingBalance"));

    json!({
        "status": "success",
        "data": {
            "balance": {
                "total": round2(available + used_margin),
                "available": round2(available),
                "used_margin": round2(used_margin),
                "collateral": round2(collateral),
                "opening_balance": round2(opening),
                "payin": round2(jf(funds, "payinAmount")),
                "payout": round2(jf(funds, "payoutAmount")),
            },
            "pnl": round2(total_pnl),
            "positions": positions,
            "holdings": holdings,
        }
    })
}

/// Live Data Pool readout for the AI Smart Trading engine.
pub async fn pool_get(
    State(rt): State<RealtimeState>,
    axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let force = q
        .get("refresh")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    Json(rt.pool_readout(force).await)
}

pub async fn strategies_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "strategies": d.strategies }))
}

pub async fn logs_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "logs": d.logs }))
}

pub async fn movers_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    // On-demand scan so the readout fills even before the background loop ticks
    // (and regardless of the engine run/arm state).
    rt.refresh_movers().await;
    Json(rt.movers_readout().await)
}

pub async fn commodities_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let (on, selected) = rt
        .doc()
        .map(|d| (d.settings.commodity_on, d.settings.commodity_list.clone()))
        .unwrap_or((false, Vec::new()));
    let mut payload = crate::market::commodities_json();
    if let Some(o) = payload.as_object_mut() {
        o.insert("ok".into(), json!(true));
        o.insert("on".into(), json!(on));
        o.insert("selected".into(), json!(selected));
    }
    Json(payload)
}

pub async fn templates_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let list = rt.doc().map(|d| d.templates.clone()).unwrap_or_default();
    Json(json!({ "ok": true, "templates": list }))
}

/// Backup tab: full durable engine state (settings, strategies, templates,
/// positions, closed ledger / trade statistics) as JSON.
pub async fn state_export_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    Json(json!({ "ok": true, "state": rt.export_state() }))
}

/// Backup tab: replace the whole engine state from a snapshot, persist it, and
/// report how many strategies/positions/closed trades were restored.
pub async fn state_import_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let state = v.get("state").cloned().unwrap_or(Value::Null);
    match rt.import_state(&state) {
        Ok(n) => Json(json!({ "ok": true, "restored": n })),
        Err(e) => Json(json!({ "ok": false, "message": e })),
    }
}

pub async fn template_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let action = js(&v, "action");
    let name = js(&v, "name");
    let mut out = json!({ "ok": false, "error": "unknown action" });
    let mut after_log: Option<(String, String)> = None;
    if let Some(mut d) = rt.doc() {
        match action.as_str() {
            "save" => {
                if name.is_empty() {
                    out = json!({ "ok": false, "error": "name required" });
                } else {
                    let side = {
                        let s = js(&v, "side");
                        if s.is_empty() { "bullish".to_string() } else { s }
                    };
                    let settings = serde_json::to_value(&d.settings).unwrap_or(json!({}));
                    // Capture the ticked strategy set too, so "Run saved template"
                    // restores the exact run definition (old-app parity).
                    let selected: Vec<String> = d
                        .selected
                        .iter()
                        .filter(|(_, on)| **on)
                        .map(|(id, _)| id.clone())
                        .collect();
                    d.templates.insert(
                        name.clone(),
                        json!({
                            "name": name,
                            "side": side,
                            "savedAt": now_ms(),
                            "settings": settings,
                            "selected": selected
                        }),
                    );
                    out = json!({ "ok": true, "name": name });
                }
            }
            "delete" => {
                d.templates.remove(&name);
                out = json!({ "ok": true });
            }
            "open" => {
                if let Some(t) = d.templates.get(&name).cloned() {
                    if let Some(sv) = t.get("settings") {
                        if let Ok(s) = serde_json::from_value::<Settings>(sv.clone()) {
                            d.settings = s;
                            out = json!({ "ok": true });
                        }
                    }
                } else {
                    out = json!({ "ok": false, "error": "template not found" });
                }
            }
            // "AST saved templates quick run": apply the template's saved settings,
            // restore its ticked strategy set, then start the engine in that
            // template's own run mode (strategies / Indicator-filters / AI auto-pick).
            "run" => {
                if let Some(t) = d.templates.get(&name).cloned() {
                    if let Some(sv) = t.get("settings") {
                        if let Ok(s) = serde_json::from_value::<Settings>(sv.clone()) {
                            d.settings = s;
                        }
                    }
                    let mut restored = 0usize;
                    if let Some(arr) = t.get("selected").and_then(|x| x.as_array()) {
                        let ids: Vec<String> = arr
                            .iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect();
                        if !ids.is_empty() {
                            d.selected.clear();
                            for id in ids {
                                if d.strategies.iter().any(|s| s.id == id) {
                                    d.selected.insert(id, true);
                                    restored += 1;
                                }
                            }
                        }
                    }
                    d.engine_on = true;
                    after_log = Some((
                        "info".into(),
                        format!(
                            "Quick run template \"{name}\" started (arm required) - {restored} strategy(s) restored from the template"
                        ),
                    ));
                    out = json!({
                        "ok": true,
                        "name": name,
                        "side": js(&t, "side"),
                        "restored": restored
                    });
                } else {
                    out = json!({ "ok": false, "error": "template not found" });
                }
            }
            _ => {}
        }
    }
    if let Some((level, msg)) = after_log {
        rt.log(&level, &msg);
    }
    rt.save();
    Json(out)
}

pub async fn trend_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    // NIFTY trend-following readout. Re-run the (throttled) trend + scan passes so
    // the header is fresh even between engine ticks, then return the pick detail.
    let on = rt.doc().map(|d| d.settings.nifty_trend_on).unwrap_or(false);
    rt.refresh_nifty_trend().await;
    rt.refresh_nifty_scan().await;
    let detail = rt.nifty_readout().await;
    let dir = rt.nifty_dir.load(Ordering::Relaxed);
    // Surface each pick by its underlying (the id the UI removes / the scanner
    // excludes), not the resolved option leg.
    let picks: Vec<Value> = jarr(&detail, "picks")
        .iter()
        .map(|p| {
            json!({
                "securityId": ji(p, "underlyingSecurityId"),
                "underlying": js(p, "underlying"),
                "side": js(p, "side"),
                "tradingSymbol": js(p, "tradingSymbol"),
                "changePct": jf(p, "changePct"),
            })
        })
        .collect();
    Json(json!({ "ok": true, "on": on, "dir": dir, "picks": picks, "detail": detail }))
}

// ---------------------------------------------------------------------------
// Strategy selection / staging / diagnostics / final / container
// ---------------------------------------------------------------------------

/// Top-N scoring strategies per side, used by the "AI picks" selector. Score is
/// the same composite used by the Final list (win rate + avg + net).
fn ai_pick_ids(d: &RtDoc) -> Vec<String> {
    let stats = container_stats(&d.closed, &d.strategies, d.settings.broker_charges);
    let mut by_id: HashMap<String, f64> = HashMap::new();
    for s in &stats {
        by_id.insert(js(s, "strategyId"), jf(s, "score"));
    }
    let n = d.settings.ai_pick_n.max(1) as usize;
    let mut bull: Vec<(f64, String)> = Vec::new();
    let mut bear: Vec<(f64, String)> = Vec::new();
    for s in &d.strategies {
        let score = by_id.get(&s.id).copied().unwrap_or(0.0);
        if strategy_is_bull(s) {
            bull.push((score, s.id.clone()));
        } else {
            bear.push((score, s.id.clone()));
        }
    }
    bull.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    bear.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<String> = Vec::new();
    if d.settings.ai_pick_bull {
        out.extend(bull.into_iter().take(n).map(|(_, id)| id));
    }
    if d.settings.ai_pick_bear {
        out.extend(bear.into_iter().take(n).map(|(_, id)| id));
    }
    out
}

pub async fn selection_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "selected": d.selected, "aiPicks": ai_pick_ids(&d) }))
}

pub async fn selection_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let action = js(&v, "action");
    let mut out = json!({ "ok": true });
    if let Some(mut d) = rt.doc() {
        match action.as_str() {
            "sync" => {
                d.selected.clear();
                for id in jarr(&v, "ids") {
                    if let Some(s) = id.as_str() {
                        d.selected.insert(s.to_string(), true);
                    }
                }
            }
            "selectAll" => {
                let ids: Vec<String> = d.strategies.iter().map(|s| s.id.clone()).collect();
                for id in ids {
                    d.selected.insert(id, true);
                }
            }
            "none" => d.selected.clear(),
            "toggle" => {
                let id = js(&v, "id");
                let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(true);
                d.selected.insert(id, on);
            }
            "addSaved" => {
                // Add a saved strategy (by id or unique name) into the run set.
                let id = js(&v, "id");
                let name = js(&v, "name");
                let found = d
                    .strategies
                    .iter()
                    .find(|s| (!id.is_empty() && s.id == id) || (!name.is_empty() && s.name == name))
                    .map(|s| s.id.clone());
                if let Some(sid) = found {
                    d.selected.insert(sid, true);
                } else {
                    out = json!({ "ok": false, "error": "strategy not found" });
                }
            }
            "remove" => {
                let id = js(&v, "id");
                d.selected.remove(&id);
            }
            "aiPick" => {
                if let Some(n) = v.get("n").and_then(|x| x.as_i64()) {
                    d.settings.ai_pick_n = n;
                }
                if let Some(b) = v.get("bull").and_then(|x| x.as_bool()) {
                    d.settings.ai_pick_bull = b;
                }
                if let Some(b) = v.get("bear").and_then(|x| x.as_bool()) {
                    d.settings.ai_pick_bear = b;
                }
                d.settings.ai_pick = true;
                let picks = ai_pick_ids(&d);
                for id in picks {
                    d.selected.insert(id, true);
                }
            }
            "runIn" => {
                if let Some(b) = v.get("enabled").and_then(|x| x.as_bool()) {
                    d.settings.run_in_enabled = b;
                }
                let side = js(&v, "side");
                if !side.is_empty() {
                    d.settings.run_in_side = side;
                }
                if let Some(b) = v.get("auto").and_then(|x| x.as_bool()) {
                    d.settings.run_in_auto = b;
                }
            }
            _ => out = json!({ "ok": false, "error": "unknown action" }),
        }
    }
    rt.save();
    Json(out)
}

pub async fn staging_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "staging": d.staging, "autoSend": d.staging_auto }))
}

pub async fn staging_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let action = js(&v, "action");
    let mut out = json!({ "ok": true });
    if let Some(mut d) = rt.doc() {
        match action.as_str() {
            "add" => {
                let id = js(&v, "id");
                let name = js(&v, "name");
                if let Some(s) = d
                    .strategies
                    .iter()
                    .find(|s| (!id.is_empty() && s.id == id) || (!name.is_empty() && s.name == name))
                    .cloned()
                {
                    let key = s.id.clone();
                    if !d.staging.iter().any(|x| js(x, "key") == key) {
                        d.staging.push(json!({
                            "key": key,
                            "name": s.name,
                            "cat": if strategy_is_bull(&s) { "bullish" } else { "bearish" },
                            "tf": s.timeframe,
                            "side": s.side,
                            "addedAt": now_ms(),
                            "sentAt": Value::Null,
                        }));
                    }
                    if d.staging_auto {
                        d.selected.insert(s.id.clone(), true);
                    }
                } else {
                    out = json!({ "ok": false, "error": "strategy not found" });
                }
            }
            "remove" => {
                let key = js(&v, "key");
                d.staging.retain(|x| js(x, "key") != key);
            }
            "removeAll" => d.staging.clear(),
            "send" => {
                let keys: Vec<String> = jarr(&v, "keys")
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
                let mut targets: Vec<(String, String)> = Vec::new();
                for x in d.staging.iter_mut() {
                    let key = js(x, "key");
                    let id = key.strip_prefix("pt:").unwrap_or(&key).to_string();
                    if keys.is_empty() || keys.contains(&key) {
                        if let Some(o) = x.as_object_mut() {
                            o.insert("sentAt".into(), json!(now_ms()));
                        }
                        targets.push((id, js(x, "name")));
                    }
                }
                for (id, name) in targets {
                    if d.strategies.iter().any(|s| s.id == id || (!name.is_empty() && s.name == name)) {
                        d.selected.insert(id, true);
                    }
                }
            }
            "autoSend" => {
                let on = v.get("on").and_then(|x| x.as_bool()).unwrap_or(false);
                d.staging_auto = on;
                if on {
                    for s in d.strategies.clone() {
                        if !d.staging.iter().any(|x| js(x, "key") == s.id) {
                            d.staging.push(json!({
                                "key": s.id, "name": s.name,
                                "cat": if strategy_is_bull(&s) { "bullish" } else { "bearish" },
                                "tf": s.timeframe, "side": s.side,
                                "addedAt": now_ms(), "sentAt": Value::Null,
                            }));
                        }
                        d.selected.insert(s.id.clone(), true);
                    }
                }
            }
            _ => out = json!({ "ok": false, "error": "unknown action" }),
        }
    }
    rt.save();
    Json(out)
}

pub async fn entry_timing_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    Json(json!({ "ok": true, "rows": d.entry_timing }))
}

pub async fn entry_timing_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    if js(&v, "action") == "clear" {
        if let Some(mut d) = rt.doc() {
            d.entry_timing.clear();
        }
        rt.save();
    }
    Json(json!({ "ok": true }))
}

pub async fn final_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    let charges_on = d.settings.broker_charges;
    Json(json!({ "ok": true, "final": final_scan(&d, &d.closed, charges_on) }))
}

pub async fn final_post(State(rt): State<RealtimeState>, Json(v): Json<Value>) -> impl IntoResponse {
    let action = js(&v, "action");
    let mut out = json!({ "ok": true });
    if let Some(mut d) = rt.doc() {
        match action.as_str() {
            "save" => {
                let key = js(&v, "key");
                let name = js(&v, "name");
                if !key.is_empty() {
                    d.final_excluded.remove(&key);
                    if !d.final_strategies.iter().any(|x| js(x, "key") == key) {
                        d.final_strategies.push(json!({
                            "key": key, "strategyId": key, "strategyName": name,
                            "source": "manual", "manual": true, "savedAt": now_ms(),
                        }));
                    }
                }
            }
            "remove" => {
                for k in jarr(&v, "keys").iter().filter_map(|x| x.as_str()) {
                    d.final_strategies.retain(|x| js(x, "key") != k);
                    d.final_excluded.insert(k.to_string(), true);
                }
            }
            "removeAll" => {
                for x in d.final_strategies.clone() {
                    d.final_excluded.insert(js(&x, "key"), true);
                }
                d.final_strategies.clear();
            }
            "run" => {
                let key = js(&v, "key");
                if d.strategies.iter().any(|s| s.id == key) {
                    d.selected.insert(key, true);
                }
            }
            _ => out = json!({ "ok": false, "error": "unknown action" }),
        }
    }
    rt.save();
    Json(out)
}

pub async fn container_get(State(rt): State<RealtimeState>) -> impl IntoResponse {
    let d = rt.doc.lock().unwrap_or_else(|e| e.into_inner());
    let stats = container_stats(&d.closed, &d.strategies, d.settings.broker_charges);
    let bull = d.strategies.iter().filter(|s| strategy_is_bull(s)).count();
    let bear = d.strategies.len() - bull;
    let traded = stats.len();
    let trades: i64 = stats.iter().map(|s| ji(s, "trades")).sum();
    let last_day = stats
        .iter()
        .map(|s| ji(s, "lastDay"))
        .max()
        .map(|v| v.to_string())
        .unwrap_or_default();
    Json(json!({
        "ok": true,
        "container": stats,
        "summary": {
            "strategies": d.strategies.len(),
            "bullish": bull,
            "bearish": bear,
            "fromAe": d.staging.len(),
            "deployed": d.selected.len(),
            "withTrades": traded,
            "trades": trades,
            "lastTradedDay": last_day,
        },
        "templates": d.templates.keys().cloned().collect::<Vec<String>>(),
    }))
}

/// Realtime router, generic over the outer app state.
/// Registers the full AI Smart engine API under `$p` (e.g. `/api/rt` or
/// `/api/paper`) on a fresh `Router`. The caller's return type chooses the state
/// type, so the same table drives the real and paper engines.
macro_rules! rt_routes {
    ($p:literal) => {
        Router::new()
            .route(concat!($p, "/snapshot"), get(snapshot))
            .route(concat!($p, "/closed"), get(closed_get))
            .route(concat!($p, "/settings"), post(settings_post))
            .route(concat!($p, "/method"), post(method_post))
            .route(concat!($p, "/strategies"), get(strategies_get).post(strategies_post))
            .route(concat!($p, "/strategy/delete"), post(strategy_delete))
            .route(concat!($p, "/engine"), post(engine_post))
            .route(concat!($p, "/tick"), post(tick_post))
            .route(concat!($p, "/arm"), post(arm_post))
            .route(concat!($p, "/autolots"), post(auto_lots_post))
            .route(concat!($p, "/entry"), post(entry_post))
            .route(concat!($p, "/close"), post(close_post))
            .route(concat!($p, "/square_off"), post(square_off_post))
            .route(concat!($p, "/reset"), post(reset_post))
            .route(concat!($p, "/freeze_qty"), get(freeze_get))
            .route(concat!($p, "/instrument"), get(instrument_get))
            .route(concat!($p, "/chart-lines"), get(chart_lines_get))
            .route(concat!($p, "/account"), get(account_get))
            .route(concat!($p, "/pool"), get(pool_get))
            .route(concat!($p, "/movers"), get(movers_get))
            .route(concat!($p, "/trend"), get(trend_get))
            .route(concat!($p, "/commodities"), get(commodities_get))
            .route(concat!($p, "/templates"), get(templates_get).post(template_post))
            .route(concat!($p, "/selection"), get(selection_get).post(selection_post))
            .route(concat!($p, "/staging"), get(staging_get).post(staging_post))
            .route(concat!($p, "/entry-timing"), get(entry_timing_get).post(entry_timing_post))
            .route(concat!($p, "/final"), get(final_get).post(final_post))
            .route(concat!($p, "/container"), get(container_get))
            .route(concat!($p, "/logs"), get(logs_get))
            .route(concat!($p, "/stats"), get(crate::stats::stats_get))
            .route(concat!($p, "/state/export"), get(state_export_get))
            .route(concat!($p, "/state/import"), post(state_import_post))
    };
}

/// Real-trading API (`/api/rt/*`), served from the app-wide `RealtimeState`.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
    RealtimeState: FromRef<S>,
{
    rt_routes!("/api/rt")
}

/// Paper-trading API (`/api/paper/*`), routed to the dedicated paper engine.
pub fn paper_router() -> Router<RealtimeState> {
    rt_routes!("/api/paper")
}

/// Second independent paper-trading API (`/api/paper2/*`), routed to its own
/// engine with a separate durable state file - shares no data with `/api/paper`.
pub fn paper2_router() -> Router<RealtimeState> {
    rt_routes!("/api/paper2")
}

// Keep an unused import from warning in some build profiles.
#[allow(dead_code)]
fn _touch(_: LegName, _: ModifyOrderRequest, _: MarketState) {}

#[cfg(test)]
mod gate_tests {
    use super::*;

    /// Serialises the tests that mutate the process-wide indicator cache and
    /// derivation counter. Without this, the tests run in parallel and one
    /// test's `reset_global_ind_cache()` can wipe another's counted work,
    /// making the count assertions flaky.
    fn ind_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn ramp(n: usize, start: f64, step: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let c = start + step * i as f64;
                Candle { time: i as i64 * 300_000, open: c - step, high: c + step.abs(), low: c - step.abs(), close: c, volume: 1000.0 + i as f64 }
            })
            .collect()
    }

    /// Oscillating series with swing pivots, so the structural indicators
    /// (auto trendline, pitchfork, auto S/R, ...) have something to resolve.
    fn wave(n: usize, start: f64, drift: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let c = start + drift * i as f64 + ((i as f64) * 0.35).sin() * 3.0;
                Candle {
                    time: i as i64 * 300_000,
                    open: c - 0.4,
                    high: c + 1.2,
                    low: c - 1.2,
                    close: c,
                    volume: 1000.0 + ((i * 37) % 500) as f64,
                }
            })
            .collect()
    }

    fn bull_strategy() -> Strategy {
        Strategy { id: "t".into(), name: "T".into(), category: "BULLISH".into(), side: "BUY".into(), ..Default::default() }
    }
    fn bear_strategy() -> Strategy {
        Strategy { id: "t".into(), name: "T".into(), category: "BEARISH".into(), side: "SELL".into(), ..Default::default() }
    }

    #[test]
    fn independent_engines_use_distinct_state_files() {
        // The real engine, the first paper engine and the second paper engine must
        // each persist to their own file, so no engine can ever read another's
        // strategies/settings/ledger.
        assert_ne!(state_path(STATE_FILE_REAL), state_path(STATE_FILE_PAPER));
        assert_ne!(state_path(STATE_FILE_PAPER), state_path(STATE_FILE_PAPER2));
        assert_ne!(state_path(STATE_FILE_REAL), state_path(STATE_FILE_PAPER2));
    }

    #[tokio::test]
    async fn blocked_delayed_entry_releases_the_fresh_edge() {
        // In "fresh signal" mode the entry edge is consumed the moment the order
        // is queued for the paper latency simulation. If the fill never lands
        // (margin block / rejection / engine disarmed) the edge must be released,
        // otherwise the strategy latches silent and stops trading until the gate
        // happens to reset and print a brand-new signal.
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        rt.set_sig_state("scan:1:PE", true);
        assert!(rt.sig_state_is_set("scan:1:PE"), "edge is consumed at queue time");
        rt.release_fresh_edge("scan:1:PE");
        assert!(!rt.sig_state_is_set("scan:1:PE"), "an unfilled entry must release the edge");
    }

    #[tokio::test]
    async fn duplicate_entry_on_the_same_contract_is_blocked() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let s = Settings::default();
        assert!(!rt.contract_running(12345), "no position yet");
        assert!(!rt.duplicate_entry_blocked(&s, 12345, true), "nothing to dedupe against");
        if let Some(mut d) = rt.doc() {
            d.positions.push(json!({ "id": "p1", "securityId": 12345, "strategyId": "scan:1:CE" }));
        }
        assert!(rt.contract_running(12345), "running position is seen");
        // A second entry on the same contract is blocked: fresh-signal
        // re-trigger, a second strategy resolving to the strike, or a scan after
        // a restart with a cleared in-memory signal edge.
        assert!(rt.duplicate_entry_blocked(&s, 12345, true), "running contract blocks a duplicate");
        // A different strike is unaffected.
        assert!(!rt.duplicate_entry_blocked(&s, 99999, true), "another strike is fine");
        // Manual Order Placement bypasses the guard (operator-driven).
        assert!(!rt.duplicate_entry_blocked(&s, 12345, false), "manual placement is not deduped");
        // The explicit "repeat while condition holds" stacking mode is exempt.
        let mut stacking = s.clone();
        stacking.multi_always_on = true;
        assert!(!rt.duplicate_entry_blocked(&stacking, 12345, true), "always-on stacking is exempt");
        // An unresolved security id (0) is never treated as a contract.
        assert!(!rt.duplicate_entry_blocked(&s, 0, true), "unresolved id is not deduped");
    }

    #[tokio::test]
    async fn once_per_signal_blocks_repeat_strikes_until_a_fresh_signal() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let id = "scan:1:CE";
        assert!(!rt.pulse_blocked(id, 111), "nothing traded yet");
        // Trading a strike remembers it for the current signal pulse.
        rt.pulse_mark(id, 111);
        assert!(rt.pulse_blocked(id, 111), "same strike is blocked within the pulse");
        assert!(!rt.pulse_blocked(id, 222), "a different strike is still allowed");
        assert!(!rt.pulse_blocked("scan:2:CE", 111), "another leg has its own memory");
        // A gate that stays true keeps the strike blocked until the pulse ends.
        rt.set_sig_state(id, true);
        assert!(!rt.sig_state_is_set("scan:2:CE"), "only this leg's edge was set");
        // The gate stopping ends the pulse: next meet is a brand-new fresh signal.
        rt.clear_signal_edge(id);
        assert!(!rt.sig_state_is_set(id), "gate reset clears the fresh-edge flag");
        assert!(!rt.pulse_blocked(id, 111), "a fresh signal may trade the strike again");
    }

    #[tokio::test]
    async fn once_per_signal_is_signal_scoped_not_position_scoped() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let mut s = Settings::default();
        s.once_per_signal = true;
        s.multi_always_on = true; // stacking mode must not change the signal rule
        let strat = Strategy {
            id: "scan:1:PE".into(),
            security_id: 1,
            category: "BEARISH".into(),
            synthetic: true,
            ..Default::default()
        };
        let contract = 116798;
        // Nothing traded this signal yet: allowed even while a position runs.
        if let Some(mut d) = rt.doc() {
            d.positions.push(json!({ "id": "p1", "securityId": contract, "strategyId": strat.id }));
        }
        assert_eq!(
            rt.once_per_signal_block_reason(&s, &strat, contract),
            None,
            "a running position alone never blocks - only a repeat inside the same signal does"
        );
        // The same contract already traded this signal: blocked.
        rt.pulse_mark(&strat.id, contract);
        assert_eq!(
            rt.once_per_signal_block_reason(&s, &strat, contract),
            Some("already traded this strike on the current signal")
        );
        // A new signal (pulse reset) may trade the same strike again, position or not.
        rt.clear_signal_edge(&strat.id);
        assert_eq!(rt.once_per_signal_block_reason(&s, &strat, contract), None);
        // Non-synthetic (normal-mode) strategies are never held by this guard.
        let mut norm = strat.clone();
        norm.synthetic = false;
        rt.pulse_mark(&norm.id, contract);
        assert_eq!(rt.once_per_signal_block_reason(&s, &norm, contract), None);
        // Feature off: no guard at all.
        s.once_per_signal = false;
        assert_eq!(rt.once_per_signal_block_reason(&s, &strat, contract), None);
    }

    #[tokio::test]
    async fn signal_end_debounce_ignores_a_single_flicker() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let id = "scan:1:PE";
        // First failing scan starts the debounce; not due yet.
        assert!(!rt.end_signal_if_due(id), "a lone failing scan does not end the signal");
        // The gate recovers on the next scan: the debounce is cancelled...
        rt.note_signal_pass(id);
        // ...so the following failure starts a brand-new count, still not due.
        assert!(!rt.end_signal_if_due(id), "the dip was cancelled, so the count restarts");
        // Simulate a continuous failure older than the debounce window.
        if let Ok(mut m) = rt.sig_fail_since.lock() {
            m.insert(id.to_string(), now_ms() - SIGNAL_END_DEBOUNCE_MS - 1);
        }
        assert!(rt.end_signal_if_due(id), "a sustained failure ends the signal");
    }

    #[tokio::test]
    async fn nifty_strict_gates_block_scanner_sides_on_line_disagreement() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        // Feature off: no gate ever applies.
        let off = Settings::default();
        assert!(rt.nifty_line_side_ok(&off, true));
        assert!(rt.nifty_line_side_ok(&off, false));

        // Trend on but no strict checkbox ticked: still a no-op.
        let mut plain = Settings::default();
        plain.nifty_trend_on = true;
        assert!(rt.nifty_line_side_ok(&plain, true));
        assert!(rt.nifty_line_side_ok(&plain, false));

        // NET-direction gate: CE needs net > 0, PE needs net < 0; a tie blocks.
        let mut net = Settings::default();
        net.nifty_trend_on = true;
        net.nifty_net_strict = true;
        rt.nifty_raw_net.store(1, Ordering::Relaxed);
        assert!(rt.nifty_line_side_ok(&net, true), "net bullish allows CE");
        assert!(!rt.nifty_line_side_ok(&net, false), "net bullish blocks PE");
        rt.nifty_raw_net.store(-1, Ordering::Relaxed);
        assert!(!rt.nifty_line_side_ok(&net, true), "net bearish blocks CE");
        assert!(rt.nifty_line_side_ok(&net, false), "net bearish allows PE");
        rt.nifty_raw_net.store(0, Ordering::Relaxed);
        assert!(!rt.nifty_line_side_ok(&net, true), "tie blocks CE");
        assert!(!rt.nifty_line_side_ok(&net, false), "tie blocks PE");

        // All-lines-agree gate: CE needs every line bull (+1), PE every bear (-1);
        // a mixed read (0) blocks both.
        let mut agree = Settings::default();
        agree.nifty_trend_on = true;
        agree.nifty_all_agree_strict = true;
        rt.nifty_agree.store(1, Ordering::Relaxed);
        assert!(rt.nifty_line_side_ok(&agree, true), "all-bull allows CE");
        assert!(!rt.nifty_line_side_ok(&agree, false), "all-bull blocks PE");
        rt.nifty_agree.store(-1, Ordering::Relaxed);
        assert!(!rt.nifty_line_side_ok(&agree, true), "all-bear blocks CE");
        assert!(rt.nifty_line_side_ok(&agree, false), "all-bear allows PE");
        rt.nifty_agree.store(0, Ordering::Relaxed);
        assert!(!rt.nifty_line_side_ok(&agree, true), "mixed blocks CE");
        assert!(!rt.nifty_line_side_ok(&agree, false), "mixed blocks PE");

        // Both ticked: BOTH must agree. Net bullish but a mixed line read fails
        // the agree gate, so the CE leg is still blocked.
        let mut both = Settings::default();
        both.nifty_trend_on = true;
        both.nifty_net_strict = true;
        both.nifty_all_agree_strict = true;
        rt.nifty_raw_net.store(1, Ordering::Relaxed);
        rt.nifty_agree.store(0, Ordering::Relaxed);
        assert!(!rt.nifty_line_side_ok(&both, true), "agree gate can veto a net-bullish CE");
        rt.nifty_agree.store(1, Ordering::Relaxed);
        assert!(rt.nifty_line_side_ok(&both, true), "both agree allows CE");
    }

    #[tokio::test]
    async fn apply_nifty_direction_tracks_raw_net_and_agree_state() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        // All three lines bullish (net +3) from NEUTRAL commits at once: net +1,
        // agree +1, side BULLISH.
        rt.apply_nifty_direction(vec!["a".into(), "b".into(), "c".into()], vec![], 3, "5min");
        assert_eq!(rt.nifty_raw_net.load(Ordering::Relaxed), 1);
        assert_eq!(rt.nifty_agree.load(Ordering::Relaxed), 1);
        assert_eq!(rt.nifty_dir.load(Ordering::Relaxed), 1);
        // Mixed on the SAME side (2 bull / 1 bear, net +1): net stays +1 but agree
        // drops to 0, so the all-lines-agree gate blocks even though net passes.
        rt.apply_nifty_direction(vec!["a".into(), "b".into()], vec!["c".into()], 1, "5min");
        assert_eq!(rt.nifty_raw_net.load(Ordering::Relaxed), 1);
        assert_eq!(rt.nifty_agree.load(Ordering::Relaxed), 0);
        // A lone opposite read must NOT flip the committed side - it starts the
        // confirmation window instead, so a per-tick wiggle cannot switch legs.
        rt.apply_nifty_direction(vec![], vec!["a".into(), "b".into()], -2, "5min");
        assert_eq!(rt.nifty_dir.load(Ordering::Relaxed), 1, "one opposite read holds the side");
        assert_eq!(rt.nifty_raw_net.load(Ordering::Relaxed), 1);
        // Only once the new side has persisted past the confirm window does it
        // commit: net -1, agree -1, side BEARISH.
        rt.nifty_pending_since
            .store(now_ms() - NIFTY_FLIP_CONFIRM_MS - 1, Ordering::Relaxed);
        rt.apply_nifty_direction(vec![], vec!["a".into(), "b".into()], -2, "5min");
        assert_eq!(rt.nifty_dir.load(Ordering::Relaxed), -1);
        assert_eq!(rt.nifty_raw_net.load(Ordering::Relaxed), -1);
        assert_eq!(rt.nifty_agree.load(Ordering::Relaxed), -1);
        // A dead-heat tie holds the committed side and its gate state - a
        // momentary tie must never free BOTH legs.
        rt.apply_nifty_direction(vec!["a".into()], vec!["b".into()], 0, "5min");
        assert_eq!(rt.nifty_dir.load(Ordering::Relaxed), -1);
        assert_eq!(rt.nifty_raw_net.load(Ordering::Relaxed), -1);
        assert_eq!(rt.nifty_agree.load(Ordering::Relaxed), -1);
    }

    #[tokio::test]
    async fn nifty_strict_gate_blocks_the_opposite_movers_side() {
        // With BOTH Top Gainers and Top Losers selected in Top Movers and the
        // NIFTY-trend strict gate ON, only the side the assigned straight-line
        // indicators confirm may build legs; the opposite side is dropped.
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let gainer = 13061; // 360ONE (NSE_EQ EQUITY)
        let loser = 21614; // ABCAPITAL (NSE_EQ EQUITY)
        if let Ok(mut g) = rt.movers_cache.lock() {
            *g = (
                now_ms(),
                json!({ "gainers": [{"securityId": gainer, "last": 100.0}], "losers": [{"securityId": loser, "last": 100.0}] }),
            );
        }
        let mut s = Settings::default();
        s.movers_on = true;
        s.nifty_trend_on = true;
        s.filters.insert("BullSlTrend".into(), true);
        s.filters.insert("BearSlTrend".into(), true);
        let ce = format!("scan:{}:CE", gainer);
        let pe = format!("scan:{}:PE", loser);
        let has = |out: &[Strategy], key: &str| out.iter().any(|t| t.id == key);

        // No strict gate: the old behaviour builds BOTH sides.
        assert!(has(&rt.scanner_targets(&s), &ce) && has(&rt.scanner_targets(&s), &pe), "baseline runs both sides");

        // NET gate, NIFTY bearish: Top Losers (PE) run, Top Gainers (CE) blocked.
        s.nifty_net_strict = true;
        rt.nifty_raw_net.store(-1, Ordering::Relaxed);
        let out = rt.scanner_targets(&s);
        assert!(!has(&out, &ce), "bearish NIFTY strictly blocks the Top Gainer CE leg");
        assert!(has(&out, &pe), "bearish NIFTY keeps the Top Loser PE leg");

        // NET gate, NIFTY bullish: the mirror image.
        rt.nifty_raw_net.store(1, Ordering::Relaxed);
        let out = rt.scanner_targets(&s);
        assert!(has(&out, &ce), "bullish NIFTY keeps the Top Gainer CE leg");
        assert!(!has(&out, &pe), "bullish NIFTY strictly blocks the Top Loser PE leg");

        // Tie (no committed direction): both sides blocked.
        rt.nifty_raw_net.store(0, Ordering::Relaxed);
        let out = rt.scanner_targets(&s);
        assert!(!has(&out, &ce) && !has(&out, &pe), "a tie blocks both sides");

        // ALL-LINES-AGREE gate: all-bearish keeps PE only, mixed blocks both.
        s.nifty_net_strict = false;
        s.nifty_all_agree_strict = true;
        rt.nifty_agree.store(-1, Ordering::Relaxed);
        let out = rt.scanner_targets(&s);
        assert!(has(&out, &pe) && !has(&out, &ce), "all-bearish keeps PE, blocks CE");
        rt.nifty_agree.store(0, Ordering::Relaxed);
        let out = rt.scanner_targets(&s);
        assert!(!has(&out, &ce) && !has(&out, &pe), "a mixed line read blocks both sides");

        // Gate OFF again: both sides come back (no permanent lock).
        s.nifty_all_agree_strict = false;
        assert!(has(&rt.scanner_targets(&s), &ce) && has(&rt.scanner_targets(&s), &pe), "gate off restores both sides");
    }

    #[tokio::test]
    async fn nifty_direction_owns_the_movers_side_over_the_auto_bias() {
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let gainer = 13061; // 360ONE (NSE_EQ EQUITY)
        let loser = 21614; // ABCAPITAL (NSE_EQ EQUITY)
        if let Ok(mut g) = rt.movers_cache.lock() {
            *g = (
                now_ms(),
                json!({ "gainers": [{"securityId": gainer, "last": 100.0}], "losers": [{"securityId": loser, "last": 100.0}] }),
            );
        }
        let mut s = Settings::default();
        s.movers_on = true;
        s.nifty_trend_on = true;
        s.filters.insert("BullSlTrend".into(), true);
        s.filters.insert("BearSlTrend".into(), true);
        // Auto Select Mode is ON and would pick the side from the movers bias.
        s.run_in_enabled = true;
        s.run_in_auto = true;
        let ce = format!("scan:{}:CE", gainer);
        let pe = format!("scan:{}:PE", loser);
        let has = |out: &[Strategy], key: &str| out.iter().any(|t| t.id == key);

        // A bullish NIFTY must arm the Top Gainer CE leg even though the movers
        // auto bias points PE (a down day with the index line rising).
        rt.mover_bias.store(-1, Ordering::Relaxed);
        rt.nifty_dir.store(1, Ordering::Relaxed);
        if let Ok(mut g) = rt.nifty_bull_filters.lock() {
            *g = vec!["autotrend".into()];
        }
        if let Ok(mut g) = rt.nifty_bear_filters.lock() {
            g.clear();
        }
        let out = rt.scanner_targets(&s);
        assert!(has(&out, &ce), "bullish NIFTY owns the side: Top Gainer CE must run over a PE auto bias");
        assert!(!has(&out, &pe), "bullish NIFTY must not arm the Top Loser PE leg");

        // Mirror: a bearish NIFTY owns the PE leg over a CE auto bias.
        rt.mover_bias.store(1, Ordering::Relaxed);
        rt.nifty_dir.store(-1, Ordering::Relaxed);
        if let Ok(mut g) = rt.nifty_bull_filters.lock() {
            g.clear();
        }
        if let Ok(mut g) = rt.nifty_bear_filters.lock() {
            *g = vec!["autotrend".into()];
        }
        let out = rt.scanner_targets(&s);
        assert!(has(&out, &pe), "bearish NIFTY owns the side: Top Loser PE must run over a CE auto bias");
        assert!(!has(&out, &ce), "bearish NIFTY must not arm the Top Gainer CE leg");
    }

    #[test]
    fn arrow_flip_fires_only_on_the_flip_bar() {
        let candles = wave(400, 100.0, 0.0);
        // Tokens whose arrows come from the line's own slope (wiggly series).
        let pairs: &[(&str, &str, usize)] = &[
            ("ArrowTrendCore", "vlcore", 0),
            ("ArrowZigZag", "zzline", 1),
            ("ArrowVl", "vl", 0),
            ("ArrowElliottWave", "ewtrend", 0),
            ("ArrowPriceAction", "patrend", 0),
            ("ArrowComboMaster", "trendmaster", 1),
            ("ArrowPaneConsensus", "panemaster", 0),
        ];
        let mut hit = None;
        for (token, id, idx) in pairs {
            for off in 1..250 {
                let now = series_dir(id, &[], *idx, true, &candles, off);
                let prev = series_dir(id, &[], *idx, true, &candles, off + 1);
                if now == Some(true) && prev == Some(false) {
                    hit = Some((*token, off));
                    break;
                }
            }
            if hit.is_some() {
                break;
            }
        }
        let (token, off) = hit.expect("at least one line must flip up somewhere in an oscillating series");
        assert_eq!(filter_eval(&format!("Bull{token}"), &candles, off), Some(true));
        assert_eq!(filter_eval(&format!("Bull{token}"), &candles, off + 1), Some(false));
        assert_eq!(filter_eval(&format!("Bear{token}"), &candles, off), Some(false));
    }

    #[test]
    fn straight_line_consensus_gate_tracks_majority_vote() {
        let candles = wave(400, 100.0, 0.05);
        let dirs = algo_core::indicators::sl_consensus_dir(&candles, 2, 5, 5.0);
        assert_eq!(dirs.len(), candles.len());
        let mut flips = 0;
        for off in 0..candles.len() {
            let d = dirs[dirs.len() - 1 - off];
            // Trend gate: bullish tolerates a rising/unknown line, bearish a
            // falling/unknown one, so a fresh 0 is non-blocking on both sides.
            assert_eq!(filter_eval("BullSlConsensus", &candles, off), Some(d >= 0));
            assert_eq!(filter_eval("BearSlConsensus", &candles, off), Some(d <= 0));
            if d != 0 {
                flips += 1;
            }
        }
        assert!(flips > 0, "consensus must resolve on an oscillating series");
        // Arrow gate fires only on the bar the majority vote flips to the side.
        let mut arrows = 0;
        for off in 0..candles.len() {
            if filter_eval("BullArrowConsensus", &candles, off) == Some(true) {
                arrows += 1;
            }
        }
        assert!(arrows > 0, "Arrow Consensus must fire at least once");
    }

    #[test]
    fn nifty_indicator_assignment_splits_call_and_put() {
        let conf: Vec<String> = ["autotrend", "zzline", "trendmaster", "projline"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // A steadily rising index resolves the straight-line trend indicators
        // bullish -> net positive, every id lands in the CE bucket.
        let up = ramp(400, 100.0, 0.6);
        let (bull, bear, net) = nifty_indicator_assignment(&up, &conf, 5, false);
        assert!(net > 0, "rising index must net bullish, got {net}");
        assert!(!bull.is_empty() && bear.is_empty());
        // The mirror-image falling index flips the assignment to the PE bucket.
        let down = ramp(400, 340.0, -0.6);
        let (bull2, bear2, net2) = nifty_indicator_assignment(&down, &conf, 5, false);
        assert!(net2 < 0, "falling index must net bearish, got {net2}");
        assert!(!bear2.is_empty() && bull2.is_empty());
        // Colour mode: each confirmation id's side must be exactly the COLOUR read
        // of that indicator's visible line on the last closed candle.
        let mut exp_bull: Vec<String> = Vec::new();
        let mut exp_bear: Vec<String> = Vec::new();
        for id in &conf {
            let out = algo_core::compute(id, &up, &algo_core::model::Settings::default());
            match out.get(sl_output_index(id)).and_then(|s| color_read(s, 0)) {
                Some(SlTrend::Bull) => exp_bull.push(id.clone()),
                Some(SlTrend::Bear) => exp_bear.push(id.clone()),
                _ => {}
            }
        }
        let (bull_c, bear_c, net_c) = nifty_indicator_assignment(&up, &conf, 5, true);
        assert_eq!(bull_c, exp_bull, "colour mode bull ids must match the per-indicator colour read");
        assert_eq!(bear_c, exp_bear, "colour mode bear ids must match the per-indicator colour read");
        assert_eq!(net_c, exp_bull.len() as i64 - exp_bear.len() as i64);
    }

    fn line_of(chronological: &[f64]) -> algo_core::model::SeriesOut {
        let mut s = algo_core::model::SeriesOut::default();
        s.data = chronological
            .iter()
            .enumerate()
            .map(|(i, v)| algo_core::model::Point { time: i as i64, value: *v, color: None })
            .collect();
        s
    }

    #[test]
    fn line_trend_reads_the_lines_latest_segment() {
        // The trend is the direction of the newest drawn segment - what the chart
        // paints on the right edge - NOT the net across a window of older legs.
        // A line that fell then turned up is BULLISH now, even though the net over
        // the last few vertices is still down: the old windowed read reported the
        // stale side here, which is why NIFTY-trend stuck on PE through a turn.
        let turn_up = line_of(&[10.0, 6.0, 9.0, 5.0, 8.0]);
        assert_eq!(straight_line_trend(&turn_up, 0, 5), Some(SlTrend::Bull));
        // A line that rose then turned down is BEARISH now.
        let turn_down = line_of(&[4.0, 8.0, 5.0, 9.0, 6.0]);
        assert_eq!(straight_line_trend(&turn_down, 0, 5), Some(SlTrend::Bear));
        // A clear rise stays bullish.
        let rise = line_of(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(straight_line_trend(&rise, 0, 5), Some(SlTrend::Bull));
        // Flat line -> flat (upstream keeps the previous side).
        let flat = line_of(&[3.0, 3.0, 3.0, 3.0, 3.0]);
        assert_eq!(straight_line_trend(&flat, 0, 5), Some(SlTrend::Flat));
        // Not enough history -> unresolved.
        assert_eq!(straight_line_trend(&line_of(&[1.0]), 0, 5), None);
        assert_eq!(straight_line_trend(&line_of(&[1.0, 2.0]), 0, 5), None);
    }

    fn line_colored(vals: &[(f64, &str)]) -> algo_core::model::SeriesOut {
        let mut s = algo_core::model::SeriesOut::default();
        s.data = vals
            .iter()
            .enumerate()
            .map(|(i, (v, c))| algo_core::model::Point {
                time: i as i64,
                value: *v,
                color: if c.is_empty() { None } else { Some((*c).to_string()) },
            })
            .collect();
        s
    }

    #[test]
    fn color_read_reads_the_line_end_colour() {
        // The renderer paints each segment with its END point's colour, so the
        // segment drawn across the latest candle carries the NEWEST point's
        // colour. Read that - never a fixed `len - 2`, which on the sparse
        // straight-line series (one point per vertex) can be days old.
        let green_end = line_colored(&[(1.0, ""), (2.0, SL_GREEN), (3.0, SL_GREEN), (4.0, SL_GREEN)]);
        assert_eq!(color_read(&green_end, 0), Some(SlTrend::Bull));
        let red_end = line_colored(&[(5.0, SL_RED), (4.0, SL_RED), (3.0, SL_RED)]);
        assert_eq!(color_read(&red_end, 0), Some(SlTrend::Bear));
        // offset skips the newest points.
        let mixed = line_colored(&[(1.0, SL_GREEN), (2.0, SL_GREEN), (3.0, SL_RED)]);
        assert_eq!(color_read(&mixed, 0), Some(SlTrend::Bear));
        assert_eq!(color_read(&mixed, 1), Some(SlTrend::Bull));
        // Missing / unknown colour -> None (non-blocking), never a side.
        let none = line_colored(&[(1.0, ""), (2.0, "#123456")]);
        assert_eq!(color_read(&none, 0), None);
    }

    #[test]
    fn straight_line_color_filters_match_the_visible_line_colour() {
        // Each SlColor* gate must equal the COLOUR read of the VISIBLE overlay the
        // chart draws (e.g. ElliottWave -> wavefib), on the last closed candle.
        let tokens = [
            "ElliottWave", "SupplyDemand", "PriceAction", "ZigZag", "ComboMaster",
            "PaneConsensus", "AutoTrendline", "Pitchfork", "TrendProjection",
            "GannFan", "FibFan", "Srema", "Consensus",
        ];
        let up = ramp(400, 100.0, 0.6);
        let down = ramp(400, 340.0, -0.6);
        let mut saw_bull = 0;
        let mut saw_bear = 0;
        for candles in [&up, &down] {
            for tok in tokens {
                let (id, idx) = sl_color_series(tok).unwrap();
                let s = ind_series(id, candles, &algo_core::model::Settings::default());
                let read = color_read(s.get(idx).unwrap(), 0);
                let (bull_expect, bear_expect) = match read {
                    Some(SlTrend::Bull) => (Some(true), Some(false)),
                    Some(SlTrend::Bear) => (Some(false), Some(true)),
                    _ => (Some(true), Some(true)),
                };
                assert_eq!(
                    filter_eval(&format!("BullSlColor{tok}"), candles, 0),
                    bull_expect,
                    "bull colour for {tok}"
                );
                assert_eq!(
                    filter_eval(&format!("BearSlColor{tok}"), candles, 0),
                    bear_expect,
                    "bear colour for {tok}"
                );
                match read {
                    Some(SlTrend::Bull) => saw_bull += 1,
                    Some(SlTrend::Bear) => saw_bear += 1,
                    _ => {}
                }
            }
        }
        // The colour read must actually flip with the line, not always agree.
        assert!(saw_bull > 0, "at least one visible line must read GREEN somewhere");
        assert!(saw_bear > 0, "at least one visible line must read RED somewhere");
    }

    #[test]
    fn color_scan_reuses_the_sample_until_the_window_elapses() {
        // The colour detection is read on a fixed cadence: a sample is held for
        // COLOR_SCAN_MS and only then re-derived, no matter how many ticks arrive.
        let cons = ConsensusCfg::default();
        let sup = PivotTrendCfg::default();
        let res = PivotTrendCfg::default();
        let candles = wave(400, 100.0, 0.3);
        let k = "BullSlColorAutoTrendline";
        let key: ColorSampleKey = (k.to_string(), candles[0].time, candles.len(), 0);
        color_samples().lock().unwrap().clear();
        let fresh = filter_eval(k, &candles, 0);
        let forced = if fresh == Some(true) { Some(false) } else { Some(true) };
        color_samples().lock().unwrap().insert(key.clone(), ColorSample { at_ms: now_ms(), val: forced });
        assert_eq!(
            color_filter_sampled(k, &candles, 0, cons, sup, res),
            forced,
            "within the window the cached colour sample must be reused"
        );
        {
            let mut g = color_samples().lock().unwrap();
            if let Some(s) = g.get_mut(&key) {
                s.at_ms = now_ms() - COLOR_SCAN_MS - 1;
            }
        }
        assert_eq!(
            color_filter_sampled(k, &candles, 0, cons, sup, res),
            fresh,
            "an expired sample must be re-read"
        );
        color_samples().lock().unwrap().clear();
    }

    #[test]
    fn nifty_timeframe_follows_the_chart_checkboxes() {
        // The NIFTY engine must run on the exact timeframe the operator ticked
        // for the charts: 1 min, 5 min, or both (both must agree before a flip).
        let mut s = Settings::default();
        s.tf_1min = false;
        s.tf_5min = true;
        assert_eq!(nifty_timeframe(&s), "5min");
        s.tf_1min = true;
        s.tf_5min = false;
        assert_eq!(nifty_timeframe(&s), "1min");
        s.tf_1min = true;
        s.tf_5min = true;
        assert_eq!(nifty_timeframe(&s), "both");
        // Neither ticked falls back to the engine default, never an empty tf.
        s.tf_1min = false;
        s.tf_5min = false;
        assert_eq!(nifty_timeframe(&s), "5min");
        assert_eq!(nifty_dir_label(1), "BULLISH(CE)");
        assert_eq!(nifty_dir_label(-1), "BEARISH(PE)");
        assert_eq!(nifty_dir_label(0), "NEUTRAL");
    }

    #[test]
    fn straight_line_consensus_warmup_is_non_blocking() {
        let candles = ramp(3, 100.0, 1.0);
        // Too little history for the voters to resolve => unknown => pass.
        assert_eq!(filter_eval("BullSlConsensus", &candles, 0), Some(true));
        assert_eq!(filter_eval("BearSlConsensus", &candles, 0), Some(true));
        assert_eq!(filter_eval("BullArrowConsensus", &candles, 0), Some(false));
    }

    #[test]
    fn straight_line_arrow_filters_use_pivot_trend() {
        // Constant-slope fits (auto trendline, pitchfork, projection, fans, S/R
        // EMA reversal) cannot flip on a per-bar slope change, so their arrow
        // filters read the fractal swing structure instead, matching the chart.
        let candles = wave(400, 100.0, 0.05);
        for token in ["ArrowAutoTrendline", "ArrowTrendProjection", "ArrowPitchfork", "ArrowSrema", "ArrowGannFan", "ArrowFibFan"] {
            let mut resolved = 0;
            for off in 0..candles.len() {
                let expect = algo_core::indicators::trend_flip_at(&candles, off, true, 5.0);
                let expect_bear = algo_core::indicators::trend_flip_at(&candles, off, false, 5.0);
                assert_eq!(filter_eval(&format!("Bull{token}"), &candles, off), expect);
                assert_eq!(filter_eval(&format!("Bear{token}"), &candles, off), expect_bear);
                if expect == Some(true) {
                    resolved += 1;
                }
            }
            assert!(resolved > 0, "{token} must fire at least once on an oscillating series");
        }
    }

    #[test]
    fn ema1_is_the_bar_and_drives_only_crosses() {
        // EMA(1) == the bar, and it is read only by the cross-above/below
        // helpers; level, trend and gap confirmations stay on the raw candle.
        let candles = wave(200, 100.0, 0.1);
        for off in 0..10 {
            assert_eq!(ema1(&candles, off), close_at(&candles, off), "EMA(1) must equal the bar");
            assert_eq!(ema_val(1, &candles, off), close_at(&candles, off));
        }
        // Cross filters named via EMA(1) agree with the close-based helpers.
        assert_eq!(filter_eval("BullEma1_9", &candles, 0), cross_close_ema(9, &candles, 0, true));
        assert_eq!(filter_eval("BearEma1_9", &candles, 0), cross_close_ema(9, &candles, 0, false));
        // Level/trend confirmations read the raw candle close, not an EMA1 step.
        assert_eq!(
            filter_eval("BullGtUp", &candles, 0),
            close_at(&candles, 0).zip(ema_val(9, &candles, 0)).map(|(c, e)| c > e)
        );
        // Gap filters use the raw candle body (open vs prior high/low).
        let mut gapped = ramp(120, 100.0, 0.0);
        let n = gapped.len();
        let prev_high = gapped[n - 2].high;
        gapped[n - 1].open = prev_high + 1.0;
        gapped[n - 1].high = prev_high + 2.0;
        gapped[n - 1].close = prev_high + 1.5;
        assert_eq!(filter_eval("BullGapUp", &gapped, 0), Some(true));
        assert_eq!(filter_eval("BearGapDown", &gapped, 0), Some(false));
    }

    #[test]
    fn movers_auto_bias_follows_selection_not_market_breadth() {
        // Only gainers selected -> CE regardless of a broadly bearish market.
        assert_eq!(movers_auto_bias(true, false, 5, 0, 1, 9), 1);
        // Only losers selected -> PE regardless of a broadly bullish market.
        assert_eq!(movers_auto_bias(false, true, 0, 5, 9, 1), -1);
        // Both legs: the side with more live rows wins.
        assert_eq!(movers_auto_bias(true, true, 5, 3, 1, 9), 1);
        assert_eq!(movers_auto_bias(true, true, 3, 5, 9, 1), -1);
        // Exact tie falls back to market breadth, then neutral.
        assert_eq!(movers_auto_bias(true, true, 4, 4, 2, 8), -1);
        assert_eq!(movers_auto_bias(true, true, 4, 4, 8, 2), 1);
        assert_eq!(movers_auto_bias(true, true, 4, 4, 5, 5), 0);
        // Nothing selected -> neutral (manual side fallback).
        assert_eq!(movers_auto_bias(false, false, 0, 0, 9, 1), 0);
    }

    #[tokio::test]
    async fn both_movers_legs_disable_the_single_side_auto_bias() {
        // Selecting BOTH the Top Gainers and Top Losers legs must let both the
        // CE and PE scanner legs run instead of collapsing onto the movers
        // dominance side. With a single leg selected the bias still picks it.
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);

        let mut s = Settings::default();
        s.movers_on = true;
        s.movers_gainers = 5;
        s.movers_losers = 5;
        s.run_in_enabled = true;
        s.run_in_auto = true;
        s.run_in_side = "PE".into();
        rt.mover_bias.store(1, Ordering::Relaxed);

        // Both legs selected: no single auto side, and the manual run-in side
        // must not re-narrow the scanner either.
        assert_eq!(rt.auto_option_side(&s), None);
        assert_eq!(rt.effective_run_in_side(&s), None);
        assert_eq!(rt.active_side(&s), None);

        // Only the gainer leg selected: the auto bias picks CE again.
        s.movers_losers = 0;
        assert_eq!(rt.auto_option_side(&s), Some("CE"));
        assert_eq!(rt.active_side(&s), Some("CE"));

        // Only the loser leg selected: the auto bias picks PE.
        rt.mover_bias.store(-1, Ordering::Relaxed);
        s.movers_gainers = 0;
        s.movers_losers = 5;
        assert_eq!(rt.auto_option_side(&s), Some("PE"));
        assert_eq!(rt.active_side(&s), Some("PE"));

        // Mover scan off: the manual run-in side takes over again.
        rt.mover_bias.store(1, Ordering::Relaxed);
        s.movers_on = false;
        assert_eq!(rt.auto_option_side(&s), None);
        assert_eq!(rt.effective_run_in_side(&s), Some("PE"));
    }

    #[test]
    fn option_strike_type_parses_resolved_legs() {
        assert_eq!(option_strike_type("POLICYBZR-Sep2026-1900-PE"), Some((1900.0, "PE")));
        assert_eq!(option_strike_type("CHOLAFIN-Sep2026-1800-PE"), Some((1800.0, "PE")));
        assert_eq!(option_strike_type("NIFTY-Sep2026-24500-CE"), Some((24500.0, "CE")));
        // Non-options and malformed legs must not be treated as option fills.
        assert_eq!(option_strike_type("POLICYBZR"), None);
        assert_eq!(option_strike_type("POLICYBZR-Sep2026-FUT"), None);
        assert_eq!(option_strike_type(""), None);
    }

    #[test]
    fn paper_charges_are_side_aware_and_positive() {
        // A long and its mirrored short must pay the same statutory total: the
        // sell leg carries STT and the buy leg carries stamp, whichever way the
        // trade is expressed.
        let long = compute_charges_for_trade(100.0, 120.0, 50.0, "BUY", "OPTIDX", "NIFTY 24000 CE", 1000.0).unwrap();
        let short = compute_charges_for_trade(120.0, 100.0, 50.0, "SELL", "OPTIDX", "NIFTY 24000 CE", -1000.0).unwrap();
        assert!(long.total > 0.0, "charges must be positive");
        assert_eq!(long.segment, "options");
        assert!((long.total - short.total).abs() < 0.01, "long/short charges should match: {} vs {}", long.total, short.total);
        // net = gross - total round-trip charges.
        assert!((long.net - (long.gross - long.total)).abs() < 0.01);
    }

    #[test]
    fn paper_charge_segments_and_rounding_match_dhan() {
        // Instrument / symbol drives the segment, exactly like Python segmentFor().
        assert_eq!(charges_segment("OPTIDX", "NIFTY 24000 CE"), "options");
        assert_eq!(charges_segment("FUTSTK", "RELIANCE FUT"), "futures");
        assert_eq!(charges_segment("", "NIFTY 24000 PE"), "options");
        assert_eq!(charges_segment("EQUITY", "RELIANCE"), "delivery");
        // Delivery: no brokerage, STT 0.1% on BOTH legs.
        let buy = side_charges("delivery", true, 100_000.0);
        let sell = side_charges("delivery", false, 100_000.0);
        assert_eq!(buy.brokerage, 0.0);
        assert_eq!(buy.stt, 100.0);
        assert_eq!(sell.stt, 100.0);
        // STT rounds to the nearest rupee (0.0625% of 1000 = 0.625 -> 1).
        assert_eq!(side_charges("options", false, 1000.0).stt, 1.0);
        // Stamp rounds to the nearest rupee (0.015% of 10000 = 1.5 -> 2).
        assert_eq!(side_charges("delivery", true, 10_000.0).stamp, 2.0);
        // Options brokerage is a flat Rs 20, so a bigger turnover bends the curve.
        let one = side_charges("options", true, 1_000_000.0);
        let two = side_charges("options", true, 2_000_000.0);
        assert!(two.total < 2.0 * one.total, "brokerage cap should bend the curve: {} -> {}", one.total, two.total);
    }

    #[test]
    fn paper_fill_price_is_adverse_and_bounded() {
        // BUY fills above the mark, SELL below it.
        assert!(paper_fill_price(100.0, true, 50.0) > 100.0);
        assert!(paper_fill_price(100.0, false, 50.0) < 100.0);
        // Zero slippage is an exact fill; a SELL can never fill below zero.
        assert_eq!(paper_fill_price(100.0, true, 0.0), 100.0);
        assert!(paper_fill_price(1.0, false, 100_000.0) >= 0.05);
        // Rejection PRNG stays in [0, 1) for many seeds.
        for i in 0..1000 {
            let r = paper_rand01(paper_seed(i));
            assert!((0.0..1.0).contains(&r), "rand out of range: {r}");
        }
    }

    #[test]
    fn fno_limit_buy_price_is_marketable_and_above_entry() {
        // The F&O limit must always sit strictly above the market, so it can
        // never be placed behind/below the entry, and it fills like a market
        // order. Checked across cheap and expensive premiums.
        for px in [0.05, 1.0, 12.76, 100.0, 500.0, 2500.0] {
            let limit = fno_limit_buy_price(px);
            assert!(limit > px, "limit {limit} must be above entry {px}");
            // At least one tick of protection so the order is marketable.
            assert!(limit - px >= 0.049, "limit {limit} too close to entry {px}");
        }
    }

    #[test]
    fn settings_camel_case_roundtrip() {
        let mut s = Settings::default();
        s.option_side = "CE".into();
        s.brain_mode = "auto".into();
        s.all_in_one = true;
        s.filters.insert("BullIncUp".into(), true);
        // Selected Strategies section: manual / AI-pick / Run Strategy In controls
        // must round-trip with the exact camelCase keys the frontend sends.
        s.call_manual = false;
        s.ai_pick = true;
        s.ai_pick_n = 3;
        s.ai_pick_bull = true;
        s.ai_pick_bear = false;
        s.run_in_enabled = true;
        s.run_in_side = "PE".into();
        s.run_in_auto = true;
        // Run-mode section: the frontend sends `filterMode` for the
        // Normal / Indicator-filters mode selector.
        s.filter_mode = true;
        // Scanner "remove" blacklist: the frontend posts `scannerExclude` (int
        // security ids removed from the Top Movers / NIFTY trend picks).
        s.scanner_exclude = vec![17818, 1234];
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["optionSide"], json!("CE"));
        assert_eq!(v["brainMode"], json!("auto"));
        assert_eq!(v["oppositeVeto"], json!("relative"));
        assert_eq!(v["allInOne"], json!(true));
        assert_eq!(v["filterMode"], json!(true));
        assert_eq!(v["filters"]["BullIncUp"], json!(true));
        assert_eq!(v["callManual"], json!(false));
        assert_eq!(v["aiPick"], json!(true));
        assert_eq!(v["aiPickN"], json!(3));
        assert_eq!(v["aiPickBull"], json!(true));
        assert_eq!(v["aiPickBear"], json!(false));
        assert_eq!(v["runInEnabled"], json!(true));
        assert_eq!(v["runInSide"], json!("PE"));
        assert_eq!(v["runInAuto"], json!(true));
        assert_eq!(v["scannerExclude"], json!([17818, 1234]));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(back.option_side, "CE");
        assert!(!back.call_manual && back.ai_pick && back.ai_pick_n == 3);
        assert!(back.run_in_enabled && back.run_in_side == "PE" && back.run_in_auto);
        assert!(back.filter_mode);
        assert_eq!(back.scanner_exclude, vec![17818, 1234]);
        assert_eq!(back.opposite_veto, "relative");
    }

    #[test]
    fn unknown_filter_blocks_fail_closed() {
        let c = ramp(60, 100.0, 1.0);
        assert_eq!(filter_eval("BullSomeUnimplementedThing", &c, 0), None);
        let mut s = Settings::default();
        s.filters.insert("BullSomeUnimplementedThing".into(), true);
        // An armed filter that cannot be computed no longer counts as a pass, so
        // it blocks instead of firing against the chart.
        assert!(!filter_gate(&s, &bull_strategy(), &c, 0));
    }

    #[test]
    fn strict_and_gate_blocks_on_failure() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        s.filters.insert("BullIncUp".into(), true);
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "rising close passes BullIncUp");
        // The armed bull filter is met while the strategy is bearish: the opposite
        // agreement must veto the entry (it is no longer silently ignored).
        assert!(!filter_gate(&s, &bear_strategy(), &c, 0), "met opposite filter vetoes");
        s.filters.insert("BearIncDown".into(), true);
        assert!(!filter_gate(&s, &bear_strategy(), &c, 0), "opposing down filter fails");
    }

    #[test]
    fn premium_chart_only_gates_every_side_on_bull_filters() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        s.filters.insert("BullIncUp".into(), true);
        // The operator also has a bearish filter ticked (e.g. for the PE side).
        s.filters.insert("BearIncDown".into(), true);
        // Normally a bearish strategy is gated by its own bearish filters; the
        // failing bear filter plus the met opposite (bull) filter vetoes.
        assert!(!filter_gate(&s, &bear_strategy(), &c, 0), "bear side uses bear filters normally");
        // "Premium chart only (run + trade)": every side is gated by the bullish
        // filter set, so the rising ramp passes on the PE side too and the ticked
        // bearish filter can no longer veto it.
        s.premium_only = true;
        assert!(filter_gate(&s, &bear_strategy(), &c, 0), "premium chart uses bull filters on the PE side");
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "premium chart uses bull filters on the CE side");
    }

    #[test]
    fn strict_line_gate_covers_direction_and_colour_line_filters() {
        let _g = ind_test_lock();
        let up = ramp(400, 100.0, 0.6);
        let down = ramp(400, 340.0, -0.6);
        let tokens = [
            "AutoTrendline", "ElliottWave", "ZigZag", "SupplyDemand", "PriceAction",
            "GannFan", "Pitchfork", "TrendProjection",
        ];
        // A "Straight Line Indicators" DIRECTION row whose bull gate trends up on
        // the rising ramp and down on the falling ramp...
        let dir_tok = tokens
            .iter()
            .find(|tok| {
                filter_eval(&format!("BullSl{tok}"), &up, 0) == Some(true)
                    && filter_eval(&format!("BullSl{tok}"), &down, 0) == Some(false)
            })
            .expect("a bull line whose direction is up / down");
        // ...and a COLOUR-detection row that reads green up / red down.
        let col_tok = tokens
            .iter()
            .find(|tok| {
                filter_eval(&format!("BullSlColor{tok}"), &up, 0) == Some(true)
                    && filter_eval(&format!("BullSlColor{tok}"), &down, 0) == Some(false)
            })
            .expect("a bull line that is green up / red down");
        color_samples().lock().unwrap().clear();

        let mut bull = bull_strategy();
        bull.synthetic = true;
        let mut bear = bear_strategy();
        bear.synthetic = true;

        // Direction row only.
        let mut s = Settings::default();
        s.strict_line_color = true;
        s.filters.insert(format!("BullSl{dir_tok}"), true);
        assert!(strict_line_gate_ok(&s, &bull, &up, 0), "direction row confirms an up line");
        assert!(!strict_line_gate_ok(&s, &bull, &down, 0), "direction row blocks a down line");
        // The row is a bull filter, so it never gates the opposite PE leg.
        assert!(strict_line_gate_ok(&s, &bear, &down, 0), "opposite-side row does not gate the PE leg");
        color_samples().lock().unwrap().clear();

        // Colour row only.
        let mut s2 = Settings::default();
        s2.strict_line_color = true;
        s2.filters.insert(format!("BullSlColor{col_tok}"), true);
        color_samples().lock().unwrap().clear();
        assert!(strict_line_gate_ok(&s2, &bull, &up, 0), "colour row confirms a green line");
        color_samples().lock().unwrap().clear();
        assert!(!strict_line_gate_ok(&s2, &bull, &down, 0), "colour row blocks a red line");
        color_samples().lock().unwrap().clear();

        // Both sections together: either dissenting line vetoes the leg.
        s.filters.insert(format!("BullSlColor{col_tok}"), true);
        color_samples().lock().unwrap().clear();
        assert!(!strict_line_gate_ok(&s, &bull, &down, 0), "either section can veto");
        // A non-line filter (EMA family) that fails must NOT be read by this gate.
        let mut s3 = Settings::default();
        s3.strict_line_color = true;
        s3.filters.insert("BullIncUp".into(), true);
        assert!(strict_line_gate_ok(&s3, &bull, &down, 0), "non-line filters are not part of the strict gate");
        color_samples().lock().unwrap().clear();

        // Feature off -> non-blocking again.
        s.strict_line_color = false;
        assert!(strict_line_gate_ok(&s, &bull, &down, 0), "gate off -> no line veto");
        // NIFTY Trend Following ON: the manual all-agree gate is skipped for
        // scanner legs - the assigned NIFTY indicators own the opposite-side
        // block (niftyNetStrict / niftyAllAgreeStrict), so ticked lines must not
        // stall every entry.
        s.strict_line_color = true;
        s.nifty_trend_on = true;
        assert!(strict_line_gate_ok(&s, &bull, &down, 0), "NIFTY trend owns the strict block");
        s.nifty_trend_on = false;
        // Normal-mode (non-synthetic) strategies are never line-gated.
        s.strict_line_color = true;
        bull.synthetic = false;
        assert!(strict_line_gate_ok(&s, &bull, &down, 0), "normal-mode strategy is not line-gated");
        color_samples().lock().unwrap().clear();
    }

    #[test]
    fn straight_line_filter_predicate_matches_both_sections() {
        for k in [
            "BullSlAutoTrendline", "BearSlPitchfork", "BullVl", "BearVl",
            "BullSlConsensus", "BearSlSupport", "BearSlResistance",
            "BullSlColorAutoTrendline", "BearSlColorVl", "BearSlColorConsensus",
        ] {
            assert!(is_straight_line_filter(k), "{k} must be a straight-line filter");
        }
        for k in [
            "BullArrowZigZag", "BearArrowVl", "BullIncUp", "BearIncDown", "BullOit",
            "BearOit", "BullCandle", "BullEmaTrend9", "BearMacd", "BullMeetOvlHma",
        ] {
            assert!(!is_straight_line_filter(k), "{k} must NOT be a straight-line filter");
        }
    }

    #[test]
    fn all_in_one_toggles_strict_and_vs_majority() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        // On a rising ramp two of the three ticked bull filters pass, one fails.
        s.filters.insert("BullIncUp".into(), true);
        s.filters.insert("BullIncUpAll".into(), true);
        s.filters.insert("BullLtUp".into(), true);
        // Non-strict (default): a majority of the bull filters pass -> allowed.
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "majority clears the non-strict gate");
        // Strict AND: one of the three bull filters fails -> blocked.
        s.all_in_one = true;
        assert!(!filter_gate(&s, &bull_strategy(), &c, 0), "strict AND blocks when any filter fails");
    }

    #[test]
    fn filter_mode_uses_majority_and_combines_with_brain_auto() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        // On a rising ramp two of the three ticked bull filters pass, one fails.
        s.filters.insert("BullIncUp".into(), true);
        s.filters.insert("BullIncUpAll".into(), true);
        s.filters.insert("BullLtUp".into(), true);
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "non-filter majority clears");
        // Indicator-filters mode no longer forces strict AND: a majority still
        // clears the gate while the checkbox is off.
        s.filter_mode = true;
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "filter mode falls back to majority");
        // AI Brain AUTO layers on top: a threshold the majority score cannot
        // reach blocks the entry.
        s.brain_mode = "auto".into();
        s.brain_threshold = 90;
        assert!(!filter_gate(&s, &bull_strategy(), &c, 0), "brain threshold above majority score blocks");
        // A threshold the majority score clears allows the entry.
        s.brain_threshold = 50;
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "brain threshold below majority score allows");
        // The strict-AND checkbox alone re-imposes "every filter must pass".
        s.brain_mode = "off".into();
        s.all_in_one = true;
        assert!(!filter_gate(&s, &bull_strategy(), &c, 0), "strict AND blocks when any filter fails");
        // Every ticked filter passing => allowed even under strict AND.
        s.filters.clear();
        s.filters.insert("BullIncUp".into(), true);
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "single passing filter allowed");
    }

    #[test]
    fn arrow_filters_collapse_to_one_or_trigger() {
        let c = wave(400, 100.0, 0.0);
        let mut s = Settings::default();
        s.filter_mode = true;
        s.filters.insert("BullArrowZigZag".into(), true);
        s.filters.insert("BullArrowTrendCore".into(), true);
        // A bar where at least one of the two selected arrows prints.
        let off = (1..300)
            .find(|&o| {
                filter_eval("BullArrowZigZag", &c, o) == Some(true)
                    || filter_eval("BullArrowTrendCore", &c, o) == Some(true)
            })
            .expect("at least one arrow must fire on an oscillating series");
        // All arrow toggles collapse into ONE OR-ed condition, so strict AND with
        // only arrows passes when any single arrow fires (they would almost never
        // print on the same tick if each were counted separately).
        let (ok, f) = filter_gate_facts(&s, &bull_strategy(), &c, off);
        assert!(ok, "any fresh arrow clears the collapsed arrow trigger");
        assert_eq!(f.total, 1, "all enabled arrows collapse into one condition");
        // Ordinary filters stay separate and are combined with the arrow group.
        s.filters.insert("BullIncUp".into(), true);
        let (_, f2) = filter_gate_facts(&s, &bull_strategy(), &c, off);
        assert_eq!(f2.total, 2, "arrow group + one normal filter = two conditions");
    }

    #[test]
    fn brain_auto_conflict_veto_blocks_strong_opposite() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        s.brain_mode = "auto".into();
        s.brain_threshold = 50;
        s.all_in_one = true;
        s.filters.insert("BullIncUp".into(), true);
        // Own side passes and there is no opposite agreement.
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "no opposite agreement => allowed");
        // Opposite side now leads STRICTLY: the own side has no passing filter
        // while a passing opposite filter (Elder Force is a non-blocking
        // confirmer that always evaluates true) counts against it. The relative
        // veto fires even though Brain mode alone would not reach this branch.
        s.filters.clear();
        s.filters.insert("BullIncDown".into(), true);
        s.filters.insert("BearPbrElderforce".into(), true);
        let (ok, f) = filter_gate_facts(&s, &bull_strategy(), &c, 0);
        assert!(!ok && f.veto, "opposite leading strictly vetoes even under Brain AUTO");
    }

    #[test]
    fn brain_auto_threshold() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        s.brain_mode = "auto".into();
        s.brain_threshold = 50;
        s.filters.insert("BullIncUp".into(), true);
        s.filters.insert("BullEmaTrend9".into(), true);
        s.filters.insert("BullIncUpAll".into(), true);
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "all three pass => score 100");
        s.filters.clear();
        s.filters.insert("BullIncUp".into(), true);
        s.filters.insert("BearIncDown".into(), true);
        s.brain_threshold = 50;
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "bear filter evaluates false so no veto");
    }

    #[test]
    fn opposite_filter_vetoes_without_brain() {
        let c = ramp(60, 100.0, 1.0);
        let mut s = Settings::default();
        s.filters.insert("BullIncUp".into(), true);
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "own side passes => allowed");
        s.filters.insert("BearIncDown".into(), true);
        // On a rising ramp the bear filter is unmet, so it must not veto.
        assert!(filter_gate(&s, &bull_strategy(), &c, 0), "unmet opposite does not veto");
        // Opposite now leads STRICTLY: the own side has no passing filter and a
        // met opposite filter (Elder Force always evaluates true) counts once -
        // so the relative veto fires even with Brain off.
        s.filters.clear();
        s.filters.insert("BullIncDown".into(), true);
        s.filters.insert("BearPbrElderforce".into(), true);
        let (ok, f) = filter_gate_facts(&s, &bull_strategy(), &c, 0);
        assert!(!ok && f.veto, "opposite leading strictly vetoes with brain off");
    }

    #[test]
    fn opposite_veto_relative_tie_and_off() {
        let c = ramp(60, 100.0, 1.0);
        // Tie (own 1 vs opposite 1): a symmetric Bull+Bear filter set must not
        // deadlock into "no trade" - the relative veto leaves the entry to the
        // ordinary own-side majority rule.
        let mut s = Settings::default();
        s.filters.insert("BullIncUp".into(), true);
        s.filters.insert("BearPbrElderforce".into(), true);
        let (ok, f) = filter_gate_facts(&s, &bull_strategy(), &c, 0);
        assert!(ok && !f.veto, "tie (1 vs 1) must not veto");

        // Mode `off`: even when the opposite side strictly leads, no veto is set.
        let mut s = Settings::default();
        s.opposite_veto = "off".into();
        s.filters.insert("BullIncDown".into(), true);
        s.filters.insert("BearPbrElderforce".into(), true);
        let (_, f) = filter_gate_facts(&s, &bull_strategy(), &c, 0);
        assert!(!f.veto, "opposite veto OFF must never set veto");

        // Mode `relative` (the default): opposite strictly leading does veto.
        let mut s = Settings::default();
        assert_eq!(s.opposite_veto, "relative");
        s.filters.insert("BullIncDown".into(), true);
        s.filters.insert("BearPbrElderforce".into(), true);
        let (_, f) = filter_gate_facts(&s, &bull_strategy(), &c, 0);
        assert!(f.veto, "relative veto fires when opposite strictly leads");
    }

    #[test]
    fn direction_guard_blocks_opposite() {
        let up = ramp(60, 100.0, 1.0);
        let down = ramp(60, 200.0, -1.0);
        let mut s = Settings::default();
        s.dir_guard = true;
        assert!(!direction_opposite(&s, &bull_strategy(), &up, 0), "aligned bull not blocked");
        assert!(direction_opposite(&s, &bull_strategy(), &down, 0), "clear opposite majority blocked");
        assert!(!direction_opposite(&s, &bear_strategy(), &down, 0), "aligned bear not blocked");
        s.dir_guard = false;
        assert!(!direction_opposite(&s, &bull_strategy(), &down, 0), "guard off never blocks");
    }

    #[test]
    fn scanner_sides_must_match_the_active_trade_side() {
        // No committed side: each side runs on its own gating filters.
        assert_eq!(side_allowed(true, true, None), (true, true));
        // Committed to CE: only the bull side (which the CE leg follows) may run.
        assert_eq!(side_allowed(true, true, Some("CE")), (true, false));
        assert_eq!(side_allowed(true, true, Some("PE")), (false, true));
        // Regression: only Bear filters armed but the engine is forced into CE
        // (manual "Run Strategy In" with no auto direction). The bearish side
        // cannot gate a CE entry, so nothing may run - previously the bearish
        // strategy still built and fired an opposite CE order.
        assert_eq!(side_allowed(false, true, Some("CE")), (false, false));
        assert_eq!(side_allowed(true, false, Some("PE")), (false, false));
        // An unarmed side can never be gated, even without a committed side.
        assert_eq!(side_allowed(false, true, None), (false, true));
        assert_eq!(side_allowed(true, false, None), (true, false));
    }

    #[test]
    fn filter_side_routing_forces_the_filter_side() {
        let bull = bull_strategy();
        let bear = bear_strategy();
        let mut s = Settings::default();
        // Off: no forced side, normal multi-source routing applies.
        assert_eq!(routed_option_side(&s, &bull), None);
        assert_eq!(routed_option_side(&s, &bear), None);

        // On: the strategy's own filter side decides the leg, so a bullish
        // strategy can only ever trade CE and a bearish one only PE - regardless
        // of the other direction sources being armed.
        s.filter_side_route = true;
        assert_eq!(routed_option_side(&s, &bull), Some("CE"));
        assert_eq!(routed_option_side(&s, &bear), Some("PE"));

        // The competing overrides are suppressed while routing is on.
        s.nifty_trend_on = true;
        s.run_in_enabled = true;
        s.run_in_side = "CE".into();
        s.option_side = "CE".into();
        assert_eq!(routed_option_side(&s, &bear), Some("PE"), "bearish leg cannot be flipped to CE");

        // A generic BUY strategy counts as bullish; a SELL one as bearish.
        let generic = Strategy { category: String::new(), side: "BUY".into(), ..Default::default() };
        let generic_sell = Strategy { category: String::new(), side: "SELL".into(), ..Default::default() };
        assert_eq!(routed_option_side(&s, &generic), Some("CE"));
        assert_eq!(routed_option_side(&s, &generic_sell), Some("PE"));
    }

    #[test]
    fn filter_side_route_round_trip_uses_camel_case() {
        let mut s = Settings::default();
        assert!(!s.filter_side_route);
        s.filter_side_route = true;
        s.strict_line_color = true;
        s.once_per_signal = true;
        s.nifty_net_strict = true;
        s.nifty_all_agree_strict = true;
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["filterSideRoute"], serde_json::json!(true));
        assert_eq!(v["strictLineColor"], serde_json::json!(true));
        assert_eq!(v["oncePerSignal"], serde_json::json!(true));
        assert_eq!(v["niftyNetStrict"], serde_json::json!(true));
        assert_eq!(v["niftyAllAgreeStrict"], serde_json::json!(true));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert!(back.filter_side_route);
        assert!(back.strict_line_color);
        assert!(back.once_per_signal);
        assert!(back.nifty_net_strict);
        assert!(back.nifty_all_agree_strict);
    }
    #[test]
    fn movers_index_leg_settings_round_trip_uses_camel_case() {
        let mut s = Settings::default();
        assert!(s.movers_index_legs.is_empty());
        s.movers_index_legs.push(MoversIndexLeg { security_id: 13, side: "CE".into() });
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["moversIndexLegs"][0]["securityId"], serde_json::json!(13));
        assert_eq!(v["moversIndexLegs"][0]["side"], serde_json::json!("CE"));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(back.movers_index_legs.len(), 1);
        assert_eq!(back.movers_index_legs[0].security_id, 13);
        assert_eq!(back.movers_index_legs[0].side, "CE");
    }

    #[test]
    fn assigned_index_leg_wins_and_unassigned_is_skipped() {
        let mut s = Settings::default();
        // No assignment: the index is skipped and contributes no side.
        assert_eq!(movers_index_leg(&s, 13), None);
        assert_eq!(index_target_side(&s, 13), None);
        s.movers_indices.push(13);
        assert_eq!(index_target_side(&s, 13), None, "no leg => index is skipped");

        // Assign CE: only the CE side runs and it is authoritative.
        s.movers_index_legs.push(MoversIndexLeg { security_id: 13, side: "CE".into() });
        assert_eq!(movers_index_leg(&s, 13), Some("CE"));
        assert_eq!(index_target_side(&s, 13), Some(true));

        // The assigned leg overrides the NIFTY lock / Run-Strategy-In / Option
        // Type / filter-side routing for that index, even armed for the opposite
        // side.
        s.nifty_trend_on = true;
        s.run_in_enabled = true;
        s.run_in_side = "PE".into();
        s.option_side = "PE".into();
        s.filter_side_route = true;
        let idx = Strategy {
            exchange_segment: "IDX_I".into(),
            instrument: "INDEX".into(),
            security_id: 13,
            category: "BEARISH".into(),
            ..Default::default()
        };
        assert_eq!(routed_option_side(&s, &idx), Some("CE"), "assigned CE leg wins over every other source");

        // A non-index strategy with the same security id is unaffected.
        let stock = Strategy {
            exchange_segment: "NSE_EQ".into(),
            instrument: "EQUITY".into(),
            security_id: 13,
            category: "BEARISH".into(),
            ..Default::default()
        };
        assert_eq!(routed_option_side(&s, &stock), Some("PE"), "filter-side routing still applies to stocks");

        // Assign PE: flips to the PE leg; an invalid side value is ignored.
        s.movers_index_legs[0] = MoversIndexLeg { security_id: 13, side: "PE".into() };
        assert_eq!(index_target_side(&s, 13), Some(false));
        s.movers_index_legs[0] = MoversIndexLeg { security_id: 13, side: "nonsense".into() };
        assert_eq!(movers_index_leg(&s, 13), None, "invalid side is treated as no assignment");
    }

    #[test]
    fn commodity_leg_settings_round_trip_uses_camel_case() {
        let mut s = Settings::default();
        assert!(s.commodity_legs.is_empty());
        s.commodity_legs.push(MoversIndexLeg { security_id: 428348, side: "PE".into() });
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["commodityLegs"][0]["securityId"], serde_json::json!(428348));
        assert_eq!(v["commodityLegs"][0]["side"], serde_json::json!("PE"));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert_eq!(back.commodity_legs.len(), 1);
        assert_eq!(back.commodity_legs[0].security_id, 428348);
        assert_eq!(back.commodity_legs[0].side, "PE");
    }

    #[test]
    fn assigned_commodity_leg_is_authoritative_and_drives_the_filter_side() {
        let mut s = Settings::default();
        // No assignment: no forced side, global resolution applies.
        assert_eq!(commodity_leg(&s, 428348), None);
        assert_eq!(commodity_target_side(&s, 428348), None);

        // Assign CE: only CE runs. The side the scanner builds the synthetic on
        // (Some(true)) is what makes the BULLISH indicator filters the entry rule.
        s.commodity_legs.push(MoversIndexLeg { security_id: 428348, side: "CE".into() });
        assert_eq!(commodity_leg(&s, 428348), Some("CE"));
        assert_eq!(commodity_target_side(&s, 428348), Some(true));

        // The assigned leg wins over the NIFTY lock / run-in / option-type /
        // filter-side routing for that commodity, even armed for the opposite side.
        s.nifty_trend_on = true;
        s.run_in_enabled = true;
        s.run_in_side = "PE".into();
        s.option_side = "PE".into();
        s.filter_side_route = true;
        let comm = Strategy {
            exchange_segment: "MCX_COMM".into(),
            instrument: "FUTCOM".into(),
            security_id: 428348,
            category: "BEARISH".into(),
            ..Default::default()
        };
        assert_eq!(routed_option_side(&s, &comm), Some("CE"), "assigned CE leg wins over every other source");

        // A non-commodity strategy with the same security id is unaffected by the
        // commodity leg list.
        let stock = Strategy {
            exchange_segment: "NSE_EQ".into(),
            instrument: "EQUITY".into(),
            security_id: 428348,
            category: "BEARISH".into(),
            ..Default::default()
        };
        assert_eq!(routed_option_side(&s, &stock), Some("PE"), "filter-side routing still applies to stocks");

        // Assign PE: flips to the PE leg; an invalid side value is ignored.
        s.commodity_legs[0] = MoversIndexLeg { security_id: 428348, side: "PE".into() };
        assert_eq!(commodity_target_side(&s, 428348), Some(false));
        s.commodity_legs[0] = MoversIndexLeg { security_id: 428348, side: "nonsense".into() };
        assert_eq!(commodity_leg(&s, 428348), None, "invalid side is treated as no assignment");
    }

    const UI_BULL: &[&str] = &[
        "IncUp", "GapUp", "IncUpAll", "CrossUp", "GtUp", "LtUp", "PaneCrossUp", "PaneIncUpAll",
        "BullBbwInc", "BullSmf", "BullAsr", "BullOit", "BullBbCrossAbove", "BullPcCrossAbove",
        "BullEma9_21", "BullEma21_35", "BullEma35_50", "BullEma50_100", "BullEma100_200",
        "BullEma200_300", "BullEma1_9", "BullEma1_21", "BullEma1_35", "BullEma1_50",
        "BullEma1_100", "BullEma1_200", "BullEma1_300", "BullEmaTrend9", "BullEmaTrend21",
        "BullEmaTrend35", "BullEmaTrend50", "BullEmaTrend100", "BullEmaTrend200",
        "BullEmaTrend300", "BullSt10_1_2", "BullSt10_2_3", "BullSt1CloseCrossAbove",
        "BullVwapCloseCrossAbove", "BullPbrAo", "BullPbrSmiio", "BullPbrDpo", "BullPbrMfi",
        "BullPbrUo", "BullPbrWilliamsR", "BullPbrBbpct", "BullPbrPvt", "BullPbrAd", "BullPbrSmf",
        "BullPbrBbwUp", "BullPbrAtrUp", "BullPbrVoloscUp", "BullMeetEma9_21", "BullMeetEma21_35",
        "BullMeetEma35_50", "BullMeetEma50_100", "BullMeetEma100_200", "BullMeetEma200_300",
        "BullMeetSt10_1_2", "BullMeetSt10_2_3", "BullMeetCloseSt", "BullMeetCloseVwap",
        "BullMeetPaneCross", "BullMeetCross", "BullMeetCloseBb", "BullMeetClosePc", "BullMeetVl",
        "BullPbrCmf", "BullPbrCci", "BullPbrSqzmom", "BullPbrElderforce",
        "BullPbgMacd", "BullPbgPpo", "BullPbgSmiio", "BullPbgTsi", "BullPbgStochrsi", "BullPbgSmf",
        "BullPbgRsi", "BullPbgObv", "BullPbgFisher", "BullPbgAroon", "BullPbgVortex", "BullPbgAdx",
        "BullMeetOvlHma", "BullMeetOvlTenkan", "BullMeetOvlKijun", "BullMeetOvlKeltner",
        "BullMeetOvlDonchian", "BullMeetOvlTrendCore",
        "BullObrHma", "BullObrTenkan", "BullObrKijun", "BullObrSenkouA", "BullObrKeltner",
        "BullObrDonchian", "BullObrTrendCore",
        "BullSlElliottWave", "BullSlSupplyDemand", "BullSlPriceAction", "BullSlZigZag",
        "BullSlComboMaster", "BullSlPaneConsensus", "BullSlAutoTrendline", "BullSlPitchfork",
        "BullSlTrendProjection", "BullSlGannFan", "BullSlFibFan", "BullSlSrema", "BullVl",
        "BullSlColorElliottWave", "BullSlColorSupplyDemand", "BullSlColorPriceAction", "BullSlColorZigZag",
        "BullSlColorComboMaster", "BullSlColorPaneConsensus", "BullSlColorAutoTrendline", "BullSlColorPitchfork",
        "BullSlColorTrendProjection", "BullSlColorGannFan", "BullSlColorFibFan", "BullSlColorSrema",
        "BullSlColorVl", "BullSlColorConsensus", "BullSlColorSupport", "BullSlColorResistance",
        "BullArrowElliottWave", "BullArrowSupplyDemand", "BullArrowPriceAction", "BullArrowZigZag",
        "BullArrowComboMaster", "BullArrowPaneConsensus", "BullArrowAutoTrendline", "BullArrowPitchfork",
        "BullArrowTrendProjection", "BullArrowGannFan", "BullArrowFibFan", "BullArrowSrema",
        "BullArrowTrendCore", "BullArrowOit", "BullArrowVl",
        "BullCandle", "BullElliott", "BullIndicator", "BullPane", "BullSymmetry", "BullStructure",
        "BullAtr",
    ];
    const UI_BEAR: &[&str] = &[
        "IncDown", "GapDown", "IncDownAll", "CrossDown", "GtDown", "LtDown", "PaneCrossDown",
        "PaneIncDownAll", "BearBbwInc", "BearSmf", "BearAsr", "BearOit", "BearBbCrossBelow",
        "BearPcCrossBelow", "BearEma9_21", "BearEma21_35", "BearEma35_50", "BearEma50_100",
        "BearEma100_200", "BearEma200_300", "BearEma1_9", "BearEma1_21", "BearEma1_35",
        "BearEma1_50", "BearEma1_100", "BearEma1_200", "BearEma1_300", "BearEmaTrend9",
        "BearEmaTrend21", "BearEmaTrend35", "BearEmaTrend50", "BearEmaTrend100", "BearEmaTrend200",
        "BearEmaTrend300", "BearSt10_1_2", "BearSt10_2_3", "BearSt1CloseCrossBelow",
        "BearVwapCloseCrossBelow", "BearPbrAo", "BearPbrSmiio", "BearPbrDpo", "BearPbrMfi",
        "BearPbrUo", "BearPbrWilliamsR", "BearPbrBbpct", "BearPbrPvt", "BearPbrAd", "BearPbrSmf",
        "BearPbrBbwUp", "BearPbrAtrUp", "BearPbrVoloscUp", "BearMeetEma9_21", "BearMeetEma21_35",
        "BearMeetEma35_50", "BearMeetEma50_100", "BearMeetEma100_200", "BearMeetEma200_300",
        "BearMeetSt10_1_2", "BearMeetSt10_2_3", "BearMeetCloseSt", "BearMeetCloseVwap",
        "BearMeetPaneCross", "BearMeetCross", "BearMeetCloseBb", "BearMeetClosePc", "BearMeetVl",
        "BearPbrCmf", "BearPbrCci", "BearPbrSqzmom", "BearPbrElderforce",
        "BearPbgMacd", "BearPbgPpo", "BearPbgSmiio", "BearPbgTsi", "BearPbgStochrsi", "BearPbgSmf",
        "BearPbgRsi", "BearPbgObv", "BearPbgFisher", "BearPbgAroon", "BearPbgVortex", "BearPbgAdx",
        "BearMeetOvlHma", "BearMeetOvlTenkan", "BearMeetOvlKijun", "BearMeetOvlKeltner",
        "BearMeetOvlDonchian", "BearMeetOvlTrendCore",
        "BearObrHma", "BearObrTenkan", "BearObrKijun", "BearObrSenkouA", "BearObrKeltner",
        "BearObrDonchian", "BearObrTrendCore",
        "BearSlElliottWave", "BearSlSupplyDemand", "BearSlPriceAction", "BearSlZigZag",
        "BearSlComboMaster", "BearSlPaneConsensus", "BearSlAutoTrendline", "BearSlPitchfork",
        "BearSlTrendProjection", "BearSlGannFan", "BearSlFibFan", "BearSlSrema", "BearVl",
        "BearSlColorElliottWave", "BearSlColorSupplyDemand", "BearSlColorPriceAction", "BearSlColorZigZag",
        "BearSlColorComboMaster", "BearSlColorPaneConsensus", "BearSlColorAutoTrendline", "BearSlColorPitchfork",
        "BearSlColorTrendProjection", "BearSlColorGannFan", "BearSlColorFibFan", "BearSlColorSrema",
        "BearSlColorVl", "BearSlColorConsensus", "BearSlColorSupport", "BearSlColorResistance",
        "BearArrowElliottWave", "BearArrowSupplyDemand", "BearArrowPriceAction", "BearArrowZigZag",
        "BearArrowComboMaster", "BearArrowPaneConsensus", "BearArrowAutoTrendline", "BearArrowPitchfork",
        "BearArrowTrendProjection", "BearArrowGannFan", "BearArrowFibFan", "BearArrowSrema",
        "BearArrowTrendCore", "BearArrowOit", "BearArrowVl",
        "BearCandle", "BearElliott", "BearIndicator", "BearPane", "BearSymmetry", "BearStructure",
        "BearAtr",
    ];

    #[test]
    fn all_ui_filters_eval_without_panic() {
        let up = wave(400, 100.0, 0.2);
        let down = wave(400, 200.0, -0.2);
        for (keys, candles) in [(UI_BULL, &up), (UI_BEAR, &down)] {
            for k in keys {
                assert!(
                    filter_eval(k, candles, 0).is_some(),
                    "UI filter {k} must be implemented (return Some)"
                );
            }
        }
    }

    #[test]
    fn ultrafast_gate_derives_each_indicator_once() {
        let _g = ind_test_lock();
        // Instrumentation proof of the memo layer: on the same candle slice a
        // naive per-filter evaluation re-derives shared indicators (EMA 9/21/35,
        // BB, ST, pane lines, ...) over and over, while one `filter_gate` pass
        // derives each (indicator, settings) pair exactly once.
        let candles = wave(400, 100.0, 0.15);
        let mut s = Settings::default();
        for k in UI_BULL {
            s.filters.insert((*k).to_string(), true);
        }
        let strat = bull_strategy();

        reset_ind_compute_calls();
        let mut naive = 0usize;
        for k in UI_BULL {
            // Worst case (what the old code always paid): no cache at all.
            reset_global_ind_cache();
            reset_ind_compute_calls();
            let _ = filter_eval(k, &candles, 0);
            naive += ind_compute_calls();
        }

        reset_global_ind_cache();
        reset_ind_compute_calls();
        let _ = filter_gate(&s, &strat, &candles, 0);
        let memo = ind_compute_calls();

        eprintln!("ultrafast: naive={naive} memo={memo} filters={}", UI_BULL.len());
        assert!(memo > 0, "gate must derive indicators");
        assert!(naive >= UI_BULL.len(), "naive pass derives at least one per filter");
        assert!(
            memo < naive,
            "memo must collapse duplicate derivations: naive={naive} memo={memo}"
        );
    }

    #[test]
    fn persistent_cache_shared_across_strategies_and_ticks() {
        let _g = ind_test_lock();
        // Phase-2: the content-addressed cache is shared engine-wide, so the
        // per-strategy candle clone and the next tick's re-evaluation cost zero
        // indicator derivations until the candle contents actually change.
        let candles = wave(400, 100.0, 0.15);
        let mut s = Settings::default();
        for k in UI_BULL {
            s.filters.insert((*k).to_string(), true);
        }
        let strat = bull_strategy();

        reset_global_ind_cache();
        reset_ind_compute_calls();
        let _ = filter_gate(&s, &strat, &candles, 0);
        let first = ind_compute_calls();
        assert!(first > 0, "cold cache must derive indicators");

        // A different Vec with identical contents (what each strategy gets).
        let clone = candles.clone();
        reset_ind_compute_calls();
        let _ = filter_gate(&s, &strat, &clone, 0);
        assert_eq!(ind_compute_calls(), 0, "identical content must be free");

        // Same slice on the next tick.
        reset_ind_compute_calls();
        let _ = filter_gate(&s, &strat, &candles, 0);
        assert_eq!(ind_compute_calls(), 0, "unchanged tick must be free");

        // A changed last bar is new content and must re-derive (no stale read).
        let mut changed = candles.clone();
        let n = changed.len();
        changed[n - 1].close += 1.0;
        reset_ind_compute_calls();
        let _ = filter_gate(&s, &strat, &changed, 0);
        assert!(ind_compute_calls() > 0, "changed content must re-derive");
    }

    #[test]
    fn memoised_gate_matches_unscoped_evaluation() {
        // Correctness parity: the memo scope must not change a single result,
        // and the strict-AND gate verdict must match a plain per-filter AND.
        let candles = wave(500, 120.0, 0.1);
        for (keys, strat) in [(UI_BULL, bull_strategy()), (UI_BEAR, bear_strategy())] {
            let mut enabled = Settings::default();
            for k in keys {
                enabled.filters.insert((*k).to_string(), true);
            }
            // Fresh scope per call (recomputes), one shared scope (memo), and no
            // scope at all (raw): all three must agree filter-for-filter.
            let scoped: Vec<Option<bool>> = keys
                .iter()
                .map(|k| {
                    let _sc = IndScope::enter();
                    filter_eval(k, &candles, 0)
                })
                .collect();
            let shared: Vec<Option<bool>> = {
                let _sc = IndScope::enter();
                keys.iter().map(|k| filter_eval(k, &candles, 0)).collect()
            };
            let raw: Vec<Option<bool>> = keys.iter().map(|k| filter_eval(k, &candles, 0)).collect();
            assert_eq!(scoped, shared, "memo changed a filter result");
            assert_eq!(shared, raw, "memo diverged from an unscoped pass");
            // The strict-AND gate requires every armed side filter to pass.
            let mut st = enabled.clone();
            st.all_in_one = true;
            let gate = filter_gate(&st, &strat, &candles, 0);
            let bull_side = strategy_is_bull(&strat);
            let strict = enabled
                .filters
                .iter()
                .filter(|(k, v)| **v && !is_stream_flag(k) && (if bull_side { filter_is_bull(k) } else { filter_is_bear(k) }))
                .all(|(k, _)| filter_eval(k, &candles, 0).unwrap_or(false));
            assert_eq!(gate, strict, "gate verdict diverged from strict-AND");
        }
    }

    #[test]
    #[ignore = "diagnostic latency budget; run with --ignored"]
    fn ultrafast_gate_latency_budget() {
        let _g = ind_test_lock();
        // Latency proof of the memoised gate on the full 115-filter side.
        let candles = wave(400, 100.0, 0.15);
        let mut s = Settings::default();
        for k in UI_BULL {
            s.filters.insert((*k).to_string(), true);
        }
        let strat = bull_strategy();
        let _ = filter_gate(&s, &strat, &candles, 0);
        let n = 500u32;
        let t = std::time::Instant::now();
        for _ in 0..n {
            std::hint::black_box(filter_gate(&s, &strat, &candles, 0));
        }
        let per = t.elapsed().as_secs_f64() / n as f64;
        eprintln!(
            "gate latency: {:.3} ms/pass over {} armed filters",
            per * 1000.0,
            UI_BULL.len()
        );
        // Generous debug-build ceiling; release is several times faster.
        assert!(per < 0.5, "full-side gate pass must stay well under a second: {per}s");
    }

    #[test]
    #[ignore = "diagnostic per-filter profiling; run with --ignored"]
    fn ultrafast_profile_slowest_filters() {
        let _g = ind_test_lock();
        // Diagnostic: per-filter cost so the next optimisation round targets the
        // real offenders instead of guessing.
        let candles = wave(400, 100.0, 0.15);
        let n = 50u32;
        let mut rows: Vec<(f64, &str)> = Vec::new();
        for k in UI_BULL {
            let _ = filter_eval(k, &candles, 0);
            let t = std::time::Instant::now();
            for _ in 0..n {
                let _sc = IndScope::enter();
                std::hint::black_box(filter_eval(k, &candles, 0));
            }
            rows.push((t.elapsed().as_secs_f64() / n as f64 * 1000.0, k));
        }
        rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let total: f64 = rows.iter().map(|(t, _)| t).sum();
        eprintln!("--- slowest filters (ms/call, total {total:.2} ms) ---");
        for (ms, k) in rows.iter().take(20) {
            eprintln!("{ms:>9.3}  {k}");
        }
    }

    #[test]
    fn run_and_trade_routing() {
        let mut s = Settings::default();
        s.run_index = "both".into();
        s.run_fno = "spot".into();
        s.run_comm = "spot".into();
        s.trade_in_index = "premium".into();
        s.trade_in_comm = "futures".into();

        let idx = Strategy { exchange_segment: "IDX_I".into(), instrument: "INDEX".into(), ..Default::default() };
        assert_eq!(strat_category(&idx), StratCat::Index);
        assert_eq!(run_mode_of(&idx, &s), "both");
        assert_eq!(trade_mode_of(&idx, &s), "premium");

        let fno = Strategy { exchange_segment: "NSE_EQ".into(), instrument: "EQUITY".into(), ..Default::default() };
        assert_eq!(strat_category(&fno), StratCat::Fno);
        assert_eq!(run_mode_of(&fno, &s), "spot");
        assert_eq!(trade_mode_of(&fno, &s), "premium", "F&O always forced to premium");

        let comm = Strategy { exchange_segment: "MCX_COMM".into(), instrument: "FUTCOM".into(), ..Default::default() };
        assert_eq!(strat_category(&comm), StratCat::Comm);
        assert_eq!(trade_mode_of(&comm, &s), "premium", "commodities trade on premium too");

        // Index options classify as indices (so they follow the indices run mode).
        let idx_opt = Strategy { exchange_segment: "NSE_FNO".into(), instrument: "OPTIDX".into(), ..Default::default() };
        assert_eq!(strat_category(&idx_opt), StratCat::Index);
        let comm_opt = Strategy { exchange_segment: "MCX_COMM".into(), instrument: "OPTFUT".into(), ..Default::default() };
        assert_eq!(strat_category(&comm_opt), StratCat::Comm);
    }

    #[tokio::test]
    async fn nifty_trend_applies_to_fno_stocks_only() {
        // NIFTY Trend Following must never lock an index or a commodity
        // strategy onto its direction - only F&O stocks follow the straight
        // lines. This is the regression guard for that scoping.
        let market = crate::market::MarketState::new();
        let dhan = crate::broker::DhanState::new(market);
        let rt = RealtimeState::new_paper(dhan);
        let mut s = Settings::default();
        s.nifty_trend_on = true;

        let fno = Strategy { exchange_segment: "NSE_EQ".into(), instrument: "EQUITY".into(), ..Default::default() };
        let idx = Strategy { exchange_segment: "IDX_I".into(), instrument: "INDEX".into(), ..Default::default() };
        let comm = Strategy { exchange_segment: "MCX_COMM".into(), instrument: "FUTCOM".into(), ..Default::default() };
        let idx_opt = Strategy { exchange_segment: "NSE_FNO".into(), instrument: "OPTIDX".into(), ..Default::default() };
        let comm_opt = Strategy { exchange_segment: "MCX_COMM".into(), instrument: "OPTFUT".into(), ..Default::default() };

        // Bullish NIFTY: only the F&O stock locks to CE.
        rt.nifty_dir.store(1, Ordering::Relaxed);
        assert_eq!(rt.nifty_locked_side(&fno, &s), Some("CE"));
        assert_eq!(
            rt.auto_option_side(&s),
            None,
            "the NIFTY direction is not a global auto side, so it cannot leak onto other universes"
        );
        assert_eq!(rt.nifty_locked_side(&idx, &s), None, "indices are never locked");
        assert_eq!(rt.nifty_locked_side(&comm, &s), None, "commodities are never locked");
        assert_eq!(rt.nifty_locked_side(&idx_opt, &s), None, "index options are never locked");
        assert_eq!(rt.nifty_locked_side(&comm_opt, &s), None, "commodity options are never locked");

        // Bearish NIFTY: the F&O stock flips to PE; the rest stay untouched.
        rt.nifty_dir.store(-1, Ordering::Relaxed);
        assert_eq!(rt.nifty_locked_side(&fno, &s), Some("PE"));
        assert_eq!(rt.nifty_locked_side(&idx, &s), None);
        assert_eq!(rt.nifty_locked_side(&comm, &s), None);

        // Feature off: no lock even for an F&O stock.
        s.nifty_trend_on = false;
        assert_eq!(rt.nifty_locked_side(&fno, &s), None);
    }

    #[test]
    fn run_and_trade_defaults_match_old_app() {
        // Requested defaults: run indices=both / F&O=spot / commodities=spot,
        // trade is ALWAYS premium (enforced for indices, F&O stocks and
        // commodities alike), so the trade-in dropdown defaults reflect that.
        let s = Settings::default();
        assert!(!s.premium_only);
        assert_eq!(s.run_index, "both");
        assert_eq!(s.run_fno, "spot");
        assert_eq!(s.run_comm, "spot");
        assert_eq!(s.trade_in_index, "premium");
        assert_eq!(s.trade_in_fno, "premium");
        assert_eq!(s.trade_in_comm, "premium");
        assert!(!s.run_in_default && !s.trade_in_default);

        // And the routing helper resolves them to the same effective modes.
        let idx = Strategy { exchange_segment: "IDX_I".into(), instrument: "INDEX".into(), ..Default::default() };
        let fno = Strategy { exchange_segment: "NSE_EQ".into(), instrument: "EQUITY".into(), ..Default::default() };
        let comm = Strategy { exchange_segment: "MCX_COMM".into(), instrument: "FUTCOM".into(), ..Default::default() };
        assert_eq!(run_mode_of(&idx, &s), "both");
        assert_eq!(run_mode_of(&fno, &s), "spot");
        assert_eq!(run_mode_of(&comm, &s), "spot");
        assert_eq!(trade_mode_of(&idx, &s), "premium");
        assert_eq!(trade_mode_of(&fno, &s), "premium");
        assert_eq!(trade_mode_of(&comm, &s), "premium");

        // "Premium chart only" pins everything to premium.
        let mut p = s.clone();
        p.premium_only = true;
        assert_eq!(run_mode_of(&comm, &p), "premium");
        assert_eq!(trade_mode_of(&idx, &p), "premium");
    }

    #[test]
    fn option_strategy_maps_to_underlying_spot() {
        // "Run Strategy In: Spot" on an option-instrument strategy evaluates the
        // underlying spot chart, so the option's own trading symbol must map back
        // to the index/equity catalog entry.
        let idx_opt = Strategy {
            exchange_segment: "NSE_FNO".into(),
            instrument: "OPTIDX".into(),
            trading_symbol: "NIFTY 50".into(),
            security_id: 99999,
            ..Default::default()
        };
        let u = underlying_strategy(&idx_opt).expect("index option maps to underlying");
        assert_eq!(u.security_id, 13);
        assert_eq!(u.exchange_segment, "IDX_I");
        assert_eq!(u.instrument, "INDEX");
        assert_eq!(u.trading_symbol, "NIFTY 50");

        // A plain underlying strategy keeps its own instrument.
        let eq = Strategy {
            exchange_segment: "NSE_EQ".into(),
            instrument: "EQUITY".into(),
            trading_symbol: "RELIANCE".into(),
            ..Default::default()
        };
        assert!(underlying_strategy(&eq).is_none());
    }

    #[test]
    fn profit_trail_trails_running_profit_by_the_configured_percent() {
        // BUY: 10% of the running profit is given back. Peak profit 1000 ->
        // give back 100 -> stop sits 100 behind the peak.
        assert!((profit_trail_stop(true, 100.0, 1000.0, 10.0) - 1000.0).abs() < 1e-9);
        // A larger peak drags the stop forward proportionally.
        assert!((profit_trail_stop(true, 100.0, 2000.0, 10.0) - 1900.0).abs() < 1e-9);
        // In profit the stop is always above entry, so a reversal exits in
        // profit, never at a loss.
        assert!(profit_trail_stop(true, 100.0, 40.0, 10.0) > 100.0);
        // SELL mirrors.
        assert!((profit_trail_stop(false, 2000.0, 1000.0, 10.0) - 1100.0).abs() < 1e-9);
        assert!(profit_trail_stop(false, 2000.0, 40.0, 10.0) < 2000.0);
        // Giveback is exactly pct of the peak - even a sub-tick profit keeps the
        // stop above entry, so a small winner is never forced to a loss.
        assert!((profit_trail_stop(true, 100.0, 0.2, 10.0) - (100.0 + 0.2 - 0.02)).abs() < 1e-9);
        assert!(profit_trail_stop(true, 100.0, 0.04, 10.0) > 100.0);
        assert!(profit_trail_stop(false, 2000.0, 0.04, 10.0) < 2000.0);
    }

    #[test]
    fn point_trail_trails_a_fixed_jump_behind_running_profit() {
        // BUY entry 100, jump 5 -> peak profit 30 leaves the stop 5 behind.
        assert!((point_trail_stop(true, 100.0, 30.0, 5.0) - 125.0).abs() < 1e-9);
        // A bigger peak drags the fixed jump forward.
        assert!((point_trail_stop(true, 100.0, 80.0, 5.0) - 175.0).abs() < 1e-9);
        // Still behind the peak, never off entry.
        assert!(point_trail_stop(true, 100.0, 30.0, 5.0) > 100.0);
        // SELL mirrors.
        assert!((point_trail_stop(false, 2000.0, 30.0, 5.0) - 1975.0).abs() < 1e-9);
        assert!(point_trail_stop(false, 2000.0, 30.0, 5.0) < 2000.0);
        // A jump smaller than one tick is floored to a tick.
        assert!((point_trail_stop(true, 100.0, 30.0, 0.0) - (100.0 + 30.0 - 0.05)).abs() < 1e-9);
    }

    #[test]
    fn disabled_manual_levels_ignore_stale_percent_and_config() {
        let mut s = Settings::default();
        // Isolate the manual levels from the AI fallback.
        s.sl_auto = false;
        // A method config `tpPct` must never arm a target on its own now that
        // the manual Take Profit card is gone.
        let (_, tp, _, _, _) = levels_for(&s, &json!({ "tpPct": 5.0 }), "BUY", 100.0, 0.0, 0.0, 0.0);
        assert_eq!(tp, 0.0);
        // Unticked SL / Trail SL must not arm either.
        s.manual_sl = false;
        s.manual_sl_pct = 2.0;
        s.manual_trail_sl = false;
        s.manual_trail_sl_pct = 10.0;
        let (sl, _, trail, _, _) =
            levels_for(&s, &json!({ "slPct": 2.0, "trailPct": 10.0 }), "BUY", 100.0, 0.0, 0.0, 0.0);
        assert_eq!(sl, 0.0);
        assert_eq!(trail, 0.0);
        // Ticking them on arms exactly the operator-set / config values.
        s.manual_sl = true;
        s.manual_trail_sl = true;
        let (sl, tp, trail, _, _) =
            levels_for(&s, &json!({ "slPct": 2.0, "tpPct": 5.0, "trailPct": 10.0 }), "BUY", 100.0, 0.0, 0.0, 0.0);
        assert!((sl - 98.0).abs() < 1e-9, "sl={sl}");
        assert_eq!(tp, 0.0);
        assert!((trail - 10.0).abs() < 1e-9, "trail={trail}");
    }

    #[test]
    fn definable_filters_are_active() {        let up = ramp(320, 100.0, 0.7);
        let down = ramp(320, 320.0, -0.7);
        for k in [
            "BullEma9_21",
            "BullEmaTrend9",
            "BullSt10_1_2",
            "BullSt1CloseCrossAbove",
            "BullBbCrossAbove",
            "BullPcCrossAbove",
            "BullMeetCloseSt",
            "BullMeetCloseBb",
            "BullMeetClosePc",
            "BullPbrAo",
            "BullSmf",
            "BullMeetVl",
        ] {
            assert!(filter_eval(k, &up, 0).is_some(), "{k} should be implemented");
        }
        for k in ["BearEma9_21", "BearSt10_1_2", "BearMeetCloseSt", "BearPbrAo"] {
            assert!(filter_eval(k, &down, 0).is_some(), "{k} should be implemented");
        }
    }

    #[test]
    fn support_and_resistance_trend_filters_read_their_settings() {
        let mut s = Settings::default();
        s.sup_strength = 7.0;
        s.sup_look = 20.0;
        s.sup_full_span = true;
        let sup = support_trend_cfg(&s);
        assert_eq!(sup.strength, 7.0);
        assert_eq!(sup.look, 20.0);
        assert!(sup.full_span);
        s.res_atr_period = 21.0;
        s.res_min_pct = 0.2;
        let res = resistance_trend_cfg(&s);
        assert_eq!(res.atr_period, 21.0);
        assert_eq!(res.min_pct, 0.2);
        // The gate always resolves (non-blocking when the line is not drawn yet),
        // so an unresolved fit can never freeze live entries.
        let c = ramp(320, 100.0, 0.5);
        assert!(filter_eval_inner("BullSlSupport", &c, 0, ConsensusCfg::default(), sup, res).is_some());
        assert!(filter_eval_inner("BearSlResistance", &c, 0, ConsensusCfg::default(), sup, res).is_some());
    }

    #[test]
    fn manual_margin_amount_overrides_percentage() {
        // No manual amount: percentage of the available balance.
        assert!((margin_budget_of(0.0, 50.0, 200_000.0) - 100_000.0).abs() < 1e-9);
        assert!((margin_budget_of(0.0, 100.0, 200_000.0) - 200_000.0).abs() < 1e-9);
        // Manual amount wins, capped by the live balance.
        assert!((margin_budget_of(40_000.0, 100.0, 200_000.0) - 40_000.0).abs() < 1e-9);
        assert!((margin_budget_of(500_000.0, 100.0, 200_000.0) - 200_000.0).abs() < 1e-9);
        // Zero percentage with no amount stays disabled.
        assert_eq!(margin_budget_of(0.0, 0.0, 200_000.0), 0.0);
    }

    #[test]
    fn margin_block_reason_blocks_only_when_required_exceeds_free() {
        // 500k budget, 100k locked by one running trade => 400k free.
        assert!(margin_block_reason(300_000.0, 500_000.0, 100_000.0, 1).is_none());
        // Exactly at the free amount is allowed (not strictly greater).
        assert!(margin_block_reason(400_000.0, 500_000.0, 100_000.0, 1).is_none());
        // A rupee over the free amount is blocked, and the reason names the lock.
        let reason = margin_block_reason(400_001.0, 500_000.0, 100_000.0, 1)
            .expect("over-budget entry must be blocked");
        assert!(reason.contains("trade blocked"));
        assert!(reason.contains("locked 100000"));
        // Fully locked budget blocks any positive requirement.
        assert!(margin_block_reason(1.0, 500_000.0, 500_000.0, 3).is_some());
        // A zero requirement never blocks (the "no budget configured" guard
        // lives in `margin_block_for_qty`, not in this pure reason helper).
        assert!(margin_block_reason(0.0, 0.0, 0.0, 0).is_none());
    }

    #[test]
    fn paper_margin_budget_counts_locked_margin_once() {
        // 500k wallet, 100k of open notional: the budget must stay the full 500k
        // and "available" must be 500k - 100k = 400k. The old code sized the
        // budget off the post-lock wallet and then subtracted the lock again, so
        // budgets shrank with every open trade and "available" hit 0 early.
        let cap = 500_000.0;
        let closed: Vec<Value> = Vec::new();
        let positions = vec![json!({ "qty": 1000.0, "entry": 100.0 })];

        let wallet = paper_wallet_of(cap, &closed, false);
        assert!((wallet - 500_000.0).abs() < 1e-9);
        // Free cash still subtracts the lock exactly once.
        let free = paper_available_of(cap, &closed, &positions, false);
        assert!((free - 400_000.0).abs() < 1e-9);

        let budget = margin_budget_of(0.0, 100.0, wallet);
        let (locked, count) = locked_margin_of(&positions);
        assert!((budget - 500_000.0).abs() < 1e-9);
        assert!((locked - 100_000.0).abs() < 1e-9);
        assert!((budget - locked - 400_000.0).abs() < 1e-9);
        assert_eq!(count, 1);
    }

    // --- Engine controls: timeframe / MTF / auto-lots wiring -----------------

    #[test]
    fn engine_tf_follows_own_unless_ast_settings() {
        let mut s = Settings::default();
        s.tf_1min = true;
        s.tf_5min = true;
        let strat = Strategy { timeframe: "1min".into(), ..Default::default() };
        // Own TF wins while "Use AST settings" is off.
        assert_eq!(engine_tf(&s, &strat), "1min");
        // ON forces the first ticked engine timeframe regardless of the strategy.
        s.use_own_settings = true;
        assert_eq!(engine_tf(&s, &strat), "1min");
        s.tf_1min = false;
        assert_eq!(engine_tf(&s, &strat), "5min");
    }

    #[test]
    fn engine_tf_defaults_to_5min_when_none_ticked() {        let mut s = Settings::default();
        s.tf_1min = false;
        s.tf_5min = false;
        let strat = Strategy { timeframe: "15min".into(), ..Default::default() };
        assert_eq!(engine_tf(&s, &strat), "5min");
    }

    #[test]
    fn mtf_pair_needs_two_timeframes() {
        let mut s = Settings::default();
        s.tf_1min = true;
        s.tf_5min = true;
        assert_eq!(mtf_pair(&s), Some(("1min".into(), "5min".into())));
        s.tf_5min = false;
        assert_eq!(mtf_pair(&s), None);
    }

    #[test]
    fn calc_auto_lots_uses_liquidity_and_margin_caps() {
        // Defaults 1%/1% reproduce the old app: 1% of min(oi, volume) / lot.
        assert_eq!(calc_auto_lots(50.0, 100.0, 1_000_000.0, 1_000_000.0, 0.0, 1.0, 1.0), Some(200.0));
        // Margin: 95% of available / (lot * price).
        assert_eq!(calc_auto_lots(50.0, 100.0, 0.0, 0.0, 1_000_000.0, 1.0, 1.0), Some(190.0));
        // Both: the tighter cap wins.
        assert_eq!(calc_auto_lots(50.0, 100.0, 1_000_000.0, 1_000_000.0, 1_000_000.0, 1.0, 1.0), Some(190.0));
        // Nothing usable -> keep manual lots.
        assert_eq!(calc_auto_lots(50.0, 100.0, 0.0, 0.0, 0.0, 1.0, 1.0), None);
    }

    #[test]
    fn calc_auto_lots_picks_the_smaller_of_volume_and_oi_percent() {
        // Volume 2% of 50,000 = 1,000 units / lot 50 = 20 lots.
        // OI 2% of 25,000 = 500 units / lot 50 = 10 lots  -> OI cap wins.
        assert_eq!(calc_auto_lots(50.0, 100.0, 25_000.0, 50_000.0, 0.0, 2.0, 2.0), Some(10.0));
        // Flip the volumes: now volume is the tighter cap.
        assert_eq!(calc_auto_lots(50.0, 100.0, 50_000.0, 25_000.0, 0.0, 2.0, 2.0), Some(10.0));
        // Different percents are applied independently: OI 1% of 100,000 = 20 lots,
        // volume 3% of 100,000 = 60 lots -> 20 lots.
        assert_eq!(calc_auto_lots(50.0, 100.0, 100_000.0, 100_000.0, 0.0, 1.0, 3.0), Some(20.0));
        // A zero OI percent drops that cap and volume alone decides.
        assert_eq!(calc_auto_lots(50.0, 100.0, 25_000.0, 50_000.0, 0.0, 0.0, 2.0), Some(20.0));
        // Missing feed data falls back to the other cap (never blocks).
        assert_eq!(calc_auto_lots(50.0, 100.0, 0.0, 50_000.0, 0.0, 2.0, 2.0), Some(20.0));
        assert_eq!(calc_auto_lots(50.0, 100.0, 25_000.0, 0.0, 0.0, 2.0, 2.0), Some(10.0));
        // Margin still applies on top of the liquidity winner.
        assert_eq!(calc_auto_lots(50.0, 100.0, 25_000.0, 50_000.0, 50_000.0, 2.0, 2.0), Some(9.0));
    }

    #[test]
    fn sl_auto_drives_atr_floor() {
        let mut s = Settings::default();
        s.sl_auto = true;
        // ATR stop is honoured only when the auto flag is on.
        let (sl, _, _, _, _) = levels_for(&s, &json!({}), "BUY", 100.0, 95.0, 110.0, 3.0);
        assert!((sl - 95.0).abs() < 1e-9);
        s.sl_auto = false;
        let (sl2, _, _, _, _) = levels_for(&s, &json!({}), "BUY", 100.0, 95.0, 110.0, 3.0);
        assert_eq!(sl2, 0.0);
    }

    #[test]
    fn rr_target_overrides_ai_tp() {
        let mut s = Settings::default();
        s.ai_tp_pct = true;
        // Without RR the AI TP wins (the passed-in AI level).
        let (_, tp, _, _, _) = levels_for(&s, &json!({}), "BUY", 100.0, 95.0, 112.0, 3.0);
        assert!((tp - 112.0).abs() < 1e-9);
        // With RR on: target = |entry - SL| x RR, beating the AI TP%.
        s.rr_enabled = true;
        s.rr_value = 2.0;
        let (_, tp, _, _, _) = levels_for(&s, &json!({}), "BUY", 100.0, 95.0, 112.0, 3.0);
        assert!((tp - 110.0).abs() < 1e-9);
        // SELL mirrors to the downside.
        let (_, tp, _, _, _) = levels_for(&s, &json!({}), "SELL", 100.0, 105.0, 90.0, 3.0);
        assert!((tp - 90.0).abs() < 1e-9);
        // RR on but no usable SL distance -> TP stays 0 (nothing to size against).
        let (_, tp, _, _, _) = levels_for(&s, &json!({}), "BUY", 100.0, 0.0, 110.0, 3.0);
        assert_eq!(tp, 0.0);
    }

    #[test]
    fn trades_per_strategy_gate_matches_old_engine() {
        // Max trades off -> unlimited regardless of the count.
        assert!(trade_limit_allows(false, false, 2, 99));
        // "AI auto trades" on -> the fixed cap is ignored (unlimited).
        assert!(trade_limit_allows(true, true, 2, 99));
        // Max trades on with a positive count -> block once the cap is reached.
        assert!(trade_limit_allows(true, false, 2, 1));
        assert!(!trade_limit_allows(true, false, 2, 2));
        assert!(!trade_limit_allows(true, false, 2, 5));
        // Unconfigured count (0 / negative) -> never blocks.
        assert!(trade_limit_allows(true, false, 0, 10));
        assert!(trade_limit_allows(true, false, -3, 10));
    }

    #[test]
    fn trade_limit_default_is_five_trades() {
        let s = Settings::default();
        assert!(!s.trade_limit);
        assert_eq!(s.trade_limit_count, 5);
        assert!(!s.ai_trades);
    }

    #[test]
    fn order_per_sec_budget_clamps_to_dhan_ceiling() {
        assert_eq!(order_per_sec_budget(0), 1);
        assert_eq!(order_per_sec_budget(1), 1);
        assert_eq!(order_per_sec_budget(6), 6);
        assert_eq!(order_per_sec_budget(30), 30);
        assert_eq!(order_per_sec_budget(500), 30);
    }

    #[test]
    fn scan_interval_budget_normalises_floor_and_ceiling() {
        // A stray 0/negative falls back to the normal 100ms tick.
        assert_eq!(scan_interval_budget(0), 100);
        assert_eq!(scan_interval_budget(-5), 100);
        assert_eq!(scan_interval_budget(49), 100);
        // Valid sub-minute and minute values are preserved exactly.
        assert_eq!(scan_interval_budget(50), 50);
        assert_eq!(scan_interval_budget(100), 100);
        assert_eq!(scan_interval_budget(250), 250);
        assert_eq!(scan_interval_budget(1000), 1000);
        assert_eq!(scan_interval_budget(60_000), 60_000);
        // Above one day is clamped.
        assert_eq!(scan_interval_budget(86_400_001), SCAN_INTERVAL_MAX_MS);
    }

    #[test]
    fn engine_scan_settings_round_trip_uses_camel_case() {
        let mut s = Settings::default();
        assert!(!s.scan_interval_on);
        assert_eq!(s.scan_interval_ms, 100);
        assert!(!s.scan_bar_close);
        assert_eq!(s.scan_bar_close_tf, "1min");
        s.scan_interval_on = true;
        s.scan_interval_ms = 60_000;
        s.scan_bar_close = true;
        s.scan_bar_close_tf = "5min".into();
        let v = serde_json::to_value(&s).unwrap();
        // The UI reads/writes these exact camelCase keys.
        assert_eq!(v["scanIntervalOn"], serde_json::json!(true));
        assert_eq!(v["scanIntervalMs"], serde_json::json!(60_000));
        assert_eq!(v["scanBarClose"], serde_json::json!(true));
        assert_eq!(v["scanBarCloseTf"], serde_json::json!("5min"));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert!(back.scan_interval_on);
        assert_eq!(back.scan_interval_ms, 60_000);
        assert!(back.scan_bar_close);
        assert_eq!(back.scan_bar_close_tf, "5min");
    }

    #[test]
    fn scan_bar_close_timeframe_maps_to_the_right_step() {
        assert_eq!(scan_bar_close_step_ms("1min"), 60_000);
        assert_eq!(scan_bar_close_step_ms("5min"), 300_000);
        // Anything unrecognised falls back to 1min.
        assert_eq!(scan_bar_close_step_ms("15min"), 60_000);
        assert_eq!(scan_bar_close_step_ms(""), 60_000);
        assert_eq!(scan_bar_close_tf("5m"), "5min");
        assert_eq!(scan_bar_close_tf("bogus"), "1min");
    }

    #[test]
    fn engine_order_rate_and_time_defaults_match_old_app() {
        let s = Settings::default();
        assert_eq!(s.order_per_sec, 6);
        assert_eq!(s.start_after, "09:15");
        assert_eq!(s.no_trade_after, "15:30");
        assert_eq!(s.auto_square_off_time, "15:20");
        assert!(!s.start_after_enabled && !s.no_trade_after_enabled && !s.auto_square_off_enabled);
    }

    #[test]
    fn strike_defaults_match_old_app() {
        let s = Settings::default();
        assert_eq!(s.strike_mode, "both_atm");
        assert_eq!(s.strike_count, 3);
        assert_eq!(s.option_side, "both");
        // "Only +green premium strikes" is the default selection filter.
        assert!(s.only_positive);
        // Opposite-side fallback is ON by default: the "Execute Trade In" dropdown
        // runs on the +green base.
        assert!(s.fallback_opposite_side);
        // "Pick fastest positive rising LTP" is opt-in, off by default; it shares
        // the same direction/fallback wiring when switched on.
        assert!(!s.fastest_rising);
        assert_eq!(s.fastest_count, 3);
        // Manual Strike Select (testing) is off until the operator turns it on.
        assert!(!s.manual_strikes_enabled);
        assert!(s.manual_strikes.is_empty());
    }

    #[test]
    fn opposite_side_mode_only_for_single_sided_modes() {
        assert_eq!(RealtimeState::opposite_side_mode("above"), Some("below"));
        assert_eq!(RealtimeState::opposite_side_mode("below"), Some("above"));
        assert_eq!(RealtimeState::opposite_side_mode("above_atm"), Some("below_atm"));
        assert_eq!(RealtimeState::opposite_side_mode("below_atm"), Some("above_atm"));
        // Case-insensitive.
        assert_eq!(RealtimeState::opposite_side_mode("ABOVE"), Some("below"));
        // Both-sided / ATM-only modes have no opposite side to fall back to.
        assert_eq!(RealtimeState::opposite_side_mode("both_atm"), None);
        assert_eq!(RealtimeState::opposite_side_mode("both_atm_inc"), None);
        assert_eq!(RealtimeState::opposite_side_mode("atm"), None);
        assert_eq!(RealtimeState::opposite_side_mode("nonsense"), None);
    }

    #[test]
    fn green_walk_follows_execute_trade_in_dropdown_to_chain_edge() {
        // Chain of 11 strikes (indices 0..=10), ATM at index 5. The walk is NOT
        // capped by "Number of Strikes": it runs to the chain edge in the chosen
        // direction so minus strikes can be skipped over one by one.
        let len = 11usize;
        let idx = 5usize;
        let w = |mode: &str| RealtimeState::green_walk_order(len, idx, mode);
        // Strictly above / below: never the ATM leg.
        assert_eq!(w("above"), vec![6, 7, 8, 9, 10]);
        assert_eq!(w("below"), vec![4, 3, 2, 1, 0]);
        // Including ATM: ATM first, then outward in the chosen direction.
        assert_eq!(w("above_atm"), vec![5, 6, 7, 8, 9, 10]);
        assert_eq!(w("below_atm"), vec![5, 4, 3, 2, 1, 0]);
        // Both sides: alternating outward (ATM excluded), or ATM first when
        // "including ATM".
        assert_eq!(w("both_atm"), vec![6, 4, 7, 3, 8, 2, 9, 1, 10, 0]);
        assert_eq!(w("both_atm_inc"), vec![5, 6, 4, 7, 3, 8, 2, 9, 1, 10, 0]);
        // Case-insensitive; only-ATM / unknown collapses to the ATM leg.
        assert_eq!(w("ATM"), vec![5]);
        assert_eq!(w("anything-else"), vec![5]);
        // No index escapes the chain at either edge.
        assert_eq!(w("above")[0], 6);
        assert_eq!(RealtimeState::green_walk_order(len, 0, "below"), Vec::<usize>::new());
        assert_eq!(RealtimeState::green_walk_order(len, 10, "above"), Vec::<usize>::new());
        assert_eq!(RealtimeState::green_walk_order(len, 0, "above"), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(RealtimeState::green_walk_order(len, 10, "below"), vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0]);
    }

    #[test]
    fn premium_signs_require_plus_on_both_ltp_and_change() {
        // Rising print + positive change -> plus on both (green).
        assert_eq!(RealtimeState::premium_signs_from(10.0, 9.5, 1.0), Some((true, true)));
        // Falling print even though still up on the day -> minus LTP -> rejected.
        assert_eq!(RealtimeState::premium_signs_from(10.0, 10.5, 1.0), Some((false, true)));
        // Positive LTP tick but a minus change -> rejected.
        assert_eq!(RealtimeState::premium_signs_from(10.0, 9.5, -0.5), Some((true, false)));
        // No prior print yet: fall back to the change's sign.
        assert_eq!(RealtimeState::premium_signs_from(10.0, 0.0, 0.5), Some((true, true)));
        assert_eq!(RealtimeState::premium_signs_from(10.0, 0.0, -0.5), Some((false, false)));
        // A flat print is not a plus.
        assert_eq!(RealtimeState::premium_signs_from(10.0, 10.0, 1.0), Some((false, true)));
        // Not-yet-seeded / flat change leans on the live print (no REST needed).
        assert_eq!(RealtimeState::premium_signs_from(10.0, 9.5, 0.0), Some((true, true)));
        assert_eq!(RealtimeState::premium_signs_from(10.0, 10.5, 0.0), Some((false, false)));
        // No usable live LTP -> unknown.
        assert_eq!(RealtimeState::premium_signs_from(0.0, 9.0, 1.0), None);
    }

    #[test]
    fn fastest_rising_scores_by_change_pct_then_change() {
        // Higher LTP change % wins regardless of order.
        assert!(RealtimeState::rising_score_better((5.0, 1.0), (2.0, 9.0)));
        assert!(!RealtimeState::rising_score_better((2.0, 9.0), (5.0, 1.0)));
        // Equal change %: the bigger absolute LTP change wins.
        assert!(RealtimeState::rising_score_better((3.0, 4.0), (3.0, 2.0)));
        assert!(!RealtimeState::rising_score_better((3.0, 2.0), (3.0, 4.0)));
        // Identical scores never "replace" the incumbent.
        assert!(!RealtimeState::rising_score_better((3.0, 2.0), (3.0, 2.0)));
        // A negative change % can never outrank a positive one.
        assert!(!RealtimeState::rising_score_better((-1.0, 50.0), (0.5, 0.1)));
    }

    #[test]
    fn manual_strike_settings_round_trip_uses_camel_case() {
        let mut s = Settings::default();
        s.manual_strikes_enabled = true;
        s.manual_strikes.push(ManualStrike {
            symbol_name: "NIFTY 50".into(),
            security_id: 13,
            exchange_segment: "IDX_I".into(),
            expiry: "2026-09-25".into(),
            strike: 24500.0,
            option_type: "CE".into(),
            lot: 75.0,
        });
        let v = serde_json::to_value(&s).unwrap();
        // The UI reads/writes these exact camelCase keys.
        assert_eq!(v["manualStrikesEnabled"], serde_json::json!(true));
        assert_eq!(v["manualStrikes"][0]["symbolName"], serde_json::json!("NIFTY 50"));
        assert_eq!(v["manualStrikes"][0]["optionType"], serde_json::json!("CE"));
        assert_eq!(v["manualStrikes"][0]["strike"], serde_json::json!(24500.0));
        let back: Settings = serde_json::from_value(v).unwrap();
        assert!(back.manual_strikes_enabled);
        assert_eq!(back.manual_strikes.len(), 1);
        assert_eq!(back.manual_strikes[0].symbol_name, "NIFTY 50");
    }

    #[test]
    fn manual_target_specs_group_sides_per_underlying() {
        // In Manual Strike Select the scanner universe is rebuilt from the picks:
        // one synthetic side per option type the operator actually chose, and a
        // single entry per underlying even when CE + PE (or duplicates) are added.
        let mk = |sid: i64, ot: &str| ManualStrike {
            security_id: sid,
            option_type: ot.to_string(),
            ..Default::default()
        };
        let specs = manual_target_specs(&[
            mk(13, "CE"),
            mk(13, "PE"),
            mk(13, "CE"), // duplicate CE collapses
            mk(2885, "CE"),
            mk(-1, "PE"), // invalid id ignored
            mk(9999, "xx"), // unknown option type ignored
        ]);
        assert_eq!(specs, vec![(13, true, true), (2885, true, false)]);
    }

    #[test]
    fn index_legs_use_idx_i_quote_key() {
        let legs = index_legs(&[13, 0, -5, 25]);
        assert_eq!(legs, vec![(13, "IDX_I".to_string()), (25, "IDX_I".to_string())]);
        // The key the feed cache stores indices under must match.
        assert_eq!(crate::market::quote_key(legs[0].0, &legs[0].1), "IDX_I:13");
        // The old (buggy) NSE_EQ segment produced a plain-id key that never
        // matched an index quote, silently dropping the user's index picks.
        assert_eq!(crate::market::quote_key(13, "NSE_EQ"), "13");
        assert_ne!(crate::market::quote_key(13, "NSE_EQ"), crate::market::quote_key(13, "IDX_I"));
    }

    #[test]
    fn trade_time_gate_matches_old_engine() {
        // 09:15 start, 15:30 cutoff (both disabled -> always open).
        assert!(time_gate_ok(false, "09:15", false, "15:30", 0));
        assert!(time_gate_ok(false, "09:15", false, "15:30", 23 * 60));
        // Start gate: before 09:15 blocked, at/after allowed.
        assert!(!time_gate_ok(true, "09:15", false, "15:30", 9 * 60 + 14));
        assert!(time_gate_ok(true, "09:15", false, "15:30", 9 * 60 + 15));
        // Cutoff gate: after 15:30 blocked, at 15:30 allowed.
        assert!(time_gate_ok(false, "09:15", true, "15:30", 15 * 60 + 30));
        assert!(!time_gate_ok(false, "09:15", true, "15:30", 15 * 60 + 31));
        // Malformed time never freezes the engine.
        assert!(time_gate_ok(true, "oops", true, "", 12 * 60));
    }

    #[test]
    fn trade_sessions_gate_windows() {
        let ses = |a: &str, b: &str, on: bool| TradeSession { start: a.into(), end: b.into(), enabled: on };
        // Empty list = no extra restriction, any minute allowed.
        assert!(sessions_gate_ok(&[], 9 * 60));
        assert!(sessions_gate_ok(&[], 23 * 60));
        // One window 09:30-10:30, inclusive both ends.
        let one = [ses("09:30", "10:30", true)];
        assert!(!sessions_gate_ok(&one, 9 * 60 + 29));
        assert!(sessions_gate_ok(&one, 9 * 60 + 30));
        assert!(sessions_gate_ok(&one, 10 * 60 + 30));
        assert!(!sessions_gate_ok(&one, 10 * 60 + 31));
        // Two windows: 09:30-10:30 and 14:00-15:14 (the operator's ask).
        let two = [ses("09:30", "10:30", true), ses("14:00", "15:14", true)];
        assert!(sessions_gate_ok(&two, 9 * 60 + 45));
        assert!(!sessions_gate_ok(&two, 13 * 60));
        assert!(sessions_gate_ok(&two, 14 * 60));
        assert!(sessions_gate_ok(&two, 15 * 60 + 14));
        assert!(!sessions_gate_ok(&two, 15 * 60 + 15));
        // Disabled sessions are ignored; all-disabled = no restriction.
        let off = [ses("09:30", "10:30", false)];
        assert!(sessions_gate_ok(&off, 20 * 60));
        // Malformed / reversed sessions are ignored; never freeze the engine.
        let bad = [ses("oops", "10:30", true), ses("15:00", "09:00", true)];
        assert!(sessions_gate_ok(&bad, 12 * 60));
        // `sessions_active` drives "sessions replace the legacy gate" - only a
        // real enabled window counts.
        assert!(!sessions_active(&[]));
        assert!(sessions_active(&one));
        assert!(sessions_active(&two));
        assert!(!sessions_active(&off));
        assert!(!sessions_active(&bad));
    }

    fn pos(sid: i64, side: &str, entry: f64, ltp: f64, sl: f64, tp: f64, trail: f64, qty: i64) -> Value {
        json!({
            "id": "p", "securityId": sid, "exchangeSegment": "NSE_FNO", "instrument": "OPTIDX",
            "tradingSymbol": "NIFTY CE", "side": side, "qty": qty, "fillPrice": entry,
            "entry": entry, "ltp": ltp, "sl": sl, "overallSl": sl, "tp": tp, "trail": trail,
        })
    }

    fn title_of(lines: &[Value], price: f64) -> String {
        lines
            .iter()
            .find(|l| (l["price"].as_f64().unwrap_or(0.0) - price).abs() < 0.01)
            .map(|l| l["title"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    #[test]
    fn chart_lines_build_entry_pnl_sl_target_for_buy() {
        let positions = vec![pos(999, "BUY", 100.0, 110.0, 80.0, 150.0, 0.0, 50)];
        let mut ltp = HashMap::new();
        ltp.insert(999i64, 110.0f64);
        let (lines, total, count) = build_chart_lines(true, 999, "NSE_FNO", &positions, &ltp);
        assert_eq!(count, 1);
        assert_eq!(total, 500.0); // (110-100) * 50
        assert_eq!(lines.len(), 4); // entry + live + SL + target
        assert_eq!(title_of(&lines, 100.0), "PAPER ENTRY BUY @ 100.00");
        assert!(title_of(&lines, 110.0).contains("+₹"), "pnl title: {}", title_of(&lines, 110.0));
        assert_eq!(title_of(&lines, 80.0), "PAPER SL · 80.00");
        assert_eq!(title_of(&lines, 150.0), "PAPER TARGET · 150.00");
    }

    #[test]
    fn chart_lines_sell_pnl_inverts_and_trail_sl_title() {
        let positions = vec![pos(999, "SELL", 200.0, 180.0, 220.0, 0.0, 190.0, 10)];
        let mut ltp = HashMap::new();
        ltp.insert(999i64, 180.0f64);
        let (lines, total, count) = build_chart_lines(false, 999, "NSE_FNO", &positions, &ltp);
        assert_eq!(count, 1);
        assert_eq!(total, 200.0); // (200-180) * 10
        assert_eq!(title_of(&lines, 200.0), "RT ENTRY SELL @ 200.00");
        assert!(title_of(&lines, 180.0).contains("+₹"), "pnl title: {}", title_of(&lines, 180.0));
        // A trailing short keeps its stop above the mark, titled as a TRAIL SL.
        assert_eq!(title_of(&lines, 220.0), "RT TRAIL SL · 220.00");
    }

    #[test]
    fn chart_lines_draws_separate_overall_sl_floor_after_ratchet() {
        // The live stop has ratcheted to 105 while the entry floor is still 90:
        // both must be drawn, each with its own title.
        let mut p = pos(999, "BUY", 100.0, 112.0, 105.0, 0.0, 10.0, 50);
        p["overallSl"] = json!(90.0);
        let positions = vec![p];
        let mut ltp = HashMap::new();
        ltp.insert(999i64, 112.0f64);
        let (lines, _, count) = build_chart_lines(false, 999, "NSE_FNO", &positions, &ltp);
        assert_eq!(count, 1);
        assert_eq!(title_of(&lines, 105.0), "RT TRAIL SL · 105.00");
        assert_eq!(title_of(&lines, 90.0), "RT OVERALL SL · 90.00");
        // Once the floor collapses onto the live stop, only one line remains.
        let mut p2 = pos(999, "BUY", 100.0, 112.0, 90.0, 0.0, 0.0, 50);
        p2["overallSl"] = json!(90.0);
        let (lines2, _, _) = build_chart_lines(false, 999, "NSE_FNO", &vec![p2], &ltp);
        assert!(lines2.iter().all(|l| !l["title"].as_str().unwrap_or("").contains("OVERALL")));
    }

    #[test]
    fn chart_lines_filters_other_symbols_and_segments() {
        let positions = vec![
            pos(999, "BUY", 100.0, 110.0, 80.0, 150.0, 0.0, 50),
            pos(1000, "BUY", 100.0, 110.0, 80.0, 150.0, 0.0, 50),
        ];
        let ltp = HashMap::new();
        let (lines, _, count) = build_chart_lines(false, 999, "NSE_FNO", &positions, &ltp);
        assert_eq!(count, 1);
        assert!(lines.iter().all(|l| l["title"].as_str().unwrap_or("").starts_with("RT")));
        // Wrong segment -> nothing.
        let (_, _, count) = build_chart_lines(false, 999, "BSE_FNO", &positions, &ltp);
        assert_eq!(count, 0);
        // Empty sid -> nothing.
        let (_, _, count) = build_chart_lines(false, 0, "", &positions, &ltp);
        assert_eq!(count, 0);
    }

    #[test]
    fn chart_lines_boxes_each_trail_mechanism_separately() {
        // BUY at 100, peak running profit 30: point trail (5 pts) -> 125,
        // percent trail (10%) -> 127 owns the active stop. Each other level must
        // still get its own box: point SL 125, overall SL 80, target 150 and a
        // trail-TP (20% giveback of peak) at 124.
        let mut p = pos(999, "BUY", 100.0, 130.0, 127.0, 150.0, 10.0, 50);
        p["overallSl"] = json!(80.0);
        p["peakProfit"] = json!(30.0);
        p["pointTrail"] = json!(5.0);
        p["trailTp"] = json!(20.0);
        let mut ltp = HashMap::new();
        ltp.insert(999i64, 130.0f64);
        let (lines, _, count) = build_chart_lines(false, 999, "NSE_FNO", &vec![p], &ltp);
        assert_eq!(count, 1);
        assert_eq!(title_of(&lines, 100.0), "RT ENTRY BUY @ 100.00");
        assert!(title_of(&lines, 130.0).contains("+₹"), "pnl title: {}", title_of(&lines, 130.0));
        assert_eq!(title_of(&lines, 127.0), "RT TRAIL SL · 127.00");
        assert_eq!(title_of(&lines, 125.0), "RT POINT SL · 125.00");
        assert_eq!(title_of(&lines, 80.0), "RT OVERALL SL · 80.00");
        assert_eq!(title_of(&lines, 150.0), "RT TARGET · 150.00");
        assert_eq!(title_of(&lines, 124.0), "RT TRAIL TP · 124.00");
        assert_eq!(lines.len(), 7); // entry + live + trail + point + overall + target + trail-tp
    }

    #[test]
    fn chart_lines_point_trail_active_box_not_duplicated() {
        // SELL at 200, peak 40, 5-point trail -> stop 165 which is the active
        // stop, so it is titled POINT SL and drawn once (no duplicate box).
        let mut p = pos(999, "SELL", 200.0, 160.0, 165.0, 0.0, 0.0, 10);
        p["overallSl"] = json!(220.0);
        p["peakProfit"] = json!(40.0);
        p["pointTrail"] = json!(5.0);
        let mut ltp = HashMap::new();
        ltp.insert(999i64, 160.0f64);
        let (lines, _, _) = build_chart_lines(false, 999, "NSE_FNO", &vec![p], &ltp);
        assert_eq!(title_of(&lines, 165.0), "RT POINT SL · 165.00");
        assert_eq!(title_of(&lines, 220.0), "RT OVERALL SL · 220.00");
        let point_boxes = lines
            .iter()
            .filter(|l| l["title"].as_str().unwrap_or("").contains("POINT SL"))
            .count();
        assert_eq!(point_boxes, 1);
    }

    #[test]
    fn smart_stats_aggregates_full_closed_ledger() {
        let closed = vec![
            json!({ "pnl": 100.0, "netPnl": 95.0, "charges": 5.0 }),
            json!({ "pnl": -50.0, "netPnl": -60.0, "charges": 10.0 }),
            json!({ "pnl": 0.0, "netPnl": 0.0, "charges": 2.0 }),
        ];
        let positions = vec![json!({ "pnl": 500.0 }), json!({ "pnl": -120.0 })];
        // Charges ON: realized is net-of-charges, live = realized + gross unrealized.
        let v = smart_stats(&closed, &positions, true);
        assert_eq!(v["realized"], json!(35.0)); // 95 - 60 + 0
        assert_eq!(v["unrealized"], json!(380.0)); // 500 - 120
        assert_eq!(v["net"], json!(415.0));
        assert_eq!(v["wins"], json!(1));
        assert_eq!(v["losses"], json!(1)); // breakeven is neither W nor L
        assert_eq!(v["total"], json!(3));
        assert_eq!(v["running"], json!(2));
        assert_eq!(v["charges"], json!(17.0));
        assert_eq!(v["chargesOn"], json!(true));
        assert_eq!(v["winRate"], json!(33.33));
        // Charges OFF: realized falls back to gross `pnl`; charges are still summed.
        let g = smart_stats(&closed, &positions, false);
        assert_eq!(g["realized"], json!(50.0)); // 100 - 50 + 0
        assert_eq!(g["net"], json!(430.0));
        assert_eq!(g["wins"], json!(1));
        assert_eq!(g["chargesOn"], json!(false));
        // Empty book: everything zero, never a divide-by-zero.
        let e = smart_stats(&[], &[], true);
        assert_eq!(e["total"], json!(0));
        assert_eq!(e["winRate"], json!(0.0));
        assert_eq!(e["net"], json!(0.0));
    }

    #[test]
    fn inr_group_formats_indian_thousands() {
        assert_eq!(inr_group(500.4), "500");
        assert_eq!(inr_group(1000.0), "1,000");
        assert_eq!(inr_group(123456.0), "1,23,456");
        assert_eq!(inr_group(1234567.0), "12,34,567");
    }

    #[test]
    fn spot_candle_color_gate_needs_green_for_gainer_red_for_loser() {
        // Top Gainer (bullish): only a green forming candle may enter.
        assert!(spot_candle_color_ok(true, 100.0, 101.0), "bull + green");
        assert!(!spot_candle_color_ok(true, 100.0, 99.0), "bull + red must skip");
        assert!(!spot_candle_color_ok(true, 100.0, 100.0), "bull + doji must skip");
        // Top Loser (bearish): only a red forming candle may enter.
        assert!(spot_candle_color_ok(false, 100.0, 99.0), "bear + red");
        assert!(!spot_candle_color_ok(false, 100.0, 101.0), "bear + green must skip");
        assert!(!spot_candle_color_ok(false, 100.0, 100.0), "bear + doji must skip");
    }
}


#[cfg(test)]
mod account_tests {
    use super::*;

    #[test]
    fn account_payload_matches_old_app_shape() {
        let funds = json!({
            "availabelBalance": 150000.0,
            "utilizedAmount": 50000.0,
            "collateralAmount": 10000.0,
            "sodLimit": 200000.0,
        });
        let positions = vec![
            json!({
                "securityId": "1001", "netQty": 75, "buyAvg": 100.0,
                "positionType": "LONG", "exchangeSegment": "NSE_FNO",
                "productType": "INTRADAY", "tradingSymbol": "NIFTY 24000 CE",
            }),
            json!({
                "securityId": "1002", "netQty": 50, "buyAvg": 200.0,
                "positionType": "SHORT", "exchangeSegment": "NSE_FNO",
                "productType": "INTRADAY", "tradingSymbol": "NIFTY 23000 PE",
            }),
        ];
        let holdings = vec![
            json!({
                "securityId": "1660", "totalQty": 10, "avgCostPrice": 240.5,
                "lastPrice": 250.0, "tradingSymbol": "ITC", "exchange": "NSE_EQ",
                "isin": "INE154A01025",
            }),
            json!({
                "securityId": "9999", "totalQty": 5, "avgCostPrice": 100.0,
                "tradingSymbol": "LIVE", "exchange": "NSE_EQ",
            }),
            json!({
                "securityId": "8888", "totalQty": 2, "avgCostPrice": 50.0,
                "tradingSymbol": "QUIET", "exchange": "NSE_EQ",
            }),
        ];
        let mut ltp = HashMap::new();
        ltp.insert(1001i64, 120.0);
        ltp.insert(1002i64, 180.0);
        ltp.insert(9999i64, 110.0);

        let v = account_payload(&funds, &positions, &holdings, &ltp);
        assert_eq!(v["status"], json!("success"));
        let d = &v["data"];
        assert_eq!(d["balance"]["available"], json!(150000.0));
        assert_eq!(d["balance"]["used_margin"], json!(50000.0));
        assert_eq!(d["balance"]["collateral"], json!(10000.0));
        assert_eq!(d["balance"]["total"], json!(200000.0));
        assert_eq!(d["balance"]["opening_balance"], json!(200000.0));
        assert_eq!(d["pnl"], json!(2500.0));

        let p = d["positions"].as_array().unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0]["symbol"], json!("NIFTY 24000 CE"));
        assert_eq!(p[0]["qty"], json!(75));
        assert_eq!(p[0]["buy_avg"], json!(100.0));
        assert_eq!(p[0]["ltp"], json!(120.0));
        assert_eq!(p[0]["pnl"], json!(1500.0));
        assert_eq!(p[0]["pnl_pct"], json!(20.0));
        assert_eq!(p[0]["type"], json!("LONG"));
        assert_eq!(p[0]["product"], json!("INTRADAY"));
        assert_eq!(p[1]["pnl"], json!(1000.0));

        let h = d["holdings"].as_array().unwrap();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0]["symbol"], json!("ITC"));
        assert_eq!(h[0]["ltp"], json!(250.0));
        assert_eq!(h[0]["pnl"], json!(95.0));
        assert_eq!(h[0]["pnl_pct"], json!(3.95));
        assert_eq!(h[0]["isin"], json!("INE154A01025"));
        // Fallback to the live LTP cache when the broker omits lastPrice.
        assert_eq!(h[1]["ltp"], json!(110.0));
        assert_eq!(h[1]["pnl"], json!(50.0));
        // No price anywhere: falls back to avg cost => 0 P&L, never a fake loss.
        assert_eq!(h[2]["ltp"], json!(50.0));
        assert_eq!(h[2]["pnl"], json!(0.0));
    }

    #[test]
    fn reconnect_delta_only_adopts_broker_exposure_the_engine_misses() {
        // Both books agree -> nothing to adopt.
        assert_eq!(reconnect_delta(75, 75), 0);
        assert_eq!(reconnect_delta(-75, -75), 0);
        // Engine flat, broker holds -> adopt the whole broker net.
        assert_eq!(reconnect_delta(0, 75), 75);
        assert_eq!(reconnect_delta(0, -40), -40);
        // Broker holds more in the same direction -> adopt only the extra.
        assert_eq!(reconnect_delta(75, 100), 25);
        assert_eq!(reconnect_delta(-75, -100), -25);
        // Broker holds less (partial exit) -> never adopt an opposite leg; the
        // engine qty is aligned to the open qty instead.
        assert_eq!(reconnect_delta(75, 40), 0);
        assert_eq!(reconnect_delta(-75, -40), 0);
        // Opposite signs -> no blind guess.
        assert_eq!(reconnect_delta(75, -75), 0);
    }
}

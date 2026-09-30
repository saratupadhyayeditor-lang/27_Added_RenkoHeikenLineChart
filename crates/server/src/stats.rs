//! Trade Stats analytics for the Realtime and Paper trading engines.
//!
//! This is a Rust port of the old Python app's `tradestats.js` (the "Trade
//! Stats" report) and `realtimestats.js` (the "Realtime Market Trade Stats"
//! report). Every piece of logic - IST-anchored period windows, the trade
//! statistics, the strategy-wise breakdown, the most-profitable-time-of-day
//! analysis and all chart series - is computed here. The browser only renders
//! the JSON returned by `GET /api/rt/stats` and `GET /api/paper/stats`.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::realtime::RealtimeState;

const IST_MS: i64 = 330 * 60_000;
const DAY_MS: i64 = 86_400_000;
const MONS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct StatsQuery {
    #[serde(default)]
    pub range: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub engine: String,
}

// ---------------------------------------------------------------------------
// Civil date maths (no chrono dependency)
// ---------------------------------------------------------------------------

/// Days since Unix epoch -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// (year, month, day) -> days since Unix epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[derive(Clone, Copy)]
struct Ist {
    y: i64,
    m: u32,
    d: u32,
    h: u32,
    min: u32,
    /// 0 = Sunday .. 6 = Saturday
    wd: u32,
}

fn ist_parts(ms: i64) -> Ist {
    let local = ms + IST_MS;
    let days = local.div_euclid(DAY_MS);
    let rem = local.rem_euclid(DAY_MS);
    let (y, m, d) = civil_from_days(days);
    Ist {
        y,
        m,
        d,
        h: (rem / 3_600_000) as u32,
        min: ((rem % 3_600_000) / 60_000) as u32,
        // 1970-01-01 was a Thursday (wd = 4).
        wd: (days + 4).rem_euclid(7) as u32,
    }
}

fn ist_day_start(ms: i64) -> i64 {
    let p = ist_parts(ms);
    days_from_civil(p.y, p.m, p.d) * DAY_MS - IST_MS
}

fn pad2(n: u32) -> String {
    format!("{n:02}")
}

fn fmt_clock(ms: i64) -> String {
    if ms <= 0 {
        return "--".into();
    }
    let p = ist_parts(ms);
    let ap = if p.h < 12 { "AM" } else { "PM" };
    let h12 = if p.h % 12 == 0 { 12 } else { p.h % 12 };
    format!("{}:{} {}", pad2(h12), pad2(p.min), ap)
}

fn fmt_dt(ms: i64) -> String {
    if ms <= 0 {
        return "--".into();
    }
    let p = ist_parts(ms);
    let ap = if p.h < 12 { "AM" } else { "PM" };
    let h12 = if p.h % 12 == 0 { 12 } else { p.h % 12 };
    format!(
        "{} {} {}:{} {}",
        pad2(p.d),
        MONS[(p.m - 1) as usize],
        pad2(h12),
        pad2(p.min),
        ap
    )
}

/// IST civil day key `YYYY-MM-DD`, used to de-duplicate/aggregate by day.
fn day_key(ms: i64) -> String {
    let p = ist_parts(ms);
    format!("{}-{}-{}", p.y, pad2(p.m), pad2(p.d))
}

fn day_key_label(k: &str) -> String {
    let parts: Vec<&str> = k.split('-').collect();
    if parts.len() < 3 {
        return k.to_string();
    }
    let d: u32 = parts[2].parse().unwrap_or(0);
    let m: i64 = parts[1].parse().unwrap_or(1);
    let idx = (m - 1).clamp(0, 11) as usize;
    format!("{} {}", pad2(d), MONS[idx])
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

// ---------------------------------------------------------------------------
// Period windows (IST anchored, exactly like the old report)
// ---------------------------------------------------------------------------

/// `(from_ms, label)` for a period key. `to` is always now (open ended).
fn period_meta(range: &str, now: i64) -> (i64, &'static str) {
    match range {
        "1h" => (now - 3_600_000, "Last 1 hour"),
        "today" => (ist_day_start(now), "Today"),
        "week" => {
            let ps = ist_parts(now);
            let dow = ((ps.wd + 6) % 7) as i64; // IST Monday = 0
            (ist_day_start(now) - dow * DAY_MS, "This week")
        }
        "month" => {
            let ps = ist_parts(now);
            (days_from_civil(ps.y, ps.m, 1) * DAY_MS - IST_MS, "This month")
        }
        "6m" => {
            let ps = ist_parts(now);
            let mut mo = ps.m as i64 - 1 - 6;
            let mut yy = ps.y;
            if mo < 0 {
                mo += 12;
                yy -= 1;
            }
            (
                days_from_civil(yy, (mo + 1) as u32, 1) * DAY_MS - IST_MS,
                "Last 6 months",
            )
        }
        "year" => {
            let ps = ist_parts(now);
            (days_from_civil(ps.y, 1, 1) * DAY_MS - IST_MS, "This year")
        }
        _ => (0, "All time"),
    }
}

// ---------------------------------------------------------------------------
// Trades
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Trade {
    symbol: String,
    symbol_id: i64,
    side: String,
    qty: f64,
    entry: f64,
    exit: f64,
    pnl: f64,
    net: f64,
    charges: f64,
    at: i64,
    entry_at: i64,
    reason: String,
    strategy: String,
    lots: Option<f64>,
    lot_size: Option<f64>,
}

fn jnum(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64()).filter(|f| f.is_finite())
}

fn jint(v: &Value, k: &str) -> i64 {
    v.get(k)
        .and_then(|x| x.as_i64())
        .or_else(|| v.get(k).and_then(|x| x.as_f64()).map(|f| f as i64))
        .unwrap_or(0)
}

fn jstr(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn norm(c: &Value, charges_on: bool) -> Option<Trade> {
    let at = {
        let a = jint(c, "closedAt");
        if a > 0 {
            a
        } else {
            jint(c, "at")
        }
    };
    if at <= 0 {
        return None;
    }
    let symbol = {
        let s = jstr(c, "tradingSymbol");
        if s.is_empty() {
            jstr(c, "symbol")
        } else {
            s
        }
    };
    let symbol_id = {
        let s = jint(c, "securityId");
        if s > 0 {
            s
        } else {
            jint(c, "symbolId")
        }
    };
    if symbol.is_empty() && symbol_id <= 0 {
        return None;
    }
    let pnl = jnum(c, "pnl").unwrap_or(0.0);
    // Net basis when the engine banked charges and the "Deduct Dhan charges"
    // toggle is on, else the gross P&L.
    let (net, charges) = if charges_on {
        (
            jnum(c, "netPnl").unwrap_or(pnl),
            jnum(c, "charges").unwrap_or(0.0),
        )
    } else {
        (pnl, 0.0)
    };
    Some(Trade {
        symbol,
        symbol_id,
        side: jstr(c, "side"),
        qty: jnum(c, "qty").unwrap_or(0.0),
        entry: jnum(c, "entry").unwrap_or(0.0),
        exit: jnum(c, "exit").unwrap_or(0.0),
        pnl,
        net,
        charges,
        at,
        entry_at: jint(c, "openedAt"),
        reason: {
            let r = jstr(c, "reason");
            if r.is_empty() {
                "Closed".into()
            } else {
                r
            }
        },
        strategy: jstr(c, "strategyName"),
        lots: jnum(c, "lots"),
        lot_size: jnum(c, "lotSize"),
    })
}

/// The Indicator-filters run mode records its trades under one synthetic
/// "all together" strategy name; the Mode dropdown and the strategy table use
/// this split.
fn is_filter(t: &Trade) -> bool {
    t.strategy.to_lowercase().starts_with("indicator filter")
}

fn mode_ok(t: &Trade, mode: &str) -> bool {
    match mode {
        "filter" => is_filter(t),
        "strategy" => !is_filter(t),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    n: usize,
    wins: usize,
    losses: usize,
    net: f64,
    charges: f64,
    gross_profit: f64,
    gross_loss: f64,
    avg_win: Option<f64>,
    avg_loss: Option<f64>,
    best: Option<f64>,
    worst: Option<f64>,
    profit_factor: Option<f64>,
    pf_infinite: bool,
    max_dd: f64,
    max_dd_pct: f64,
    max_streak: usize,
    max_lose_streak: usize,
}

fn stats_of(trades: &[&Trade]) -> Stats {
    let mut s = Stats::default();
    let mut peak = 0.0_f64;
    let mut streak = 0_usize;
    let mut lose_streak = 0_usize;
    let mut cum = 0.0_f64;
    for t in trades {
        let net = t.net;
        s.n += 1;
        s.charges += t.charges;
        s.net += net;
        cum += net;
        if net > 0.0 {
            s.wins += 1;
            s.gross_profit += net;
            streak += 1;
            lose_streak = 0;
            s.max_streak = s.max_streak.max(streak);
            s.best = Some(match s.best {
                Some(b) if b >= net => b,
                _ => net,
            });
        } else {
            s.losses += 1;
            s.gross_loss += net.abs();
            lose_streak += 1;
            streak = 0;
            s.max_lose_streak = s.max_lose_streak.max(lose_streak);
            s.worst = Some(match s.worst {
                Some(w) if w <= net => w,
                _ => net,
            });
        }
        if cum > peak {
            peak = cum;
        }
        let dd = peak - cum;
        if dd > s.max_dd {
            s.max_dd = dd;
            if peak > 0.0 {
                s.max_dd_pct = (dd / peak) * 100.0;
            }
        }
    }
    if s.wins > 0 {
        s.avg_win = Some(s.gross_profit / s.wins as f64);
    }
    if s.losses > 0 {
        s.avg_loss = Some(s.gross_loss / s.losses as f64);
    }
    if s.gross_loss > 0.0 {
        s.profit_factor = Some(s.gross_profit / s.gross_loss);
    } else if s.gross_profit > 0.0 {
        s.pf_infinite = true;
    } else {
        s.profit_factor = Some(0.0);
    }
    s
}

fn stats_json(s: &Stats) -> Value {
    json!({
        "n": s.n,
        "wins": s.wins,
        "losses": s.losses,
        "net": r2(s.net),
        "charges": r2(s.charges),
        "grossProfit": r2(s.gross_profit),
        "grossLoss": r2(s.gross_loss),
        "avgWin": s.avg_win.map(r2),
        "avgLoss": s.avg_loss.map(r2),
        "best": s.best.map(r2),
        "worst": s.worst.map(r2),
        "profitFactor": s.profit_factor.map(r2),
        "profitFactorInfinite": s.pf_infinite,
        "maxDD": r2(s.max_dd),
        "maxDDPct": r2(s.max_dd_pct),
        "maxStreak": s.max_streak,
        "maxLoseStreak": s.max_lose_streak,
        "avg": if s.n > 0 { r2(s.net / s.n as f64) } else { 0.0 },
        "winRate": if s.n > 0 { (s.wins as f64 / s.n as f64) * 100.0 } else { 0.0 },
    })
}

// ---------------------------------------------------------------------------
// Chart series
// ---------------------------------------------------------------------------

fn equity_series(trades: &[&Trade]) -> Value {
    if trades.is_empty() {
        return json!({ "labels": [], "values": [] });
    }
    let step = (trades.len() / 400).max(1);
    let mut labels: Vec<String> = Vec::new();
    let mut values: Vec<f64> = Vec::new();
    let mut cum = 0.0_f64;
    for (i, t) in trades.iter().enumerate() {
        cum += t.net;
        if i % step == 0 || i == trades.len() - 1 {
            labels.push(fmt_dt(t.at));
            values.push(r2(cum));
        }
    }
    json!({ "labels": labels, "values": values })
}

fn bar_color(v: f64) -> &'static str {
    if v >= 0.0 {
        "rgba(0,212,170,0.75)"
    } else {
        "rgba(239,83,80,0.75)"
    }
}

/// Net P&L distribution: by day when more than one day is present, else by
/// entry hour when there are enough trades, else one bar per trade.
fn dist_series(trades: &[&Trade]) -> Value {
    if trades.is_empty() {
        return json!({ "labels": [], "values": [], "colors": [], "byDay": false });
    }
    let mut day_map: std::collections::BTreeMap<String, f64> = std::collections::BTreeMap::new();
    for t in trades {
        *day_map.entry(day_key(t.at)).or_insert(0.0) += t.net;
    }
    let by_day = day_map.len() > 1;
    let (labels, values): (Vec<String>, Vec<f64>) = if by_day {
        (
            day_map.keys().map(|k| day_key_label(k)).collect(),
            day_map.values().map(|v| r2(*v)).collect(),
        )
    } else if trades.len() > 20 {
        let mut hour_map: std::collections::BTreeMap<u32, f64> = std::collections::BTreeMap::new();
        for t in trades {
            let p = ist_parts(t.at);
            *hour_map.entry(p.h).or_insert(0.0) += t.net;
        }
        (
            hour_map.keys().map(|h| format!("{}:00", pad2(*h))).collect(),
            hour_map.values().map(|v| r2(*v)).collect(),
        )
    } else {
        (
            trades.iter().map(|t| fmt_clock(t.at)).collect(),
            trades.iter().map(|t| r2(t.net)).collect(),
        )
    };
    let colors: Vec<&str> = values.iter().map(|v| bar_color(*v)).collect();
    json!({ "labels": labels, "values": values, "colors": colors, "byDay": by_day })
}

// ---------------------------------------------------------------------------
// Time-of-day buckets
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Bucket {
    count: usize,
    wins: usize,
    losses: usize,
    net: f64,
    charges: f64,
}

fn bucket_json(b: &Bucket) -> Value {
    json!({
        "count": b.count,
        "wins": b.wins,
        "losses": b.losses,
        "wr": if b.count > 0 { (b.wins as f64 / b.count as f64) * 100.0 } else { 0.0 },
        "net": r2(b.net),
        "avg": if b.count > 0 { r2(b.net / b.count as f64) } else { 0.0 },
        "charges": r2(b.charges),
    })
}

fn bucket_by<F: Fn(&Trade) -> Option<i64>>(trades: &[&Trade], key: F) -> std::collections::BTreeMap<i64, Bucket> {
    let mut m: std::collections::BTreeMap<i64, Bucket> = std::collections::BTreeMap::new();
    for t in trades {
        let Some(k) = key(t) else { continue };
        let b = m.entry(k).or_default();
        b.count += 1;
        b.net += t.net;
        b.charges += t.charges;
        if t.net > 0.0 {
            b.wins += 1;
        } else {
            b.losses += 1;
        }
    }
    m
}

fn scope_filter<'a>(trades: &[&'a Trade], scope: &str, now: i64) -> Vec<&'a Trade> {
    match scope {
        "today" => {
            let start = ist_day_start(now);
            trades.iter().copied().filter(|t| t.at >= start).collect()
        }
        "7d" => {
            let start = now - 7 * DAY_MS;
            trades.iter().copied().filter(|t| t.at >= start).collect()
        }
        "30d" => {
            let start = now - 30 * DAY_MS;
            trades.iter().copied().filter(|t| t.at >= start).collect()
        }
        _ => trades.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

pub fn report(closed: &[Value], q: &StatsQuery, engine: &str, armed: bool, charges_on: bool) -> Value {
    let now = now_ms();
    let mut all: Vec<Trade> = closed.iter().filter_map(|c| norm(c, charges_on)).collect();
    all.sort_by_key(|t| t.at);

    let mode = q.mode.as_str();
    let scoped: Vec<&Trade> = all.iter().filter(|t| mode_ok(t, mode)).collect();

    // Period count chips: total executed trades per window over full history.
    let chip_keys = ["1h", "today", "week", "month", "6m", "year"];
    let chips: Vec<Value> = chip_keys
        .iter()
        .map(|k| {
            let (from, label) = period_meta(k, now);
            let list: Vec<&Trade> = scoped.iter().copied().filter(|t| t.at >= from).collect();
            let st = stats_of(&list);
            json!({ "key": k, "label": label, "n": st.n, "net": r2(st.net) })
        })
        .collect();

    // Selected period.
    let (from, range_label) = period_meta(&q.range, now);
    let range_trades: Vec<&Trade> = scoped
        .iter()
        .copied()
        .filter(|t| t.at >= from)
        .collect();
    let st = stats_of(&range_trades);

    // Strategy-wise breakdown for the selected period.
    let mut smap: std::collections::BTreeMap<String, Bucket> = std::collections::BTreeMap::new();
    let mut slast: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for t in &range_trades {
        let name = if t.strategy.is_empty() {
            "Strategy".to_string()
        } else {
            t.strategy.clone()
        };
        let b = smap.entry(name.clone()).or_default();
        b.count += 1;
        b.net += t.net;
        b.charges += t.charges;
        if t.net > 0.0 {
            b.wins += 1;
        } else {
            b.losses += 1;
        }
        let e = slast.entry(name).or_insert(0);
        if t.at > *e {
            *e = t.at;
        }
    }
    let total_net: f64 = smap.values().map(|b| b.net).sum();
    let mut strategy_rows: Vec<(String, Bucket, i64)> = smap
        .into_iter()
        .map(|(k, b)| {
            let last = slast.get(&k).copied().unwrap_or(0);
            (k, b, last)
        })
        .collect();
    strategy_rows.sort_by(|a, b| {
        b.1.count
            .cmp(&a.1.count)
            .then(b.1.net.partial_cmp(&a.1.net).unwrap_or(std::cmp::Ordering::Equal))
    });
    let strategy_json: Vec<Value> = strategy_rows
        .iter()
        .map(|(name, b, last)| {
            let filter = name.to_lowercase().starts_with("indicator filter");
            let contrib = if total_net != 0.0 {
                (b.net / total_net) * 100.0
            } else {
                0.0
            };
            json!({
                "name": name,
                "count": b.count,
                "wins": b.wins,
                "losses": b.losses,
                "wr": if b.count > 0 { (b.wins as f64 / b.count as f64) * 100.0 } else { 0.0 },
                "net": r2(b.net),
                "avg": if b.count > 0 { r2(b.net / b.count as f64) } else { 0.0 },
                "charges": r2(b.charges),
                "contrib": r2(contrib),
                "last": last,
                "isFilter": filter,
            })
        })
        .collect();

    // Executed trades rows, newest first.
    let mut rows: Vec<&Trade> = range_trades.clone();
    rows.sort_by_key(|t| std::cmp::Reverse(t.at));
    let shown = rows.len();
    let trades_json: Vec<Value> = rows
        .iter()
        .map(|t| {
            json!({
                "symbol": t.symbol,
                "symbolId": t.symbol_id,
                "side": t.side,
                "qty": t.qty,
                "entry": t.entry,
                "exit": t.exit,
                "pnl": r2(t.pnl),
                "net": r2(t.net),
                "charges": r2(t.charges),
                "reason": t.reason,
                "entryAt": t.entry_at,
                "at": t.at,
                "strategy": t.strategy,
                "lots": t.lots,
                "lotSize": t.lot_size,
                "isFilter": is_filter(t),
                "engine": engine,
            })
        })
        .collect();

    // Most-profitable-time-of-day (its own scope, independent of the period).
    let scope = if q.scope.is_empty() { "all" } else { q.scope.as_str() };
    let insight_trades = scope_filter(&scoped, scope, now);
    let hour_map = bucket_by(&insight_trades, |t| {
        let ts = if t.entry_at > 0 { t.entry_at } else { t.at };
        Some(ist_parts(ts).h as i64)
    });
    let week_map = bucket_by(&insight_trades, |t| Some(ist_parts(t.at).wd as i64));

    // Hour insight summary (best hour selection mirrors the old report).
    let mut best_win_key: Option<i64> = None;
    let mut best_win_count = 0_usize;
    let mut best_net_key: Option<i64> = None;
    let mut best_net_val = f64::NEG_INFINITY;
    let mut best_wr_key: Option<i64> = None;
    let mut best_wr_val = f64::NEG_INFINITY;
    for (k, b) in &hour_map {
        if best_win_key.is_none() || b.wins > best_win_count {
            best_win_key = Some(*k);
            best_win_count = b.wins;
        }
        if b.net > best_net_val {
            best_net_val = b.net;
            best_net_key = Some(*k);
        }
        let wr = if b.count > 0 {
            (b.wins as f64 / b.count as f64) * 100.0
        } else {
            0.0
        };
        if b.count >= 3 && wr > best_wr_val {
            best_wr_val = wr;
            best_wr_key = Some(*k);
        }
    }
    let best_hour = match best_win_key {
        Some(k) if best_win_count > 0 => Some(k),
        _ => match best_net_key {
            Some(k) if best_net_val > 0.0 => Some(k),
            _ => None,
        },
    };
    let bh = best_hour.and_then(|k| hour_map.get(&k));
    let hour_insight = json!({
        "scope": scope,
        "total": insight_trades.len(),
        "bestHour": best_hour,
        "bestHourWins": bh.map(|b| b.wins),
        "bestHourCount": bh.map(|b| b.count),
        "bestHourWr": bh.map(|b| if b.count > 0 { (b.wins as f64 / b.count as f64) * 100.0 } else { 0.0 }),
        "bestHourNet": bh.map(|b| r2(b.net)),
        "bestNetKey": best_net_key.filter(|_| best_net_key.is_some()),
        "bestNetVal": best_net_key.map(|_| r2(best_net_val)),
        "bestWrKey": best_wr_key,
        "bestWrVal": best_wr_key.map(|_| best_wr_val),
    });

    // Hour table + series.
    let hour_keys: Vec<i64> = hour_map.keys().copied().collect();
    let hour_rows: Vec<Value> = hour_keys
        .iter()
        .map(|k| {
            let mut v = bucket_json(hour_map.get(k).unwrap());
            v["hour"] = json!(k);
            v
        })
        .collect();
    let hour_labels: Vec<String> = hour_keys.iter().map(|k| format!("{}:00", pad2(*k as u32))).collect();
    let hour_values: Vec<f64> = hour_keys.iter().map(|k| r2(hour_map.get(k).unwrap().net)).collect();
    let hour_colors: Vec<&str> = hour_keys
        .iter()
        .map(|k| {
            if Some(*k) == best_hour {
                "rgba(0,212,170,0.95)"
            } else if hour_map.get(k).unwrap().net >= 0.0 {
                "rgba(0,212,170,0.6)"
            } else {
                "rgba(239,83,80,0.6)"
            }
        })
        .collect();
    let best_index: i64 = best_hour
        .and_then(|k| hour_keys.iter().position(|x| *x == k))
        .map(|i| i as i64)
        .unwrap_or(-1);

    // Weekday table + series (Mon-first trading week).
    let names = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let week_order = [1_i64, 2, 3, 4, 5, 6, 0];
    let week_rows: Vec<Value> = week_order
        .iter()
        .filter_map(|wd| week_map.get(wd).map(|b| {
            let mut v = bucket_json(b);
            v["wd"] = json!(wd);
            v["name"] = json!(names[*wd as usize]);
            v
        }))
        .collect();
    let week_labels: Vec<String> = week_order
        .iter()
        .filter(|wd| week_map.contains_key(wd))
        .map(|wd| names[*wd as usize].to_string())
        .collect();
    let week_values: Vec<f64> = week_order
        .iter()
        .filter_map(|wd| week_map.get(wd).map(|b| r2(b.net)))
        .collect();
    let week_colors: Vec<&str> = week_values.iter().map(|v| bar_color(*v)).collect();

    json!({
        "ok": true,
        "engine": engine,
        "armed": armed,
        "range": if q.range.is_empty() { "all" } else { q.range.as_str() },
        "mode": mode,
        "scope": scope,
        "engineSel": q.engine,
        "rangeLabel": range_label,
        "statusText": format!(
            "{} executed trade{} in this period",
            st.n,
            if st.n == 1 { "" } else { "s" }
        ),
        "chips": chips,
        "stats": stats_json(&st),
        "trades": trades_json,
        "tradesShown": shown,
        "tradesTotal": range_trades.len(),
        "strategy": strategy_json,
        "strategyCount": strategy_json.len(),
        "strategyTotalNet": r2(total_net),
        "hour": hour_rows,
        "weekday": week_rows,
        "insight": hour_insight,
        "equity": equity_series(&range_trades),
        "dist": dist_series(&range_trades),
        "hourSeries": { "labels": hour_labels, "values": hour_values, "colors": hour_colors, "bestIndex": best_index },
        "weekSeries": { "labels": week_labels, "values": week_values, "colors": week_colors },
    })
}

// ---------------------------------------------------------------------------
// HTTP handler
// ---------------------------------------------------------------------------

/// Memoised reports, keyed by a stable hash of (engine, charges, query, ledger).
/// The Trade Stats tab polls every few seconds; recomputing the whole report -
/// normalising every closed row plus the period/strategy/hour buckets - from
/// scratch on each poll is what made the pane crawl on a large ledger. The key
/// changes only when the query or the ledger actually changes, so an unchanged
/// poll is answered from the cache. Capped and cleared wholesale when it grows.
static STATS_CACHE: Mutex<Vec<(u64, Value)>> = Mutex::new(Vec::new());

pub async fn stats_get(
    State(rt): State<RealtimeState>,
    Query(q): Query<StatsQuery>,
) -> impl IntoResponse {
    let engine = if rt.paper { "papertrade" } else { "realtime" };
    let charges_on = rt.charges_on();
    // Seed the ledger hash with everything except the ledger itself; the engine
    // adds the ledger + armed flag without cloning it.
    let mut seed = DefaultHasher::new();
    engine.hash(&mut seed);
    charges_on.hash(&mut seed);
    q.range.hash(&mut seed);
    q.mode.hash(&mut seed);
    q.scope.hash(&mut seed);
    q.engine.hash(&mut seed);
    let key = rt.closed_signature(seed.finish());
    if let Ok(g) = STATS_CACHE.lock() {
        if let Some((_, v)) = g.iter().find(|(k, _)| *k == key) {
            return Json(v.clone());
        }
    }
    let (closed, armed) = rt.stats_input();
    let value = report(&closed, &q, engine, armed, charges_on);
    if let Ok(mut g) = STATS_CACHE.lock() {
        if g.len() >= 8 {
            g.clear();
        }
        g.push((key, value.clone()));
    }
    Json(value)
}

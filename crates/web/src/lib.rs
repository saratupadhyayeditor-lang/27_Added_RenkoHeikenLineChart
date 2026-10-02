use std::cell::RefCell;
use std::rc::Rc;

use std::collections::BTreeMap;

use js_sys::{Array, Object, Reflect};
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    CanvasRenderingContext2d, CustomEvent, Document, Element, HtmlCanvasElement, MouseEvent,
    WheelEvent, Window,
};

use algo_core::indicators::registry;
use algo_core::model::*;
use algo_core::oi_trend::{
    self as oit, ClassifyOpts, ClassifyResult, ContextOpts, LevelData, LevelOpts, OiRecord,
    Regime, RegimeOpts, RegimeResult,
};
use algo_core::Candle;
use algo_core::chart_type::{self, ChartKind, RenkoConfig, RenkoMode, RenkoSource};

mod optionchain;

/// Compute one registry indicator and apply the shared straight-line colour
/// rule: every straight-line indicator draws green while the line rises
/// (bullish) and red while it falls (bearish). Non-straight-line ids are
/// untouched by `color_straight_line`.
fn compute_indicator(entry: &IndicatorEntry, candles: &[Candle], settings: &Settings) -> Vec<SeriesOut> {
    let mut series = (entry.compute)(candles, settings);
    algo_core::indicators::color_straight_line(&entry.def.id, &mut series, settings);
    series
}


// ---------------------------------------------------------------------------
// DOM helpers
// ---------------------------------------------------------------------------

fn window() -> Window {
    web_sys::window().expect("no window")
}
fn document() -> Document {
    window().document().expect("no document")
}
fn by_id(id: &str) -> Option<Element> {
    document().get_element_by_id(id)
}
fn set_html(id: &str, html: &str) {
    if let Some(e) = by_id(id) {
        e.set_inner_html(html);
    }
}

/// Promise-based sleep built on `setTimeout`, for bounded fetch retries.
async fn sleep_ms(ms: i32) {
    let p = js_sys::Promise::new(&mut |resolve, _reject| {
        let cb: &js_sys::Function = resolve.unchecked_ref();
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(cb, ms);
    });
    let _ = JsFuture::from(p).await;
}

fn ls_get(key: &str) -> Option<String> {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item(key).ok().flatten())
}

fn ls_set(key: &str, val: &str) {
    if let Some(s) = window().local_storage().ok().flatten() {
        let _ = s.set_item(key, val);
    }
}

/// Gear / remove icons for the deployed-indicator legend rows (same glyphs as
/// the old app, so the buttons read identically).
const SVG_GEAR: &str = "<svg width='10' height='10' viewBox='0 0 24 24' fill='currentColor'><path d='M19.14 12.94c.04-.3.06-.61.06-.94 0-.32-.02-.64-.07-.94l2.03-1.58c.18-.14.23-.41.12-.61l-1.92-3.32c-.12-.22-.37-.29-.59-.22l-2.39.96c-.5-.38-1.03-.7-1.62-.94l-.36-2.54c-.04-.24-.24-.41-.48-.41h-3.84c-.24 0-.43.17-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96c-.22-.08-.47 0-.59.22L2.74 8.87c-.12.21-.08.47.12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58c-.18.14-.23.41-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32c.12-.22.07-.47-.12-.61l-2.01-1.58zM12 15.6c-1.98 0-3.6-1.62-3.6-3.6s1.62-3.6 3.6-3.6 3.6 1.62 3.6 3.6-1.62 3.6-3.6 3.6z'/></svg>";
const SVG_X: &str = "<svg width='10' height='10' viewBox='0 0 24 24' stroke='currentColor' stroke-width='2.5' fill='none'><path d='M18 6L6 18M6 6l12 12'/></svg>";
const SVG_PLUS: &str = "<svg width='10' height='10' viewBox='0 0 24 24' stroke='currentColor' stroke-width='2.5' fill='none'><path d='M12 5v14M5 12h14'/></svg>";
const ALERT_COLORS: [&str; 8] = [
    "#ff5252", "#26a69a", "#7ad7ff", "#ff9800", "#b39ddb", "#ff6b6b", "#ffb300", "#66ccff",
];
const SAVE_KEY: &str = "algo_chart_indicators";

/// User-drawn BB%b alert line (the pane "+" button).
#[derive(Clone, Debug)]
struct AlertLine {
    id: String,
    price: f64,
    color: String,
}

fn close_settings() {
    if let Some(p) = by_id("indSettings") {
        p.set_class_name("ind-settings hidden");
    }
}

fn set_fill(ctx: &CanvasRenderingContext2d, c: &str) {
    ctx.set_fill_style(&JsValue::from_str(c));
}
fn set_stroke(ctx: &CanvasRenderingContext2d, c: &str) {
    ctx.set_stroke_style(&JsValue::from_str(c));
}

// ---------------------------------------------------------------------------
// Time helpers (candle time is epoch seconds of the IST wall clock as naive UTC)
// ---------------------------------------------------------------------------

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

fn fmt_time(sec: i64, intraday: bool) -> String {
    let days = sec.div_euclid(86400);
    let rem = sec.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    if intraday {
        let h = rem / 3600;
        let mi = (rem % 3600) / 60;
        let ampm = if h >= 12 { "PM" } else { "AM" };
        let h12 = if h % 12 == 0 { 12 } else { h % 12 };
        format!("{:02}:{:02} {}", h12, mi, ampm)
    } else {
        format!("{:02}-{:02}-{:02}", d, m, y % 100)
    }
}

/// Short IST month names for the timeline's day-boundary labels.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Day label for a candle time (`17-Sep`), used on the timeline whenever a new
/// trading day starts so the intraday axis is never date-less.
fn fmt_day(sec: i64) -> String {
    let (_y, m, d) = civil_from_days(sec.div_euclid(86400));
    let mon = MONTHS.get((m - 1).clamp(0, 11) as usize).copied().unwrap_or("");
    format!("{:02}-{}", d, mon)
}

/// Full IST date + time for the crosshair tooltip: `17-Sep 10:15 AM` intraday,
/// `17-Sep-25` for daily and slower timeframes.
fn fmt_datetime(sec: i64, intraday: bool) -> String {
    if !intraday {
        return fmt_time(sec, false);
    }
    format!("{} {}", fmt_day(sec), fmt_time(sec, true))
}

fn is_intraday(tf: &str) -> bool {
    !matches!(tf, "day" | "week" | "month" | "year")
}

fn fmt_val(v: f64, kind: Option<&str>) -> String {
    match kind {
        Some("percent") => format!("{:.2}%", v),
        Some("decimal") => format!("{:.3}", v),
        _ => {
            let a = v.abs();
            if a >= 1_000_000.0 {
                format!("{:.2}M", v / 1_000_000.0)
            } else if a >= 1_000.0 {
                format!("{:.2}K", v / 1_000.0)
            } else if a >= 100.0 {
                format!("{:.1}", v)
            } else {
                format!("{:.3}", v)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct Inst {
    uid: u64,
    id: String,
    name: String,
    kind: IndType,
    format: Option<String>,
    settings: Settings,
    series: Vec<SeriesOut>,
    markers: Vec<Marker>,
    alert_lines: Vec<AlertLine>,
}

/// Direction-state overlay line pushed by the OI Trend module (owned by JS).
struct DirOverlay {
    data: Vec<Point>,
    color: String,
    line_width: f64,
    markers: Vec<Marker>,
}

/// Direction of a user-drawn position tool (TradingView's Long/Short Position).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PosDir {
    Long,
    Short,
}

/// A committed position tool: entry, auto-mirrored profit target and stop-loss.
/// Times are candle times (epoch seconds) so the box stays anchored when the
/// timeframe or zoom changes.
#[derive(Clone, Debug)]
struct PositionTool {
    dir: PosDir,
    entry_time: i64,
    end_time: i64,
    entry_price: f64,
    target_price: f64,
    stop_price: f64,
}

/// Which part of a position tool a drag is moving.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PosEditKind {
    Move,
    Target,
    Stop,
}

/// In-progress draw of a brand-new position (mouse is down).
#[derive(Clone)]
struct PosDrag {
    dir: PosDir,
    entry_time: i64,
    entry_price: f64,
    cur_time: i64,
    cur_price: f64,
}

/// In-progress edit of an existing position (mouse is down).
#[derive(Clone)]
struct PosEdit {
    idx: usize,
    kind: PosEditKind,
    grab_price: f64,
    orig: PositionTool,
}

struct App {
    candles: Vec<Candle>,
    /// Untransformed candles straight from history / the live feed. `candles`
    /// above is always `chart_type::build(chart_kind, raw, renko)`, so switching
    /// chart type (or a live tick) can rebuild the display series without
    /// re-fetching.
    raw: Vec<Candle>,
    chart_kind: ChartKind,
    renko: RenkoConfig,
    insts: Vec<Inst>,
    symbol_name: String,
    sec_id: i64,
    exch: String,
    inst_type: String,
    timeframe: String,
    view_start: f64,
    view_count: f64,
    cross: Option<(f64, f64)>,
    cross_idx: Option<usize>,
    pane_cross: Option<(u64, f64, f64)>,
    dragging: bool,
    drag_x: f64,
    drag_start: f64,
    uid_seq: u64,
    alert_seq: u64,
    alert_at: BTreeMap<(u64, String), f64>,
    alert_bar: i64,
    pane_canvas: Vec<(u64, HtmlCanvasElement)>,
    realtime: bool,
    #[allow(dead_code)]
    oi_enabled: bool,
    last_bar: i64,
    /// Cumulative day volume captured when the current forming bar was rolled
    /// locally, so its volume can be shown as a per-bar delta.
    bar_vol_base: f64,
    /// True once the forming bar was rolled from live ticks (not from history),
    /// which is when delta-volume tracking applies.
    local_bar: bool,
    /// Chart-owned live feed. The chart subscribes and consumes ticks on its own
    /// websocket (independent of the sidebar's quote cache), so candle formation
    /// is driven straight from the push stream.
    ws: Option<web_sys::WebSocket>,
    ws_retry: u32,
    /// Monotonic token for the in-flight candle load. A newer symbol/timeframe
    /// load bumps it so a slow response from a previous symbol can never
    /// overwrite the chart that is now on screen.
    chart_gen: u64,
    /// Latest tick for the active instrument, straight off the chart's socket.
    live_ltp: f64,
    live_vol: f64,
    trade_lines: Vec<PriceLine>,
    oc_lines: Vec<PriceLine>,
    dir_overlay: Option<DirOverlay>,
    /// Live PCR series (put OI / call OI) from the option chain, populated by the
    /// option-analytics feed so the `pcr` chart indicator renders real data
    /// instead of the empty placeholder shape.
    oc_pcr: Vec<Point>,
    /// Live ATM implied-volatility series from the option chain, feeding the `iv`
    /// chart indicator.
    oc_iv: Vec<Point>,
    /// Last full option chain fed to the OI Trend module, so the direction
    /// overlay + legend can be recomputed as the chart's forming candle moves
    /// without waiting for the next chain refresh (old app's `onTick`).
    oi_records: Option<Vec<OiRecord>>,
    oi_spot: f64,
    oi_dte: f64,
    /// Candle/chain signature the current OI overlays were computed for; an
    /// unchanged signature skips the (heavier) regime + level recompute.
    oi_sig: String,
    /// Cached OI legend markup so identical frames never rewrite the DOM.
    oi_legend_html: String,
    /// Last time (ms) the OI strip canvas was repainted, and the width it was
    /// painted at, mirroring the old app's 700ms / resize throttle.
    oi_strip_last: f64,
    oi_strip_w: f64,
    /// Armed position tool (None = normal pan/crosshair mode).
    pos_tool: Option<PosDir>,
    /// Committed Long/Short Position drawings for the active symbol.
    positions: Vec<PositionTool>,
    /// Brand-new position currently being dragged out.
    pos_drag: Option<PosDrag>,
    /// Existing position currently being moved/resized.
    pos_edit: Option<PosEdit>,
    /// Position part under the pointer (for the hover cursor).
    pos_hover: Option<(usize, PosEditKind)>,
}

impl App {
    fn new() -> Self {
        App {
            candles: Vec::new(),
            raw: Vec::new(),
            chart_kind: ChartKind::Candles,
            renko: RenkoConfig::default(),
            insts: Vec::new(),
            symbol_name: "NIFTY 50".into(),
            sec_id: 13,
            exch: "IDX_I".into(),
            inst_type: "INDEX".into(),
            timeframe: "5min".into(),
            view_start: 0.0,
            view_count: 120.0,
            cross: None,
            cross_idx: None,
            pane_cross: None,
            dragging: false,
            drag_x: 0.0,
            drag_start: 0.0,
            uid_seq: 1,
            alert_seq: 1,
            alert_at: BTreeMap::new(),
            alert_bar: 0,
            pane_canvas: Vec::new(),
            realtime: true,
            oi_enabled: false,
            last_bar: 0,
            bar_vol_base: 0.0,
            local_bar: false,
            ws: None,
            ws_retry: 0,
            chart_gen: 0,
            live_ltp: 0.0,
            live_vol: 0.0,
            trade_lines: Vec::new(),
            oc_lines: Vec::new(),
            dir_overlay: None,
            oc_pcr: Vec::new(),
            oc_iv: Vec::new(),
            oi_records: None,
            oi_spot: 0.0,
            oi_dte: 7.0,
            oi_sig: String::new(),
            oi_legend_html: String::new(),
            oi_strip_last: 0.0,
            oi_strip_w: 0.0,
            pos_tool: None,
            positions: Vec::new(),
            pos_drag: None,
            pos_edit: None,
            pos_hover: None,
        }
    }

    fn defaults_for(def: &IndicatorDef) -> Settings {
        let mut m = Settings::new();
        for i in &def.inputs {
            m.insert(i.key.clone(), i.def.clone());
        }
        for s in &def.style {
            m.insert(s.key.clone(), s.def.clone());
        }
        m
    }

    fn add(&mut self, id: &str) {
        if let Some(entry) = registry().into_iter().find(|x| x.def.id == id) {
            let settings = App::defaults_for(&entry.def);
            let series = compute_indicator(&entry, &self.candles, &settings);
            let markers = entry
                .markers
                .map(|f| f(&self.candles, &settings))
                .unwrap_or_default();
            let uid = self.uid_seq;
            self.uid_seq += 1;
            self.insts.push(Inst {
                uid,
                id: entry.def.id.clone(),
                name: entry.def.name.clone(),
                kind: entry.def.kind,
                format: entry.def.format.clone(),
                settings,
                series,
                markers,
                alert_lines: Vec::new(),
            });
            self.refresh_special_series();
            if matches!(entry.def.id.as_str(), "pcr" | "pcrrail" | "iv") {
                crate::optionchain::ensure_loaded();
            }
        }
    }

    /// Add an indicator instance applying the given settings overlay (used by the
    /// "Best Combo" preset so it reproduces the old app's recommended defaults).
    fn add_with(&mut self, id: &str, overrides: &[(&str, Value)]) {
        if let Some(entry) = registry().into_iter().find(|x| x.def.id == id) {
            let mut settings = App::defaults_for(&entry.def);
            for (k, v) in overrides {
                settings.insert((*k).to_string(), v.clone());
            }
            let series = compute_indicator(&entry, &self.candles, &settings);
            let markers = entry
                .markers
                .map(|f| f(&self.candles, &settings))
                .unwrap_or_default();
            let uid = self.uid_seq;
            self.uid_seq += 1;
            self.insts.push(Inst {
                uid,
                id: entry.def.id.clone(),
                name: entry.def.name.clone(),
                kind: entry.def.kind,
                format: entry.def.format.clone(),
                settings,
                series,
                markers,
                alert_lines: Vec::new(),
            });
            self.refresh_special_series();
            if matches!(entry.def.id.as_str(), "pcr" | "pcrrail" | "iv") {
                crate::optionchain::ensure_loaded();
            }
        }
    }

    fn recompute(&mut self, uid: u64) {
        for inst in self.insts.iter_mut() {
            if inst.uid != uid {
                continue;
            }
            if let Some(entry) = registry().into_iter().find(|x| x.def.id == inst.id) {
                inst.series = compute_indicator(&entry, &self.candles, &inst.settings);
                inst.markers = entry
                    .markers
                    .map(|f| f(&self.candles, &inst.settings))
                    .unwrap_or_default();
            }
        }
        self.refresh_special_series();
    }

    fn recompute_all(&mut self) {
        for inst in self.insts.iter_mut() {
            if let Some(entry) = registry().into_iter().find(|x| x.def.id == inst.id) {
                inst.series = compute_indicator(&entry, &self.candles, &inst.settings);
                inst.markers = entry
                    .markers
                    .map(|f| f(&self.candles, &inst.settings))
                    .unwrap_or_default();
            }
        }
        self.refresh_special_series();
    }

    /// Re-apply the option-chain PCR / ATM-IV series onto every `pcr` / `iv`
    /// indicator instance. The candle-only indicator compute functions have no
    /// chain access, so their output is empty and the real series is layered in
    /// here after each (re)compute.
    fn refresh_special_series(&mut self) {
        if self.oc_pcr.is_empty() && self.oc_iv.is_empty() {
            return;
        }
        let pcr = self.oc_pcr.clone();
        let iv = self.oc_iv.clone();
        for inst in self.insts.iter_mut() {
            apply_oc_analytics(inst, &pcr, &iv);
        }
    }

    fn remove(&mut self, uid: u64) {
        self.insts.retain(|i| i.uid != uid);
        self.pane_canvas.retain(|(u, _)| *u != uid);
    }

    fn fit_recent(&mut self) {
        let n = self.candles.len();
        if n == 0 {
            return;
        }
        self.view_count = 120.0_f64.min(n as f64);
        self.view_start = (n as f64 - self.view_count).max(0.0);
    }

    /// Rebuild the display series from the raw candles for the current chart
    /// type using the current Renko settings.
    fn rebuild_view(&mut self) {
        self.candles = chart_type::build(self.chart_kind, &self.raw, &self.renko);
    }
}

/// Exponential moving average over a `(time, value)` series, preserving the
/// source timestamps so the fast/slow PCR lines line up with the raw line.
fn ema_points(src: &[Point], period: usize) -> Vec<Point> {
    if src.is_empty() || period == 0 {
        return Vec::new();
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(src.len());
    let mut prev = src[0].value;
    for (i, p) in src.iter().enumerate() {
        prev = if i == 0 { p.value } else { p.value * k + prev * (1.0 - k) };
        out.push(Point {
            time: p.time,
            value: prev,
            color: None,
        });
    }
    out
}

/// Layer the live option-chain series onto a `pcr` / `iv` indicator instance.
/// `pcr` ships a raw line plus fast/slow EMAs; `iv` ships the raw line.
fn apply_oc_analytics(inst: &mut Inst, pcr: &[Point], iv: &[Point]) {
    match inst.id.as_str() {
        "pcr" => {
            if inst.series.is_empty() {
                return;
            }
            inst.series[0].data = pcr.to_vec();
            if inst.series.len() >= 3 {
                inst.series[1].data = ema_points(pcr, 9);
                inst.series[2].data = ema_points(pcr, 21);
            }
        }
        "iv" => {
            if inst.series.is_empty() {
                return;
            }
            inst.series[0].data = iv.to_vec();
        }
        _ => {}
    }
}

/// Push one option-chain analytics sample (PCR + ATM IV) at the current forming
/// candle's time and refresh every PCR / IV indicator on the chart. Called by
/// the option-chain tab after each chain render.
pub(crate) fn set_oc_analytics(pcr: f64, iv: f64) {
    with_app(|app| {
        let t = app.candles.last().map(|c| c.time).unwrap_or(0);
        if t <= 0 {
            return;
        }
        if pcr > 0.0 {
            upsert_point(&mut app.oc_pcr, t, pcr);
        }
        if iv > 0.0 {
            upsert_point(&mut app.oc_iv, t, iv);
        }
        app.refresh_special_series();
    });
    render_all();
}

/// Append or replace the sample at `time`, keeping the series time-ordered and
/// bounded so a long session cannot grow it without limit.
fn upsert_point(series: &mut Vec<Point>, time: i64, value: f64) {
    if let Some(last) = series.last_mut() {
        if last.time == time {
            last.value = value;
            return;
        }
    }
    series.push(Point {
        time,
        value,
        color: None,
    });
    if series.len() > 4000 {
        let drop = series.len() - 4000;
        series.drain(0..drop);
    }
}

thread_local! {
    static APP: RefCell<App> = RefCell::new(App::new());
}

fn with_app<F: FnOnce(&mut App)>(f: F) {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        f(&mut b);
    });
}

fn read_app<F: FnOnce(&App) -> R, R>(f: F) -> R {
    APP.with(|a| f(&a.borrow()))
}

/// Like `with_app` but returns a value from the closure.
fn with_app_ret<F: FnOnce(&mut App) -> R, R>(f: F) -> R {
    APP.with(|a| {
        let mut b = a.borrow_mut();
        f(&mut b)
    })
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

async fn fetch_candles(sec_id: i64, exch: &str, inst_type: &str, tf: &str) -> Vec<Candle> {
    #[derive(serde::Deserialize)]
    struct Resp {
        #[serde(default)]
        data: Vec<Candle>,
    }
    let url = format!(
        "/api/candles?security_id={}&exchange_segment={}&instrument_type={}&timeframe={}&force=1",
        sec_id, exch, inst_type, tf
    );
    let resp_promise = window().fetch_with_str(&url);
    let resp = match JsFuture::from(resp_promise).await {
        Ok(v) => Ok(v),
        Err(e) => Err(e),
    };
    let resp = match resp {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let resp: web_sys::Response = match resp.dyn_into() {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let text = match JsFuture::from(resp.text().unwrap_or_else(|_| js_sys::Promise::resolve(&JsValue::NULL))).await {
        Ok(t) => t.as_string().unwrap_or_default(),
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Resp>(&text).map(|r| r.data).unwrap_or_default()
}

fn load_chart() {
    load_chart_preserve(false);
}

/// Reload the candles for the active instrument. When `preserve_view` is set the
/// current zoom/scroll is kept and an empty response leaves the existing candles
/// untouched - used by the realtime ticker so a new bar does not snap the user
/// back to the default view (old app's `refreshCandles` kept the viewport).
fn load_chart_preserve(preserve_view: bool) {
    let (sec, exch, it, tf) = read_app(|a| {
        (
            a.sec_id,
            a.exch.clone(),
            a.inst_type.clone(),
            a.timeframe.clone(),
        )
    });
    // Claim this load. A newer load (symbol/timeframe switch) bumps the token, so
    // this request is abandoned the moment it is superseded - its candles must
    // never land on the chart that is now showing another instrument.
    let g = with_app_ret(|a| {
        a.chart_gen += 1;
        a.chart_gen
    });
    // A fresh (non-preserving) load is a different instrument/timeframe: drop the
    // old series right away so stale candles can't masquerade as the new symbol.
    if !preserve_view {
        with_app(|a| { a.raw.clear(); a.candles.clear(); });
    }
    set_html(
        "loading",
        "<div style='color:#00d4aa;font-size:12px;padding:20px'>Loading candles...</div>",
    );
    if let Some(l) = by_id("loading") {
        let _ = l.set_attribute("style", "display:block");
    }
    if !preserve_view {
        render_all();
    }
    spawn_local(async move {
        // Dhan hands back an empty series on a transient NotConnected / token
        // refresh / 429 (the old app retried too). Retry a few times with a
        // short backoff instead of blanking the chart permanently.
        let is_option = matches!(it.to_uppercase().as_str(), "OPTIDX" | "OPTSTK" | "OPTFUT" | "OPTCUR");
        let mut candles: Vec<Candle> = Vec::new();
        for attempt in 0..3 {
            if attempt > 0 {
                sleep_ms(1500).await;
                if read_app(|a| a.chart_gen) != g {
                    return;
                }
            }
            candles = fetch_candles(sec, &exch, &it, &tf).await;
            // Intraday history is unavailable for some contracts (MCX options, and
            // any option contract whose instrument class Dhan does not chart
            // intraday). Fall back to the daily series for every exchange - not just
            // MCX - and switch the UI to `day` instead of leaving an empty grid
            // (old app's `fetchOptCandles` / `_fetch_daily_with_fallback`).
            if candles.is_empty() && tf != "day" && (exch == "MCX_COMM" || is_option) {
                let day = fetch_candles(sec, &exch, &it, "day").await;
                if !day.is_empty() {
                    prepare_chart_timeframe("day");
                    candles = day;
                }
            }
            if !candles.is_empty() {
                break;
            }
        }
        if read_app(|a| a.chart_gen) != g {
            return;
        }
        // A failed refresh must never blank a chart that is already on screen.
        if candles.is_empty() {
            if preserve_view {
                return;
            }
            with_app(|a| { a.raw.clear(); a.candles.clear(); });
            set_html(
                "loading",
                "<div style='color:#ef5350;font-size:12px;padding:20px'>No candles returned. Tap Refresh Chart to retry.</div>",
            );
            render_all();
            return;
        }
        with_app(|app| {
            // Keep the viewport pinned to the newest bar across a live reload so
            // a rolling bar does not slide off the right edge.
            let old_n = app.candles.len() as f64;
            let anchored = old_n > 0.0 && app.view_start + app.view_count >= old_n - 1.0;
            app.raw = candles;
            app.rebuild_view();
            // Fresh history resets the in-memory roll state: the ticker will
            // roll onto the current bucket from here on (no more REST).
            let step = tf_step(&app.timeframe).max(1);
            app.last_bar = ((js_sys::Date::now() / 1000.0) as i64) / step;
            app.bar_vol_base = 0.0;
            app.local_bar = false;
            // A preserve load that lands on an empty chart has no viewport to
            // preserve: a fresh symbol/timeframe load cleared the candles and
            // this load superseded it (the 1s ticker fires one the instant the
            // timeframe changes). Without a fresh fit, the previous timeframe's
            // `view_start` - an index near the end of a *shorter* series - is
            // reused on the new, longer series and the chart snaps days back.
            if !preserve_view || old_n <= 0.0 {
                app.fit_recent();
            } else if anchored {
                let n = app.candles.len() as f64;
                app.view_start = (n - app.view_count).max(0.0);
            }
            app.recompute_all();
        });
        render_all();
    });
}

/// Quote-store key for the chart instrument; matches the server's `quote_key`.
fn chart_quote_key(exch: &str, sec_id: i64) -> String {
    match exch.to_uppercase().as_str() {
        "NSE" | "BSE" | "IDX_I" => format!("IDX_I:{}", sec_id),
        _ => sec_id.to_string(),
    }
}

/// Latest traded price (and cumulative day volume) for the active instrument,
/// straight from the chart's own websocket subscription.
fn live_chart_state() -> Option<(f64, f64)> {
    read_app(|a| {
        if a.live_ltp.is_finite() && a.live_ltp > 0.0 {
            Some((a.live_ltp, a.live_vol))
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Chart-owned live feed (independent of the sidebar)
// ---------------------------------------------------------------------------

/// DNS suffix / host for the push socket. Reuses the page's origin so the same
/// reverse proxy that serves the app also carries the websocket.
fn chart_ws_url() -> String {
    let loc = window().location();
    let proto = loc.protocol().unwrap_or_else(|_| "http:".into());
    let host = loc.host().unwrap_or_default();
    let scheme = if proto == "https:" { "wss" } else { "ws" };
    format!("{}://{}/ws", scheme, host)
}

fn chart_subscribe_payload() -> String {
    let (sec, exch) = read_app(|a| (a.sec_id, a.exch.clone()));
    serde_json::json!({
        "securities": [{ "security_id": sec, "exchange_segment": exch }]
    })
    .to_string()
}

/// Send the active instrument to the server so it joins the live feed. Called on
/// open and whenever the chart switches symbol/timeframe.
fn chart_send_subscription() {
    let ws = read_app(|a| a.ws.clone());
    if let Some(ws) = ws {
        if ws.ready_state() == web_sys::WebSocket::OPEN {
            let _ = ws.send_with_str(&chart_subscribe_payload());
        }
    }
}

/// Open (or reuse) the chart's own `/ws` connection and fold every incoming
/// tick into the forming candle.
pub fn chart_feed_connect() {
    if let Some(ws) = read_app(|a| a.ws.clone()) {
        if ws.ready_state() == web_sys::WebSocket::OPEN {
            chart_send_subscription();
            return;
        }
    }
    let url = chart_ws_url();
    let Ok(ws) = web_sys::WebSocket::new(&url) else {
        return;
    };
    ws.set_binary_type(web_sys::BinaryType::Arraybuffer);

    let onopen = Closure::<dyn FnMut()>::new(move || {
        with_app(|a| a.ws_retry = 0);
        chart_send_subscription();
    });
    ws.set_onopen(Some(onopen.as_ref().unchecked_ref()));
    onopen.forget();

    let onmessage = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |ev: web_sys::MessageEvent| {
        let Some(text) = ev.data().as_string() else {
            return;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("quotes") {
            return;
        }
        let Some(data) = v.get("data").and_then(|d| d.as_object()) else {
            return;
        };
        let (sec, exch) = read_app(|a| (a.sec_id, a.exch.clone()));
        let key = chart_quote_key(&exch, sec);
        let Some(q) = data.get(&key) else {
            return;
        };
        let ltp = q.get("ltp").and_then(|x| x.as_f64()).unwrap_or(0.0);
        if !ltp.is_finite() || ltp <= 0.0 {
            return;
        }
        let vol = q.get("volume").and_then(|x| x.as_f64()).unwrap_or(0.0);
        with_app(|a| {
            a.live_ltp = ltp;
            a.live_vol = if vol.is_finite() { vol } else { 0.0 };
        });
        if read_app(|a| a.realtime) {
            apply_live_tick();
        }
    });
    ws.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let onclose = Closure::<dyn FnMut()>::new(move || {
        with_app(|a| a.ws = None);
        let retry = read_app(|a| a.ws_retry);
        with_app(|a| a.ws_retry = (a.ws_retry + 1).min(6));
        let delay = 500u32 * 2u32.pow(retry.min(4));
        let cb = Closure::<dyn FnMut()>::new(move || {
            chart_feed_connect();
        });
        window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                cb.as_ref().unchecked_ref(),
                delay as i32,
            )
            .ok();
        cb.forget();
    });
    ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
    onclose.forget();

    with_app(|a| a.ws = Some(ws));
}

/// Start a new forming candle on the current wall-clock bucket without a broker
/// round-trip, using the live feed only. The bar opens at the previous close
/// (the exchange's first print then moves it) and its volume is measured as a
/// delta of the feed's cumulative day volume.
fn roll_live_bar(bar_start: i64, cum_vol: f64) -> bool {
    with_app_ret(|app| {
        let Some(last) = app.raw.last() else {
            return false;
        };
        let open = if last.close > 0.0 { last.close } else { last.open };
        if open <= 0.0 {
            return false;
        }
        // Candle timestamps are IST wall clock encoded as naive UTC, matching
        // the server's history rows.
        let t = bar_start + 19800;
        if last.time >= t {
            return false;
        }
        // Remember whether the view was pinned to the newest bar (the default
        // right-edge position). A chart that opened on a contract with only a
        // bar or two (`fit_recent` then sets `view_count` to that tiny count)
        // must grow that window as live bars roll in, otherwise the viewport
        // stays zoomed onto a single giant candle.
        let old_n = app.candles.len() as f64;
        let anchored = app.view_start + app.view_count >= old_n - 1.0;
        app.raw.push(Candle {
            time: t,
            open,
            high: open,
            low: open,
            close: open,
            volume: 0.0,
        });
        app.rebuild_view();
        let new_n = app.candles.len() as f64;
        if anchored {
            if app.view_count + 0.5 >= old_n {
                app.view_count = app.view_count.max(120.0_f64.min(new_n));
            }
            app.view_start = (new_n - app.view_count).max(0.0);
        }
        app.bar_vol_base = cum_vol;
        app.local_bar = true;
        app.recompute_all();
        true
    })
}

/// Fold the live traded price into the trailing forming bar so the candle and
/// any indicator series track the tape between broker fetches (old app's
/// `StratEngine.liveBar`). Rolls the bar onto the current bucket in-memory; no
/// history request is made after the initial load.
fn apply_live_tick() -> bool {
    let Some((ltp, cum_vol)) = live_chart_state() else {
        return false;
    };
    let (tf, last_bar) = read_app(|a| (a.timeframe.clone(), a.last_bar));
    let step = tf_step(&tf);
    let bar = (js_sys::Date::now() / 1000.0) as i64 / step.max(1);
    if is_intraday(&tf) && step > 0 && last_bar != 0 && bar != last_bar {
        roll_live_bar(bar * step, cum_vol);
        with_app(|a| a.last_bar = bar);
    }
    let changed = with_app_ret(|app| {
        let Some(last) = app.raw.last_mut() else {
            return false;
        };
        let mut ch = false;
        if (last.close - ltp).abs() > f64::EPSILON {
            last.close = ltp;
            ch = true;
        }
        if ltp > last.high {
            last.high = ltp;
            ch = true;
        }
        if last.low <= 0.0 || ltp < last.low {
            last.low = ltp;
            ch = true;
        }
        if app.local_bar {
            let v = (cum_vol - app.bar_vol_base).max(0.0);
            if (last.volume - v).abs() > f64::EPSILON {
                last.volume = v;
                ch = true;
            }
        }
        if ch {
            app.rebuild_view();
            app.recompute_all();
        }
        ch
    });
    if changed {
        render_all();
    }
    changed
}

// ---------------------------------------------------------------------------
// Layout / scales
// ---------------------------------------------------------------------------

const AXIS_W: f64 = 62.0;
const TIME_H: f64 = 22.0;
const TOP_PAD: f64 = 10.0;

struct Plot {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
    bar_w: f64,
}

fn plot_for(app: &App, w: f64, h: f64) -> Plot {
    let left = 4.0;
    let right = w - AXIS_W;
    let top = TOP_PAD;
    let bottom = h - TIME_H;
    Plot {
        left,
        right,
        top,
        bottom,
        bar_w: ((right - left) / bar_span(app)).min(60.0),
    }
}

fn x_for(plot: &Plot, app: &App, i: usize) -> f64 {
    plot.left + (i as f64 - app.view_start + 0.5) * plot.bar_w
}

fn bar_interval(app: &App) -> f64 {
    let n = app.candles.len();
    if n >= 2 {
        let d = app.candles[n - 1].time - app.candles[n - 2].time;
        if d > 0 {
            return d as f64;
        }
    }
    60.0
}

// Number of extra bar slots needed on the right so forward-projected series
// (Trend Projection, S/R reversals, Gann/Fib continuations) stay visible.
fn future_bars(app: &App) -> f64 {
    let n = app.candles.len();
    if n == 0 {
        return 0.0;
    }
    let last = app.candles[n - 1].time;
    let iv = bar_interval(app);
    let mut max_t = last;
    for inst in &app.insts {
        for s in &inst.series {
            for p in &s.data {
                if p.value.is_finite() && p.time > max_t {
                    max_t = p.time;
                }
            }
        }
    }
    (((max_t - last) as f64) / iv).ceil().clamp(0.0, 120.0)
}

fn bar_span(app: &App) -> f64 {
    (app.view_count.max(2.0) + future_bars(app)).max(2.0)
}

// Fractional candle index for an arbitrary time. Exact candle times map to the
// integer index, in-between times interpolate between the surrounding candles,
// and future/past times extrapolate using the current bar interval.
fn frac_index(app: &App, time: i64) -> f64 {
    let c = &app.candles;
    let n = c.len();
    if n == 0 {
        return 0.0;
    }
    match c.binary_search_by_key(&time, |x| x.time) {
        Ok(i) => i as f64,
        Err(pos) => {
            let iv = bar_interval(app).max(1.0);
            if pos == 0 {
                return (time - c[0].time) as f64 / iv;
            }
            if pos >= n {
                return (n - 1) as f64 + (time - c[n - 1].time) as f64 / iv;
            }
            let t0 = c[pos - 1].time;
            let t1 = c[pos].time;
            let span = (t1 - t0) as f64;
            if span > 0.0 {
                (pos - 1) as f64 + (time - t0) as f64 / span
            } else {
                (pos - 1) as f64
            }
        }
    }
}

fn x_for_time(app: &App, plot: &Plot, time: i64) -> f64 {
    plot.left + (frac_index(app, time) - app.view_start + 0.5) * plot.bar_w
}

/// Inverse of `frac_index`: candle time for a fractional bar index. Times past
/// the last candle extrapolate with the current bar interval, so a position box
/// can be dragged into the (empty) future area like on TradingView.
fn time_for_index(app: &App, fi: f64) -> i64 {
    let c = &app.candles;
    let n = c.len();
    if n == 0 {
        return 0;
    }
    let iv = bar_interval(app).max(1.0);
    if fi <= 0.0 {
        return c[0].time + (fi * iv).round() as i64;
    }
    let last = (n - 1) as f64;
    if fi >= last {
        return c[n - 1].time + ((fi - last) * iv).round() as i64;
    }
    let i = fi.floor();
    let f = fi - i;
    let a = c[i as usize].time;
    let b = c[i as usize + 1].time;
    a + ((b - a) as f64 * f).round() as i64
}

/// Inverse of `y_for`: price under a canvas y coordinate.
fn price_at_y(plot: &Plot, lo: f64, hi: f64, y: f64) -> f64 {
    let span = (plot.bottom - plot.top).max(1.0);
    let t = (plot.bottom - y) / span;
    lo + t * (hi - lo)
}

/// Fractional bar index under a canvas x coordinate.
fn index_at_x(plot: &Plot, app: &App, x: f64) -> f64 {
    (x - plot.left) / plot.bar_w.max(0.01) + app.view_start
}

fn visible_range(app: &App) -> (usize, usize) {
    let n = app.candles.len();
    if n == 0 {
        return (0, 0);
    }
    let a = app.view_start.floor().max(0.0) as usize;
    let b = (app.view_start + app.view_count).ceil() as usize;
    (a.min(n), b.min(n))
}

fn overlay_price_extent(app: &App, lo: &mut f64, hi: &mut f64) {
    if app.candles.is_empty() {
        return;
    }
    let a = app.view_start;
    let b = app.view_start + app.view_count + future_bars(app);
    for inst in &app.insts {
        if inst.kind != IndType::Overlay {
            continue;
        }
        for s in &inst.series {
            if s.price_scale_id.is_some() || s.exclude_autoscale {
                continue;
            }
            for p in &s.data {
                if !p.value.is_finite() {
                    continue;
                }
                let fi = frac_index(app, p.time);
                if fi < a - 0.5 || fi > b + 0.5 {
                    continue;
                }
                if p.value < *lo {
                    *lo = p.value;
                }
                if p.value > *hi {
                    *hi = p.value;
                }
            }
            // horizontal level lines participate in autoscale (old app's
            // createPriceLine does the same) so they stay on-screen.
            for pl in &s.price_lines {
                if pl.price.is_finite() {
                    if pl.price < *lo {
                        *lo = pl.price;
                    }
                    if pl.price > *hi {
                        *hi = pl.price;
                    }
                }
            }
        }
    }
    for pl in app.trade_lines.iter().chain(app.oc_lines.iter()) {
        if pl.price.is_finite() {
            if pl.price < *lo {
                *lo = pl.price;
            }
            if pl.price > *hi {
                *hi = pl.price;
            }
        }
    }
    // Long/Short Position levels stay on-screen too.
    for t in &app.positions {
        for v in [t.entry_price, t.target_price, t.stop_price] {
            if v.is_finite() {
                if v < *lo {
                    *lo = v;
                }
                if v > *hi {
                    *hi = v;
                }
            }
        }
    }
}

fn price_extent(app: &App, _plot: &Plot) -> (f64, f64) {
    let (a, b) = visible_range(app);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for c in app.candles.iter().take(b).skip(a) {
        lo = lo.min(c.low);
        hi = hi.max(c.high);
    }
    if !lo.is_finite() {
        lo = 0.0;
        hi = 1.0;
    }
    overlay_price_extent(app, &mut lo, &mut hi);
    let pad = (hi - lo) * 0.06;
    if pad == 0.0 {
        return (lo - 1.0, hi + 1.0);
    }
    (lo - pad, hi + pad)
}

fn y_for(plot: &Plot, lo: f64, hi: f64, v: f64) -> f64 {
    let t = if hi == lo { 0.5 } else { (v - lo) / (hi - lo) };
    plot.bottom - t * (plot.bottom - plot.top)
}

/// Bar indices that get a vertical time gridline / label, spaced so labels stay
/// readable at the current zoom (mirrors lightweight-charts' auto tick density).
fn visible_ticks(app: &App, plot: &Plot) -> Vec<usize> {
    let (a, b) = visible_range(app);
    if b <= a {
        return Vec::new();
    }
    let step = ((74.0 / plot.bar_w).ceil() as usize).max(1);
    let mut out = Vec::new();
    let mut i = ((a + step - 1) / step) * step;
    while i < b {
        out.push(i);
        i += step;
    }
    out
}

/// Vertical gridlines for the time axis (drawn under the candles).
fn draw_time_grid(ctx: &CanvasRenderingContext2d, app: &App, plot: &Plot) {
    set_stroke(ctx, "#1a1a30");
    ctx.set_line_width(1.0);
    for i in visible_ticks(app, plot) {
        let x = x_for(plot, app, i);
        if x < plot.left || x > plot.right {
            continue;
        }
        ctx.begin_path();
        ctx.move_to(x, plot.top);
        ctx.line_to(x, plot.bottom);
        ctx.stroke();
    }
}

/// Bottom time-axis labels in IST: `HH:MM AM/PM` on intraday charts, with a
/// `DD-Mon` day marker printed at every new trading day so the timeline always
/// carries a date; `DD-MM-YY` on daily and slower timeframes.
fn draw_time_labels(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    intraday: bool,
    h: f64,
) {
    ctx.set_font("10px sans-serif");
    ctx.set_text_align("center");
    set_fill(ctx, "#8a8aa0");
    let mut prev_day: Option<i64> = None;
    for i in visible_ticks(app, plot) {
        let x = x_for(plot, app, i);
        if x < plot.left || x > plot.right {
            continue;
        }
        let t = app.candles[i].time;
        let day = t.div_euclid(86400);
        let label = if intraday {
            if prev_day != Some(day) {
                fmt_day(t)
            } else {
                fmt_time(t, true)
            }
        } else {
            fmt_time(t, false)
        };
        prev_day = Some(day);
        ctx.fill_text(&label, x, h - 7.0).ok();
    }
    ctx.set_text_align("start");
}

/// Last-traded-price line across the chart plus its right-axis price tag, the
/// lightweight-charts `priceLine` + `lastValue` the old app shows on the candle
/// series by default.
fn draw_last_price(ctx: &CanvasRenderingContext2d, app: &App, plot: &Plot, lo: f64, hi: f64) {
    let Some(last) = app.candles.last() else { return };
    let n = app.candles.len();
    let (a, b) = visible_range(app);
    if n == 0 || n - 1 < a || n - 1 >= b {
        return;
    }
    let color = if last.close >= last.open { "#00d4aa" } else { "#ff5252" };
    let y = y_for(plot, lo, hi, last.close);
    if !y.is_finite() {
        return;
    }
    set_stroke(ctx, color);
    ctx.set_line_width(1.0);
    let dash = Array::new();
    dash.push(&JsValue::from_f64(4.0));
    dash.push(&JsValue::from_f64(4.0));
    ctx.set_line_dash(&dash).ok();
    ctx.begin_path();
    ctx.move_to(plot.left, y);
    ctx.line_to(plot.right, y);
    ctx.stroke();
    let empty = Array::new();
    ctx.set_line_dash(&empty).ok();

    set_fill(ctx, color);
    ctx.fill_rect(plot.right, y - 8.0, AXIS_W, 16.0);
    set_fill(ctx, "#0b0b1a");
    ctx.set_font("10px sans-serif");
    ctx.fill_text(&format!("{:.2}", last.close), plot.right + 5.0, y + 3.5)
        .ok();
}

/// Apply a lightweight-charts line style (1 solid, 2 dotted, 3 dashed, 4 large
/// dashed) as a canvas dash pattern.
fn apply_line_style(ctx: &CanvasRenderingContext2d, style: i32) {
    let arr = Array::new();
    match style {
        2 => {
            arr.push(&JsValue::from_f64(1.0));
            arr.push(&JsValue::from_f64(3.0));
        }
        3 => {
            arr.push(&JsValue::from_f64(6.0));
            arr.push(&JsValue::from_f64(6.0));
        }
        4 => {
            arr.push(&JsValue::from_f64(10.0));
            arr.push(&JsValue::from_f64(6.0));
        }
        _ => {}
    }
    let _ = ctx.set_line_dash(&arr);
}

/// Draw one horizontal price line with a right-axis price tag (the old app's
/// `createPriceLine` + `axisLabelVisible`).
fn draw_one_price_line(
    ctx: &CanvasRenderingContext2d,
    plot: &Plot,
    y: f64,
    price: f64,
    pl: &PriceLine,
    title_top: Option<f64>,
) {
    if !y.is_finite() || y < plot.top || y > plot.bottom {
        return;
    }
    set_stroke(ctx, &pl.color);
    ctx.set_line_width(pl.line_width.max(1.0));
    apply_line_style(ctx, pl.line_style);
    ctx.begin_path();
    ctx.move_to(plot.left, y);
    ctx.line_to(plot.right, y);
    ctx.stroke();
    let empty = Array::new();
    let _ = ctx.set_line_dash(&empty);

    // Right-axis price tag: white plate with the level's colour as a left accent
    // strip so the value stays readable on the dark theme.
    set_fill(ctx, "#ffffff");
    ctx.fill_rect(plot.right, y - 8.0, AXIS_W, 16.0);
    set_fill(ctx, &pl.color);
    ctx.fill_rect(plot.right, y - 8.0, 3.0, 16.0);
    set_fill(ctx, "#0b0b1a");
    ctx.set_font("10px sans-serif");
    ctx.fill_text(&fmt_val(price, None), plot.right + 6.0, y + 3.5).ok();

    // Per-level title box (running-P&L readout / SL / trail-SL / point-SL /
    // overall-SL / target). Each level gets its own WHITE plate anchored to the
    // right edge of the plot - i.e. pinned to the newest candle - so the labels
    // sit to the right of the candles and move along with them, with the level's
    // colour kept as a left accent strip.
    if let Some(top) = title_top {
        if !pl.title.is_empty() {
            ctx.set_font("10px sans-serif");
            let w = pl.title.chars().count() as f64 * 6.2 + 12.0;
            let ly = top.min(plot.bottom - 15.0).max(plot.top + 1.0);
            let x = (plot.right - w - 4.0).max(plot.left + 3.0);
            set_fill(ctx, "#ffffff");
            ctx.fill_rect(x, ly, w, 14.0);
            set_fill(ctx, &pl.color);
            ctx.fill_rect(x, ly, 4.0, 14.0);
            set_fill(ctx, "#0b0b1a");
            ctx.fill_text(&pl.title, x + 8.0, ly + 10.5).ok();
        }
    }
}

/// All price lines produced by overlay indicator series (S/R, Fibonacci/Gann
/// grids, Wave retracement, key levels, ...). Band-mapped series (PCR) are
/// skipped here and handled by `draw_band_price_lines`.
fn draw_series_price_lines(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    lo: f64,
    hi: f64,
) {
    for inst in &app.insts {
        if inst.kind != IndType::Overlay {
            continue;
        }
        for s in &inst.series {
            if s.price_scale_id.is_some() {
                continue;
            }
            for pl in &s.price_lines {
                let y = y_for(plot, lo, hi, pl.price);
                draw_one_price_line(ctx, plot, y, pl.price, pl, None);
            }
        }
    }
}

/// Price lines attached to a band-mapped series (PCR's 1.0 line), mapped into
/// the bottom band the band series is compressed into.
fn draw_band_price_lines(ctx: &CanvasRenderingContext2d, app: &App, plot: &Plot) {
    let band_top = plot.top + (plot.bottom - plot.top) * 0.78;
    let band_bottom = plot.bottom;
    for inst in &app.insts {
        if inst.kind != IndType::Overlay {
            continue;
        }
        for s in &inst.series {
            if s.price_scale_id.is_none() || s.price_lines.is_empty() {
                continue;
            }
            let mut lo = f64::INFINITY;
            let mut hi = f64::NEG_INFINITY;
            for p in &s.data {
                if p.value.is_finite() {
                    lo = lo.min(p.value);
                    hi = hi.max(p.value);
                }
            }
            if !lo.is_finite() {
                continue;
            }
            let bhi = hi.max(lo + f64::EPSILON);
            for pl in &s.price_lines {
                let t = (pl.price - lo) / (bhi - lo);
                let y = band_bottom - t * (band_bottom - band_top);
                draw_one_price_line(ctx, plot, y, pl.price, pl, None);
            }
        }
    }
}

/// Trading-level lines (strategies / paper trade) + option-chain level lines,
/// each in its own registry so they never wipe each other (old app's
/// `setTradeLines` / `setOcLevelLines`).
fn draw_extra_price_lines(ctx: &CanvasRenderingContext2d, app: &App, plot: &Plot, lo: f64, hi: f64) {
    // Trade/OC levels are drawn top-to-bottom with their title boxes stacked so
    // closely spaced levels (running P&L beside a trail SL, point SL beside the
    // overall floor) never overlap into one unreadable block.
    let mut items: Vec<(f64, &PriceLine)> = Vec::new();
    for pl in &app.trade_lines {
        items.push((y_for(plot, lo, hi, pl.price), pl));
    }
    for pl in &app.oc_lines {
        items.push((y_for(plot, lo, hi, pl.price), pl));
    }
    items.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut cursor = f64::NEG_INFINITY;
    for (y, pl) in items {
        let mut top = (y - 15.0).max(plot.top + 1.0);
        if top < cursor {
            top = cursor;
        }
        cursor = top + 15.0;
        draw_one_price_line(ctx, plot, y, pl.price, pl, Some(top));
    }
}

/// Direction-state overlay line + trend arrows owned by the OI Trend module.
fn draw_dir_overlay(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    lo: f64,
    hi: f64,
) {
    let Some(d) = &app.dir_overlay else { return };
    if d.data.is_empty() {
        return;
    }
    ctx.set_line_width(d.line_width.max(1.0));
    set_stroke(ctx, &d.color);
    ctx.begin_path();
    let mut started = false;
    for p in &d.data {
        if !p.value.is_finite() {
            if started {
                ctx.stroke();
                started = false;
            }
            continue;
        }
        let x = x_for_time(app, plot, p.time);
        let y = y_for(plot, lo, hi, p.value);
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        if !started {
            ctx.move_to(x, y);
            started = true;
        } else {
            ctx.line_to(x, y);
        }
    }
    if started {
        ctx.stroke();
    }
    draw_markers(ctx, app, plot, lo, hi, &d.markers);
}

// ---------------------------------------------------------------------------
// Long / Short Position tools (TradingView-style risk/reward boxes)
// ---------------------------------------------------------------------------

const POS_PROFIT: &str = "#00d4aa";
const POS_LOSS: &str = "#ff5252";
const POS_ENTRY: &str = "#c0c0d0";

fn pos_label(dir: PosDir) -> &'static str {
    match dir {
        PosDir::Long => "LONG",
        PosDir::Short => "SHORT",
    }
}

fn dir_str(d: PosDir) -> &'static str {
    match d {
        PosDir::Long => "long",
        PosDir::Short => "short",
    }
}

fn dir_from_str(s: &str) -> PosDir {
    if s.eq_ignore_ascii_case("short") {
        PosDir::Short
    } else {
        PosDir::Long
    }
}

/// Resolve the profit/stop levels for a drag from `entry` to `cur`. The level
/// nearest the cursor tracks the cursor (so the box follows the mouse instead of
/// appearing inverted), and the opposite level mirrors it for a 1:1 risk/reward
/// while drawing. Long keeps profit above / stop below, Short the reverse.
/// Returns `(target, stop)`.
fn pos_levels(dir: PosDir, entry: f64, cur: f64) -> (f64, f64) {
    let d = cur - entry;
    match dir {
        // Cursor above entry -> it is the target; below -> it is the stop.
        PosDir::Long => {
            if d >= 0.0 {
                (entry + d, entry - d)
            } else {
                (entry - d, entry + d)
            }
        }
        // Cursor below entry -> it is the target; above -> it is the stop.
        PosDir::Short => {
            if d <= 0.0 {
                (entry + d, entry - d)
            } else {
                (entry - d, entry + d)
            }
        }
    }
}

fn pos_to_json(t: &PositionTool) -> Value {
    json!({
        "dir": dir_str(t.dir),
        "entryTime": t.entry_time,
        "endTime": t.end_time,
        "entryPrice": t.entry_price,
        "targetPrice": t.target_price,
        "stopPrice": t.stop_price,
    })
}

fn pos_from_json(v: &Value) -> Option<PositionTool> {
    let entry_time = v.get("entryTime").and_then(|x| x.as_i64())?;
    let end_time = v
        .get("endTime")
        .and_then(|x| x.as_i64())
        .filter(|t| *t != 0)
        .unwrap_or(entry_time);
    Some(PositionTool {
        dir: dir_from_str(v.get("dir").and_then(|x| x.as_str()).unwrap_or("long")),
        entry_time,
        end_time,
        entry_price: v.get("entryPrice").and_then(|x| x.as_f64())?,
        target_price: v.get("targetPrice").and_then(|x| x.as_f64())?,
        stop_price: v.get("stopPrice").and_then(|x| x.as_f64())?,
    })
}

/// Which part of a position is under (x, y), or None. Lines win over the body so
/// grabbing a level always resizes that level instead of moving the whole box.
fn pos_hit(app: &App, plot: &Plot, lo: f64, hi: f64, x: f64, y: f64) -> Option<(usize, PosEditKind)> {
    const TOL: f64 = 7.0;
    for (i, t) in app.positions.iter().enumerate().rev() {
        let x0 = x_for_time(app, plot, t.entry_time).clamp(plot.left, plot.right);
        let x1 = x_for_time(app, plot, t.end_time).clamp(plot.left, plot.right);
        let (xa, xb) = (x0.min(x1) - 4.0, x0.max(x1) + 4.0);
        if x < xa || x > xb {
            continue;
        }
        let y_e = y_for(plot, lo, hi, t.entry_price);
        let y_t = y_for(plot, lo, hi, t.target_price);
        let y_s = y_for(plot, lo, hi, t.stop_price);
        if (y - y_t).abs() <= TOL {
            return Some((i, PosEditKind::Target));
        }
        if (y - y_s).abs() <= TOL {
            return Some((i, PosEditKind::Stop));
        }
        if (y - y_e).abs() <= TOL {
            return Some((i, PosEditKind::Move));
        }
        let ymin = y_e.min(y_t).min(y_s);
        let ymax = y_e.max(y_t).max(y_s);
        if y > ymin && y < ymax {
            return Some((i, PosEditKind::Move));
        }
    }
    None
}

fn draw_pos_level(
    ctx: &CanvasRenderingContext2d,
    plot: &Plot,
    xa: f64,
    xb: f64,
    y: f64,
    color: &str,
    width: f64,
    style: i32,
) {
    if !y.is_finite() || y < plot.top || y > plot.bottom {
        return;
    }
    set_stroke(ctx, color);
    ctx.set_line_width(width);
    apply_line_style(ctx, style);
    ctx.begin_path();
    ctx.move_to(xa, y);
    ctx.line_to(xb.max(xa + 1.0), y);
    ctx.stroke();
    let empty = Array::new();
    let _ = ctx.set_line_dash(&empty);
}

/// Price with a fixed 2-decimal format (never abbreviated like `fmt_val`, so the
/// exact traded level is always legible).
fn pos_price(v: f64) -> String {
    format!("{:.2}", v)
}

fn measure_w(ctx: &CanvasRenderingContext2d, text: &str) -> f64 {
    ctx.measure_text(text)
        .map(|m| m.width())
        .unwrap_or_else(|_| text.chars().count() as f64 * 5.2)
}

fn draw_pos_axis_tag(ctx: &CanvasRenderingContext2d, plot: &Plot, y: f64, price: f64, color: &str) {
    if !y.is_finite() || y < plot.top || y > plot.bottom {
        return;
    }
    set_fill(ctx, color);
    ctx.fill_rect(plot.right, y - 7.0, AXIS_W, 14.0);
    set_fill(ctx, "#0b0b1a");
    ctx.set_font("10px sans-serif");
    ctx.fill_text(&pos_price(price), plot.right + 5.0, y + 3.0).ok();
}

/// TradingView-style level caption: a small dark pill with a colored accent bar,
/// right-aligned inside the box. Carries the level name plus points and percent.
fn draw_pos_caption(
    ctx: &CanvasRenderingContext2d,
    plot: &Plot,
    x_right: f64,
    y: f64,
    text: &str,
    accent: &str,
) {
    if !y.is_finite() {
        return;
    }
    ctx.set_font("9px sans-serif");
    let w = measure_w(ctx, text) + 16.0;
    let h = 14.0;
    let x = (x_right - w).clamp(plot.left + 2.0, (plot.right - w - 1.0).max(plot.left + 2.0));
    let y0 = (y - h / 2.0).clamp(plot.top + 1.0, (plot.bottom - h - 1.0).max(plot.top + 1.0));
    set_fill(ctx, "rgba(11,11,26,0.88)");
    ctx.fill_rect(x, y0, w, h);
    set_fill(ctx, accent);
    ctx.fill_rect(x, y0, 3.0, h);
    set_stroke(ctx, accent);
    ctx.set_line_width(1.0);
    let empty = Array::new();
    let _ = ctx.set_line_dash(&empty);
    ctx.stroke_rect(x + 0.5, y0 + 0.5, w - 1.0, h - 1.0);
    set_fill(ctx, "#e8e8f2");
    ctx.fill_text(text, x + 8.0, y0 + 10.0).ok();
}

/// Paint one position tool TradingView-fashion: shaded profit (green) and loss
/// (red) zones, entry/target/stop lines, right-axis price tags, and per-level
/// captions showing price, points and percent plus the live risk/reward.
fn draw_position(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    lo: f64,
    hi: f64,
    t: &PositionTool,
) {
    let x0 = x_for_time(app, plot, t.entry_time).clamp(plot.left, plot.right);
    let x1 = x_for_time(app, plot, t.end_time).clamp(plot.left, plot.right);
    let xa = x0.min(x1);
    let xb = x0.max(x1);
    let bw = (xb - xa).max(2.0);
    let y_e = y_for(plot, lo, hi, t.entry_price);
    let y_t = y_for(plot, lo, hi, t.target_price);
    let y_s = y_for(plot, lo, hi, t.stop_price);
    let cy = |y: f64| y.clamp(plot.top, plot.bottom);

    let entry = t.entry_price;
    let reward = (t.target_price - entry).abs();
    let risk = (t.stop_price - entry).abs();
    let reward_pct = if entry.abs() > 0.0 { reward / entry.abs() * 100.0 } else { 0.0 };
    let risk_pct = if entry.abs() > 0.0 { risk / entry.abs() * 100.0 } else { 0.0 };
    let rr = if risk > 0.0 { reward / risk } else { 0.0 };

    // profit zone (entry -> target) and loss zone (entry -> stop)
    let (pt, pb) = (cy(y_e).min(cy(y_t)), cy(y_e).max(cy(y_t)));
    set_fill(ctx, "rgba(0,212,170,0.14)");
    ctx.fill_rect(xa, pt, bw, (pb - pt).max(0.0));
    let (st, sb) = (cy(y_e).min(cy(y_s)), cy(y_e).max(cy(y_s)));
    set_fill(ctx, "rgba(255,82,82,0.14)");
    ctx.fill_rect(xa, st, bw, (sb - st).max(0.0));

    // outer border
    let bt = pt.min(st);
    let bb = pb.max(sb);
    set_stroke(ctx, "#4a4a72");
    ctx.set_line_width(1.0);
    let empty = Array::new();
    let _ = ctx.set_line_dash(&empty);
    ctx.stroke_rect(xa + 0.5, bt + 0.5, (bw - 1.0).max(1.0), (bb - bt - 1.0).max(1.0));

    // dashed vertical entry-time guide
    set_stroke(ctx, POS_ENTRY);
    let dash = Array::new();
    dash.push(&JsValue::from_f64(4.0));
    dash.push(&JsValue::from_f64(4.0));
    let _ = ctx.set_line_dash(&dash);
    ctx.begin_path();
    ctx.move_to(x0, plot.top);
    ctx.line_to(x0, plot.bottom);
    ctx.stroke();
    let _ = ctx.set_line_dash(&empty);

    // level lines
    draw_pos_level(ctx, plot, xa, xb, y_e, POS_ENTRY, 1.0, 2);
    draw_pos_level(ctx, plot, xa, xb, y_t, POS_PROFIT, 2.0, 0);
    draw_pos_level(ctx, plot, xa, xb, y_s, POS_LOSS, 2.0, 0);

    // right-axis price tags
    draw_pos_axis_tag(ctx, plot, y_t, t.target_price, POS_PROFIT);
    draw_pos_axis_tag(ctx, plot, y_s, t.stop_price, POS_LOSS);
    draw_pos_axis_tag(ctx, plot, y_e, t.entry_price, POS_ENTRY);

    // per-level captions with points + percent
    draw_pos_caption(
        ctx,
        plot,
        xb,
        y_t,
        &format!("Target {}  +{} (+{:.2}%)", pos_price(t.target_price), pos_price(reward), reward_pct),
        POS_PROFIT,
    );
    draw_pos_caption(
        ctx,
        plot,
        xb,
        y_s,
        &format!("Stop {}  -{} (-{:.2}%)", pos_price(t.stop_price), pos_price(risk), risk_pct),
        POS_LOSS,
    );
    draw_pos_caption(
        ctx,
        plot,
        xb,
        y_e,
        &format!("Entry {}   R:R 1:{:.2}", pos_price(entry), rr),
        POS_ENTRY,
    );

    // header: direction + profit read-out
    let title = format!("{}   +{} (+{:.2}%)", pos_label(t.dir), pos_price(reward), reward_pct);
    ctx.set_font("bold 10px sans-serif");
    let w = measure_w(ctx, &title) + 18.0;
    let lx = (xa + 4.0).min((plot.right - w - 2.0).max(plot.left + 2.0)).max(plot.left + 2.0);
    let ly = (bt + 4.0).min((plot.bottom - 17.0).max(plot.top + 1.0)).max(plot.top + 1.0);
    set_fill(ctx, "#ffffff");
    ctx.fill_rect(lx, ly, w, 15.0);
    set_fill(ctx, if t.dir == PosDir::Long { POS_PROFIT } else { POS_LOSS });
    ctx.fill_rect(lx, ly, 4.0, 15.0);
    set_fill(ctx, "#0b0b1a");
    ctx.fill_text(&title, lx + 9.0, ly + 11.0).ok();
}

fn draw_positions(ctx: &CanvasRenderingContext2d, app: &App, plot: &Plot, lo: f64, hi: f64) {
    for t in &app.positions {
        draw_position(ctx, app, plot, lo, hi, t);
    }
    if let Some(d) = &app.pos_drag {
        // While dragging, show the live target/SL so the auto-mirrored level is
        // visible before the mouse is released. The cursor-side edge tracks the
        // cursor, so the box grows with the mouse instead of feeling inverted.
        let (target, stop) = pos_levels(d.dir, d.entry_price, d.cur_price);
        let t = PositionTool {
            dir: d.dir,
            entry_time: d.entry_time,
            end_time: d.cur_time,
            entry_price: d.entry_price,
            target_price: target,
            stop_price: stop,
        };
        draw_position(ctx, app, plot, lo, hi, &t);
    }
}

fn apply_pos_cursor(app: &App) {
    let c = if app.pos_tool.is_some() || app.pos_drag.is_some() {
        "crosshair"
    } else if app.pos_edit.is_some() {
        "grabbing"
    } else {
        match app.pos_hover {
            Some((_, PosEditKind::Target)) | Some((_, PosEditKind::Stop)) => "ns-resize",
            Some((_, PosEditKind::Move)) => "move",
            None => "default",
        }
    };
    if let Some(el) = by_id("chart-container").and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()) {
        let _ = el.style().set_property("cursor", c);
    }
}

fn update_pos_buttons() {
    let tool = read_app(|a| a.pos_tool);
    for (id, dir) in [("posLongBtn", PosDir::Long), ("posShortBtn", PosDir::Short)] {
        if let Some(el) = by_id(id) {
            el.set_class_name(if tool == Some(dir) { "pos-btn active" } else { "pos-btn" });
        }
    }
    if let Some(el) = by_id("posClearBtn") {
        let has = read_app(|a| !a.positions.is_empty());
        el.set_class_name(if has { "pos-btn clear" } else { "pos-btn clear dim" });
    }
}


// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn ctx_of(canvas: &HtmlCanvasElement) -> Option<CanvasRenderingContext2d> {
    canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|o| o.dyn_into::<CanvasRenderingContext2d>().ok())
}

fn prep_canvas(canvas: &HtmlCanvasElement, w: f64, h: f64) -> Option<CanvasRenderingContext2d> {
    let dpr = window().device_pixel_ratio().max(1.0);
    canvas.set_width((w * dpr) as u32);
    canvas.set_height((h * dpr) as u32);
    canvas.set_attribute("style", &format!("width:{}px;height:{}px;display:block", w, h)).ok();
    let ctx = ctx_of(canvas)?;
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    Some(ctx)
}

fn draw_main() {
    read_app(|app| {
        let container = match by_id("chart-container") {
            Some(c) => c,
            None => return,
        };
        let w = container.client_width().max(320) as f64;
        let h = container.client_height().max(240) as f64;
        let canvas = match by_id("chartCanvas").and_then(|e| e.dyn_into::<HtmlCanvasElement>().ok()) {
            Some(c) => c,
            None => return,
        };
        let ctx = match prep_canvas(&canvas, w, h) {
            Some(c) => c,
            None => return,
        };
        let plot = plot_for(app, w, h);
        let (lo, hi) = price_extent(app, &plot);
        let intraday = is_intraday(&app.timeframe);

        // background
        set_fill(&ctx, "#0b0b1a");
        ctx.fill_rect(0.0, 0.0, w, h);

        if app.candles.is_empty() {
            set_fill(&ctx, "#666");
            ctx.set_font("12px sans-serif");
            ctx.fill_text("Connect to load chart", 16.0, 28.0).ok();
            return;
        }

        // grid + price labels
        ctx.set_line_width(1.0);
        set_stroke(&ctx, "#1a1a30");
        for k in 0..=5 {
            let y = plot.top + (plot.bottom - plot.top) * k as f64 / 5.0;
            ctx.begin_path();
            ctx.move_to(plot.left, y);
            ctx.line_to(plot.right, y);
            ctx.stroke();
            let val = hi - (hi - lo) * k as f64 / 5.0;
            set_fill(&ctx, "#8a8aa0");
            ctx.set_font("10px sans-serif");
            ctx.fill_text(&format!("{:.2}", val), plot.right + 6.0, y + 3.0).ok();
        }

        // vertical time gridlines (behind the candles)
        draw_time_grid(&ctx, app, &plot);

        // volume band (bottom 20%)
        let vol_top = plot.top + (plot.bottom - plot.top) * 0.8;
        let max_vol = app
            .candles
            .iter()
            .take(visible_range(app).1)
            .skip(visible_range(app).0)
            .map(|c| c.volume)
            .fold(0.0f64, f64::max)
            .max(1.0);
        for (i, c) in app.candles.iter().enumerate() {
            let (a, b) = visible_range(app);
            if i < a || i >= b {
                continue;
            }
            let x = x_for(&plot, app, i);
            let bw = (plot.bar_w * 0.7).max(1.0);
            let bh = (c.volume / max_vol) * (plot.bottom - vol_top);
            set_fill(&ctx, if c.close >= c.open { "#1f6f57" } else { "#7a2c2c" });
            ctx.fill_rect(x - bw / 2.0, plot.bottom - bh, bw, bh);
        }

        // axes lines
        set_stroke(&ctx, "#2d2d50");
        ctx.begin_path();
        ctx.move_to(plot.right, plot.top);
        ctx.line_to(plot.right, plot.bottom);
        ctx.move_to(plot.left, plot.bottom);
        ctx.line_to(plot.right, plot.bottom);
        ctx.stroke();

        // price series: line chart, or candles / Heikin-Ashi / Renko bars
        let (a, b) = visible_range(app);
        if app.chart_kind == ChartKind::Line {
            if b > a {
                set_stroke(&ctx, "#4fc3f7");
                ctx.set_line_width(1.5);
                ctx.begin_path();
                for i in a..b {
                    let x = x_for(&plot, app, i);
                    let y = y_for(&plot, lo, hi, app.candles[i].close);
                    if i == a {
                        ctx.move_to(x, y);
                    } else {
                        ctx.line_to(x, y);
                    }
                }
                ctx.stroke();
            }
        } else {
            for i in a..b {
                let c = app.candles[i];
                let x = x_for(&plot, app, i);
                let color = if c.close >= c.open { "#00d4aa" } else { "#ff5252" };
                set_stroke(&ctx, color);
                set_fill(&ctx, color);
                ctx.set_line_width(1.0);
                ctx.begin_path();
                ctx.move_to(x, y_for(&plot, lo, hi, c.high));
                ctx.line_to(x, y_for(&plot, lo, hi, c.low));
                ctx.stroke();
                let y_open = y_for(&plot, lo, hi, c.open);
                let y_close = y_for(&plot, lo, hi, c.close);
                let top = y_open.min(y_close);
                let bh = (y_open - y_close).abs().max(1.0);
                let bw = (plot.bar_w * 0.7).max(1.0);
                ctx.fill_rect(x - bw / 2.0, top, bw, bh);
            }
        }

        // overlay indicators
        for inst in &app.insts {
            if inst.kind != IndType::Overlay {
                continue;
            }
            for s in &inst.series {
                if s.price_scale_id.is_some() {
                    draw_band_series(&ctx, app, &plot, s, &inst.format);
                } else {
                    draw_price_series(&ctx, app, &plot, lo, hi, s);
                }
            }
        }

        // indicator-emitted horizontal price lines (S/R, Fib/Gann grids, key
        // levels, wave retracement, ...) plus band-mapped lines (PCR).
        draw_series_price_lines(&ctx, app, &plot, lo, hi);
        draw_band_price_lines(&ctx, app, &plot);
        // trading-level + option-chain level overlays
        draw_extra_price_lines(&ctx, app, &plot, lo, hi);
        // OI Trend direction-state overlay (JS-owned)
        draw_dir_overlay(&ctx, app, &plot, lo, hi);

        // marker arrows
        for inst in &app.insts {
            draw_markers(&ctx, app, &plot, lo, hi, &inst.markers);
        }

        // Long/Short Position tools (drawn above candle overlays, below crosshair)
        draw_positions(&ctx, app, &plot, lo, hi);

        // last price line + right-axis tag, then the bottom time labels
        draw_last_price(&ctx, app, &plot, lo, hi);
        draw_time_labels(&ctx, app, &plot, intraday, h);

        // crosshair: the vertical line follows the shared hovered bar on the
        // main chart, the horizontal line only while the main chart is hovered.
        if let Some(idx) = app.cross_idx.filter(|i| *i < app.candles.len()) {
            let vx = x_for(&plot, app, idx);
            set_stroke(&ctx, "#5a5a80");
            let dash = Array::new();
            dash.push(&JsValue::from_f64(4.0));
            dash.push(&JsValue::from_f64(4.0));
            ctx.set_line_dash(&dash).ok();
            ctx.begin_path();
            if let Some((_cx, cy)) = app.cross {
                ctx.move_to(plot.left, cy);
                ctx.line_to(plot.right, cy);
            }
            ctx.move_to(vx, plot.top);
            ctx.line_to(vx, plot.bottom);
            ctx.stroke();
            let empty = Array::new();
            ctx.set_line_dash(&empty).ok();

            // OHLC tooltip (only while the pointer is over the main chart)
            if app.cross.is_some() {
                let c = app.candles[idx];
                set_fill(&ctx, "#20203a");
                ctx.fill_rect(plot.left + 6.0, plot.top + 4.0, 184.0, 58.0);
                set_fill(&ctx, "#d0d0d0");
                ctx.set_font("11px monospace");
                let lines = [
                    format!("O {:.2}   H {:.2}", c.open, c.high),
                    format!("L {:.2}   C {:.2}", c.low, c.close),
                    format!("V {:.0}   {}", c.volume, fmt_datetime(c.time, intraday)),
                ];
                for (k, line) in lines.iter().enumerate() {
                    ctx.fill_text(line, plot.left + 12.0, plot.top + 20.0 + k as f64 * 15.0)
                        .ok();
                }
            }
        }
    });
}

fn draw_price_series(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    lo: f64,
    hi: f64,
    s: &SeriesOut,
) {
    if s.kind == SeriesKind::Histogram {
        let color = s.color.clone().unwrap_or_else(|| "#888".into());
        for p in &s.data {
            if !p.value.is_finite() {
                continue;
            }
            let x = x_for_time(app, plot, p.time);
            let y = y_for(plot, lo, hi, p.value);
            let y0 = y_for(plot, lo, hi, 0.0);
            set_fill(ctx, p.color.as_deref().unwrap_or(&color));
            let bw = (plot.bar_w * 0.6).max(1.0);
            ctx.fill_rect(x - bw / 2.0, y.min(y0), bw, (y - y0).abs().max(1.0));
        }
        return;
    }
    let color = s.color.clone().unwrap_or_else(|| "#888".into());
    let lw = s.line_width.unwrap_or(1.0);
    ctx.set_line_width(lw);
    let mut last: Option<(f64, f64)> = None;
    let mut run_color: Option<String> = None;
    let mut started = false;
    for p in &s.data {
        if !p.value.is_finite() {
            if started {
                ctx.stroke();
                started = false;
            }
            last = None;
            run_color = None;
            continue;
        }
        let x = x_for_time(app, plot, p.time);
        let y = y_for(plot, lo, hi, p.value);
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        let col = p.color.clone().unwrap_or_else(|| color.clone());
        match last {
            None => {
                set_stroke(ctx, &col);
                ctx.begin_path();
                ctx.move_to(x, y);
                run_color = Some(col);
                started = true;
            }
            Some((px, py)) => {
                if run_color.as_deref() != Some(col.as_str()) {
                    if started {
                        ctx.stroke();
                    }
                    set_stroke(ctx, &col);
                    ctx.begin_path();
                    ctx.move_to(px, py);
                    ctx.line_to(x, y);
                    run_color = Some(col);
                    started = true;
                } else {
                    ctx.line_to(x, y);
                }
            }
        }
        last = Some((x, y));
    }
    if started {
        ctx.stroke();
    }
}

fn draw_band_series(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    s: &SeriesOut,
    _format: &Option<String>,
) {
    let a = app.view_start;
    let b = app.view_start + app.view_count + future_bars(app);
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for p in &s.data {
        if !p.value.is_finite() {
            continue;
        }
        let fi = frac_index(app, p.time);
        if fi < a - 0.5 || fi > b + 0.5 {
            continue;
        }
        lo = lo.min(p.value);
        hi = hi.max(p.value);
    }
    if !lo.is_finite() {
        return;
    }
    let band_top = plot.top + (plot.bottom - plot.top) * 0.78;
    let band_bottom = plot.bottom;
    let (blo, bhi) = (lo, hi.max(lo + f64::EPSILON));
    let color = s.color.clone().unwrap_or_else(|| "#888".into());
    ctx.set_line_width(s.line_width.unwrap_or(1.0));
    set_stroke(ctx, &color);
    ctx.begin_path();
    let mut started = false;
    for p in &s.data {
        if !p.value.is_finite() {
            continue;
        }
        let x = x_for_time(app, plot, p.time);
        let t = (p.value - blo) / (bhi - blo);
        let y = band_bottom - t * (band_bottom - band_top);
        if !started {
            ctx.move_to(x, y);
            started = true;
        } else {
            ctx.line_to(x, y);
        }
    }
    ctx.stroke();
}

fn draw_markers(
    ctx: &CanvasRenderingContext2d,
    app: &App,
    plot: &Plot,
    lo: f64,
    hi: f64,
    markers: &[Marker],
) {
    for m in markers {
        let i = match idx_of_time(app, m.time) {
            Some(i) => i,
            None => continue,
        };
        let Some(c) = app.candles.get(i) else {
            continue;
        };
        let x = x_for_time(app, plot, m.time);
        let above = m.position == "aboveBar";
        let anchor = if above { c.high } else { c.low };
        let y = if above {
            y_for(plot, lo, hi, anchor) - 9.0
        } else {
            y_for(plot, lo, hi, anchor) + 9.0
        };
        set_fill(ctx, &m.color);
        ctx.begin_path();
        match m.shape.as_str() {
            "circle" => {
                let _ = ctx.arc(x, y, 3.0, 0.0, std::f64::consts::PI * 2.0);
            }
            "arrowUp" => {
                ctx.move_to(x, y);
                ctx.line_to(x - 5.0, y + 8.0);
                ctx.line_to(x, y + 5.5);
                ctx.line_to(x + 5.0, y + 8.0);
            }
            "arrowDown" => {
                ctx.move_to(x, y);
                ctx.line_to(x - 5.0, y - 8.0);
                ctx.line_to(x, y - 5.5);
                ctx.line_to(x + 5.0, y - 8.0);
            }
            _ => {
                if above {
                    ctx.move_to(x, y);
                    ctx.line_to(x - 4.0, y + 7.0);
                    ctx.line_to(x + 4.0, y + 7.0);
                } else {
                    ctx.move_to(x, y);
                    ctx.line_to(x - 4.0, y - 7.0);
                    ctx.line_to(x + 4.0, y - 7.0);
                }
            }
        }
        ctx.close_path();
        ctx.fill();

        if !m.text.is_empty() {
            set_fill(ctx, &m.color);
            let px = (11.0 * m.size.max(0.85)).round();
            ctx.set_font(&format!(
                "600 {px}px -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif"
            ));
            ctx.set_text_align("center");
            let ty = if above { y - 4.0 } else { y + 14.0 };
            let _ = ctx.fill_text(&m.text, x, ty);
        }
    }
}

fn idx_of_time(app: &App, time: i64) -> Option<usize> {
    app.candles.binary_search_by_key(&time, |c| c.time).ok()
}

// ---------------------------------------------------------------------------
// Panes
// ---------------------------------------------------------------------------

fn ensure_panes() {
    let panes: Vec<(u64, String, String)> = read_app(|app| {
        app.insts
            .iter()
            .filter(|i| i.kind == IndType::Pane)
            .map(|i| (i.uid, i.name.clone(), i.id.clone()))
            .collect()
    });
    let host = match by_id("ind-panes") {
        Some(h) => h,
        None => return,
    };
    // remove stale
    let stale: Vec<u64> = read_app(|app| {
        app.pane_canvas
            .iter()
            .map(|(u, _)| *u)
            .filter(|u| !panes.iter().any(|(p, _, _)| p == u))
            .collect()
    });
    for u in stale {
        if let Some(el) = by_id(&format!("pane-{}", u)) {
            let _ = host.remove_child(&el);
        }
        with_app(|app| app.pane_canvas.retain(|(x, _)| *x != u));
    }
    // add new
    for (uid, name, id) in &panes {
        if by_id(&format!("pane-{}", *uid)).is_some() {
            continue;
        }
        // drop the "no panes" placeholder the first time a pane is added
        if let Ok(empty) = host.query_selector_all(".ind-empty") {
            for k in 0..empty.length() {
                if let Some(n) = empty.get(k) {
                    if let Ok(el) = n.dyn_into::<Element>() {
                        let _ = host.remove_child(&el);
                    }
                }
            }
        }
        let doc = document();
        let boxel = doc.create_element("div").unwrap();
        boxel.set_attribute("class", "pane-box").ok();
        boxel.set_attribute("id", &format!("pane-{}", uid)).ok();
        boxel.set_attribute("data-uid", &uid.to_string()).ok();

        // ---- head ----
        let head = doc.create_element("div").unwrap();
        head.set_attribute("class", "pane-head").ok();
        let nm = doc.create_element("span").unwrap();
        nm.set_attribute("class", "pane-name").ok();
        nm.set_text_content(Some(name));
        let read = doc.create_element("span").unwrap();
        read.set_attribute("class", "pane-read").ok();
        read.set_text_content(Some("--"));
        head.append_child(&nm).ok();
        head.append_child(&read).ok();
        // "+" alert-line button (BB%b only, same as the old app)
        if id == "bbpct" {
            let plus = doc.create_element("span").unwrap();
            plus.set_attribute("class", "pane-plus").ok();
            plus.set_inner_html(SVG_PLUS);
            plus.set_attribute("title", "Add alert line").ok();
            head.append_child(&plus).ok();
            let uid2 = *uid;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                toggle_alert_box(uid2);
            });
            plus.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                .ok();
            cb.forget();
        }
        let gear = doc.create_element("span").unwrap();
        gear.set_attribute("class", "pane-gear").ok();
        gear.set_inner_html(SVG_GEAR);
        gear.set_attribute("title", "Settings").ok();
        head.append_child(&gear).ok();
        let close = doc.create_element("span").unwrap();
        close.set_attribute("class", "pane-close").ok();
        close.set_inner_html(SVG_X);
        close.set_attribute("title", "Remove").ok();
        head.append_child(&close).ok();
        boxel.append_child(&head).ok();

        // name / gear -> settings
        {
            let uid2 = *uid;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                open_settings(uid2);
            });
            for el in [&nm, &gear] {
                el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                    .ok();
            }
            cb.forget();
        }
        // close -> remove
        {
            let uid2 = *uid;
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                with_app(|app| app.remove(uid2));
                persist_indicators();
                render_all();
            });
            close
                .add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                .ok();
            cb.forget();
        }

        // ---- chart canvas ----
        let chart = doc.create_element("div").unwrap();
        chart.set_attribute("class", "pane-chart").ok();
        let canvas = doc
            .create_element("canvas")
            .unwrap()
            .dyn_into::<HtmlCanvasElement>()
            .unwrap();
        chart.append_child(&canvas).ok();
        boxel.append_child(&chart).ok();
        hook_pane_canvas(*uid, &canvas);

        // ---- alert box (hidden until "+") ----
        let ab = doc.create_element("div").unwrap();
        ab.set_attribute("class", "pane-alertbox hidden").ok();
        boxel.append_child(&ab).ok();

        host.append_child(&boxel).ok();
        with_app(|app| app.pane_canvas.push((*uid, canvas)));
    }
    if panes.is_empty() {
        set_html(
            "ind-panes",
            "<div class='ind-empty'>No pane indicators added. Use the Indicators menu to add one.</div>",
        );
    } else {
        let count = by_id("indSectionCount");
        if let Some(c) = count {
            c.set_text_content(Some(&panes.len().to_string()));
        }
    }
}

/// Crosshair sync: hovering a pane mirrors the hovered bar onto the main chart
/// (and every other pane) by driving the shared `cross_idx`.
fn hook_pane_canvas(uid: u64, canvas: &HtmlCanvasElement) {
    let cv = canvas.clone();
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
        let rect = cv.get_bounding_client_rect();
        let x = e.client_x() as f64 - rect.left();
        let y = e.client_y() as f64 - rect.top();
        let w = cv.client_width() as f64;
        let span = read_app(|a| bar_span(a));
        let bar_w = (w - AXIS_W - 4.0) / span.max(1.0);
        with_app(|app| {
            app.cross = None;
            app.pane_cross = Some((uid, x, y));
            let idx = ((x - 4.0) / bar_w.max(0.01) + app.view_start).round();
            if idx >= 0.0 && (idx as usize) < app.candles.len() {
                app.cross_idx = Some(idx as usize);
            }
        });
        render_all();
    });
    canvas
        .add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |_e: MouseEvent| {
        with_app(|app| {
            app.pane_cross = None;
            app.cross_idx = None;
        });
        render_all();
    });
    canvas
        .add_event_listener_with_callback("mouseleave", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();
}

fn draw_panes() {
    let pane_data: Vec<(u64, String, Option<String>, Vec<SeriesOut>, Vec<AlertLine>)> =
        read_app(|app| {
            app.insts
                .iter()
                .filter(|i| i.kind == IndType::Pane)
                .map(|i| {
                    (
                        i.uid,
                        i.name.clone(),
                        i.format.clone(),
                        i.series.clone(),
                        i.alert_lines.clone(),
                    )
                })
                .collect()
        });
    for (uid, _name, format, series, alerts) in pane_data {
        let canvas = read_app(|app| {
            app.pane_canvas
                .iter()
                .find(|(u, _)| *u == uid)
                .map(|(_, c)| c.clone())
        });
        let canvas = match canvas {
            Some(c) => c,
            None => continue,
        };
        let w = canvas.parent_element().map(|p| p.client_width()).unwrap_or(600) as f64;
        let h = 130.0f64;
        let ctx = match prep_canvas(&canvas, w.max(200.0), h) {
            Some(c) => c,
            None => continue,
        };
        set_fill(&ctx, "#0b0b1a");
        ctx.fill_rect(0.0, 0.0, w, h);
        let span = read_app(|a| bar_span(a));
        let plot = Plot {
            left: 4.0,
            right: w - AXIS_W,
            top: 8.0,
            bottom: h - 14.0,
            bar_w: (w - AXIS_W - 4.0) / span,
        };
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;
        let (win_a, win_b) = read_app(|app| {
            (
                app.view_start,
                app.view_start + app.view_count + future_bars(app),
            )
        });
        for s in &series {
            for p in &s.data {
                if !p.value.is_finite() {
                    continue;
                }
                let fi = read_app(|app| frac_index(app, p.time));
                if fi < win_a - 0.5 || fi > win_b + 0.5 {
                    continue;
                }
                lo = lo.min(p.value);
                hi = hi.max(p.value);
            }
        }
        // make sure the user alert lines stay inside the visible band
        for a in &alerts {
            if a.price.is_finite() {
                lo = lo.min(a.price);
                hi = hi.max(a.price);
            }
        }
        if !lo.is_finite() {
            lo = 0.0;
            hi = 1.0;
        }
        let pad = (hi - lo) * 0.08;
        let (lo, hi) = if pad == 0.0 { (lo - 1.0, hi + 1.0) } else { (lo - pad, hi + pad) };
        // grid + labels
        ctx.set_line_width(1.0);
        set_stroke(&ctx, "#1a1a30");
        for k in 0..=2 {
            let y = plot.top + (plot.bottom - plot.top) * k as f64 / 2.0;
            ctx.begin_path();
            ctx.move_to(plot.left, y);
            ctx.line_to(plot.right, y);
            ctx.stroke();
            let val = hi - (hi - lo) * k as f64 / 2.0;
            set_fill(&ctx, "#8a8aa0");
            ctx.set_font("10px sans-serif");
            ctx.fill_text(&fmt_val(val, format.as_deref()), plot.right + 6.0, y + 3.0).ok();
        }
        if lo < 0.0 && hi > 0.0 {
            set_stroke(&ctx, "#3a3a60");
            let y0 = plot.bottom - ((0.0 - lo) / (hi - lo)) * (plot.bottom - plot.top);
            ctx.begin_path();
            ctx.move_to(plot.left, y0);
            ctx.line_to(plot.right, y0);
            ctx.stroke();
        }
        read_app(|app| {
            for s in &series {
                if s.kind == SeriesKind::Histogram {
                    let color = s.color.clone().unwrap_or_else(|| "#888".into());
                    for p in &s.data {
                        if !p.value.is_finite() {
                            continue;
                        }
                        let x = x_for_time(app, &plot, p.time);
                        let y = y_for(&plot, lo, hi, p.value);
                        let y0 = y_for(&plot, lo, hi, 0.0);
                        set_fill(&ctx, p.color.as_deref().unwrap_or(&color));
                        let bw = (plot.bar_w * 0.6).max(1.0);
                        ctx.fill_rect(x - bw / 2.0, y.min(y0), bw, (y - y0).abs().max(1.0));
                    }
                } else {
                    let color = s.color.clone().unwrap_or_else(|| "#888".into());
                    ctx.set_line_width(s.line_width.unwrap_or(1.0));
                    let mut last: Option<(f64, f64)> = None;
                    let mut run_color: Option<String> = None;
                    let mut started = false;
                    for p in &s.data {
                        if !p.value.is_finite() {
                            if started {
                                ctx.stroke();
                                started = false;
                            }
                            last = None;
                            run_color = None;
                            continue;
                        }
                        let x = x_for_time(app, &plot, p.time);
                        let y = y_for(&plot, lo, hi, p.value);
                        let col = p.color.clone().unwrap_or_else(|| color.clone());
                        match last {
                            None => {
                                set_stroke(&ctx, &col);
                                ctx.begin_path();
                                ctx.move_to(x, y);
                                run_color = Some(col);
                                started = true;
                            }
                            Some((px, py)) => {
                                if run_color.as_deref() != Some(col.as_str()) {
                                    if started {
                                        ctx.stroke();
                                    }
                                    set_stroke(&ctx, &col);
                                    ctx.begin_path();
                                    ctx.move_to(px, py);
                                    ctx.line_to(x, y);
                                    run_color = Some(col);
                                    started = true;
                                } else {
                                    ctx.line_to(x, y);
                                }
                            }
                        }
                        last = Some((x, y));
                    }
                    if started {
                        ctx.stroke();
                    }
                }
                // indicator price lines for this pane series
                for pl in &s.price_lines {
                    let y = y_for(&plot, lo, hi, pl.price);
                    draw_one_price_line(&ctx, &plot, y, pl.price, pl, None);
                }
            }
        });
        // user BB%b alert lines
        for a in &alerts {
            let y = y_for(&plot, lo, hi, a.price);
            let pl = PriceLine {
                price: a.price,
                color: a.color.clone(),
                line_width: 1.0,
                line_style: 2,
                title: format!("BB%b {}", a.price),
            };
            draw_one_price_line(&ctx, &plot, y, a.price, &pl, Some((y - 15.0).max(plot.top + 1.0)));
        }
        // shared crosshair: vertical line at the hovered bar, plus the
        // horizontal line while this exact pane is being hovered.
        let cross_idx = read_app(|a| a.cross_idx);
        let n_candles = read_app(|a| a.candles.len());
        if let Some(idx) = cross_idx.filter(|i| *i < n_candles) {
            let view_start = read_app(|a| a.view_start);
            let vx = plot.left + (idx as f64 - view_start + 0.5) * plot.bar_w;
            set_stroke(&ctx, "#5a5a80");
            let dash = Array::new();
            dash.push(&JsValue::from_f64(4.0));
            dash.push(&JsValue::from_f64(4.0));
            ctx.set_line_dash(&dash).ok();
            ctx.begin_path();
            if let Some((pu, _px, py)) = read_app(|a| a.pane_cross) {
                if pu == uid {
                    ctx.move_to(plot.left, py);
                    ctx.line_to(plot.right, py);
                }
            }
            ctx.move_to(vx, plot.top);
            ctx.line_to(vx, plot.bottom);
            ctx.stroke();
            let empty = Array::new();
            ctx.set_line_dash(&empty).ok();
        }
    }
}


// ---------------------------------------------------------------------------
// Pane readings + BB%b draw-line alerts
// ---------------------------------------------------------------------------

/// Value of every series of an instance at the hovered bar (or the last bar
/// when nothing is hovered), paired with its line colour.
fn reading_values(app: &App, inst: &Inst, idx: Option<usize>) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let first_color = inst
        .series
        .iter()
        .find_map(|s| s.color.clone())
        .unwrap_or_else(|| "#bbb".into());
    for s in &inst.series {
        let color = s.color.clone().unwrap_or_else(|| first_color.clone());
        let val = match idx {
            Some(i) => app
                .candles
                .get(i)
                .and_then(|c| s.data.iter().rev().find(|p| p.time <= c.time && p.value.is_finite()))
                .map(|p| p.value),
            None => s.data.iter().rev().find(|p| p.value.is_finite()).map(|p| p.value),
        };
        if let Some(v) = val {
            out.push((color, v));
        }
    }
    out
}

/// Realtime reading badge in each pane header.
fn update_pane_readings() {
    let panes: Vec<(u64, Option<String>)> = read_app(|app| {
        app.insts
            .iter()
            .filter(|i| i.kind == IndType::Pane)
            .map(|i| (i.uid, i.format.clone()))
            .collect()
    });
    for (uid, format) in panes {
        let sel = format!(".pane-box[data-uid=\"{}\"] .pane-read", uid);
        let el = match document().query_selector(&sel) {
            Ok(Some(e)) => e,
            _ => continue,
        };
        let reads = read_app(|app| {
            let idx = app.cross_idx.or_else(|| app.candles.len().checked_sub(1));
            app.insts
                .iter()
                .find(|i| i.uid == uid)
                .map(|i| reading_values(app, i, idx))
                .unwrap_or_default()
        });
        if reads.is_empty() {
            el.set_text_content(Some("--"));
            let _ = el.set_attribute("style", "color:#666");
            continue;
        }
        let text = reads
            .iter()
            .map(|(_, v)| fmt_val(*v, format.as_deref()))
            .collect::<Vec<_>>()
            .join(" / ");
        el.set_text_content(Some(&text));
        let _ = el.set_attribute("style", &format!("color:{}", reads[0].0));
    }
}

fn alert_line_color(count: usize) -> String {
    ALERT_COLORS[count % ALERT_COLORS.len()].to_string()
}

/// Fire a transient toast (the old app's `toastAlert`).
fn toast_alert(msg: &str) {
    let doc = document();
    let boxel = match by_id("indToastBox") {
        Some(b) => b,
        None => {
            let b = doc.create_element("div").unwrap();
            b.set_attribute("id", "indToastBox").ok();
            b.set_attribute(
                "style",
                "position:fixed;top:12px;right:12px;z-index:99999;display:flex;flex-direction:column;gap:6px;max-width:340px",
            )
            .ok();
            if let Some(body) = doc.body() {
                body.append_child(&b).ok();
            }
            b
        }
    };
    let t = doc.create_element("div").unwrap();
    t.set_attribute(
        "style",
        "background:#1a1a35;border:1px solid #ffb300;border-left:3px solid #ffb300;color:#d0d0d0;padding:8px 12px;border-radius:4px;font-size:11px;box-shadow:0 4px 16px rgba(0,0,0,.5)",
    )
    .ok();
    t.set_text_content(Some(msg));
    boxel.append_child(&t).ok();
    let t2 = t.clone();
    let cb = Closure::<dyn FnMut()>::new(move || {
        if let Some(p) = t2.parent_element() {
            let _ = p.remove_child(&t2);
        }
    });
    window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(
            cb.as_ref().unchecked_ref(),
            3500,
        )
        .ok();
    cb.forget();
}

/// Crossing detection for the user's BB%b alert lines: compare the rolling
/// tail's last two values and fire a toast + `ind-alert` event on a cross.
fn check_alert_lines() {
    // Only evaluate on newly-arrived data (the last bar's time changed), not on
    // every re-render (hovering the chart re-renders too).
    let last_time = read_app(|app| app.candles.last().map(|c| c.time).unwrap_or(0));
    if last_time == 0 || last_time == read_app(|app| app.alert_bar) {
        return;
    }
    with_app(|app| app.alert_bar = last_time);
    let data: Vec<(u64, Vec<AlertLine>, Vec<f64>)> = read_app(|app| {
        app.insts
            .iter()
            .filter(|i| i.id == "bbpct")
            .map(|i| {
                let vals: Vec<f64> = i
                    .series
                    .get(0)
                    .map(|s| {
                        s.data
                            .iter()
                            .filter(|p| p.value.is_finite())
                            .map(|p| p.value)
                            .collect()
                    })
                    .unwrap_or_default();
                (i.uid, i.alert_lines.clone(), vals)
            })
            .collect()
    });
    for (uid, lines, vals) in data {
        if lines.is_empty() || vals.len() < 2 {
            continue;
        }
        let last = vals[vals.len() - 1];
        let prev = vals[vals.len() - 2];
        for ln in &lines {
            let up = prev < ln.price && last >= ln.price;
            let dn = prev > ln.price && last <= ln.price;
            if !up && !dn {
                continue;
            }
            let now = js_sys::Date::now();
            let allow = read_app(|app| {
                app.alert_at
                    .get(&(uid, ln.id.clone()))
                    .copied()
                    .unwrap_or(0.0)
            });
            if now - allow < 2000.0 {
                continue;
            }
            with_app(|app| {
                app.alert_at.insert((uid, ln.id.clone()), now);
            });
            let dir = if up { "CROSSED ABOVE" } else { "CROSSED BELOW" };
            toast_alert(&format!(
                "BB%b {} {}  ->  {}",
                dir,
                fmt_val(ln.price, Some("decimal")),
                fmt_val(last, Some("decimal"))
            ));
            // notify any listeners (strategy engine / paper trade)
            let detail = Object::new();
            let _ = Reflect::set(&detail, &"id".into(), &"bbpct".into());
            let _ = Reflect::set(
                &detail,
                &"dir".into(),
                &JsValue::from_str(if up { "up" } else { "down" }),
            );
            let _ = Reflect::set(&detail, &"level".into(), &JsValue::from_f64(ln.price));
            let _ = Reflect::set(&detail, &"value".into(), &JsValue::from_f64(last));
            let _ = Reflect::set(&detail, &"uid".into(), &JsValue::from_f64(uid as f64));
            let _ = Reflect::set(&detail, &"line".into(), &JsValue::from_str(&ln.id));
            let mut init = web_sys::CustomEventInit::new();
            init.set_detail(&detail);
            if let Ok(ev) = CustomEvent::new_with_event_init_dict("ind-alert", &init) {
                let _ = document().dispatch_event(&ev);
            }
        }
    }
}

/// "+" button: open the small popover listing this pane's alert lines.
fn toggle_alert_box(uid: u64) {
    let sel = format!(".pane-box[data-uid=\"{}\"] .pane-alertbox", uid);
    let this = match document().query_selector(&sel) {
        Ok(Some(e)) => e,
        _ => return,
    };
    let opening = this.class_name().contains("hidden");
    if let Ok(all) = document().query_selector_all(".pane-alertbox") {
        for k in 0..all.length() {
            if let Some(n) = all.get(k) {
                if let Ok(el) = n.dyn_into::<Element>() {
                    el.set_class_name("pane-alertbox hidden");
                }
            }
        }
    }
    if opening {
        this.set_class_name("pane-alertbox");
        render_alert_box(uid);
    }
}

fn render_alert_box(uid: u64) {
    let sel = format!(".pane-box[data-uid=\"{}\"] .pane-alertbox", uid);
    let boxel = match document().query_selector(&sel) {
        Ok(Some(e)) => e,
        _ => return,
    };
    let lines: Vec<AlertLine> = read_app(|app| {
        app.insts
            .iter()
            .find(|i| i.uid == uid)
            .map(|i| i.alert_lines.clone())
            .unwrap_or_default()
    });
    let last = read_app(|app| {
        app.insts
            .iter()
            .find(|i| i.uid == uid)
            .and_then(|i| i.series.get(0))
            .and_then(|s| s.data.iter().rev().find(|p| p.value.is_finite()))
            .map(|p| p.value)
            .unwrap_or(0.5)
    });
    let doc = document();
    boxel.set_inner_html("");
    let title = doc.create_element("div").unwrap();
    title.set_attribute("class", "pane-alertbox-title").ok();
    title.set_text_content(Some("BB%b alert lines (draw-line crosses)"));
    boxel.append_child(&title).ok();

    let list = doc.create_element("div").unwrap();
    list.set_attribute("class", "pane-alertbox-list").ok();
    for ln in &lines {
        let row = doc.create_element("div").unwrap();
        row.set_attribute("class", "pane-alertbox-row").ok();
        let dot = doc.create_element("span").unwrap();
        dot.set_attribute(
            "style",
            &format!(
                "width:8px;height:8px;border-radius:50%;background:{};display:inline-block",
                ln.color
            ),
        )
        .ok();
        let val = doc.create_element("span").unwrap();
        val.set_text_content(Some(&fmt_val(ln.price, Some("decimal"))));
        val.set_attribute("style", "color:#ddd").ok();
        let rm = doc.create_element("button").unwrap();
        rm.set_attribute("class", "pane-alertbox-rm").ok();
        rm.set_inner_html(SVG_X);
        rm.set_attribute("title", &format!("Remove line {}", ln.price)).ok();
        let uid2 = uid;
        let id2 = ln.id.clone();
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            remove_alert_line(uid2, &id2);
        });
        rm.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
            .ok();
        cb.forget();
        row.append_child(&dot).ok();
        row.append_child(&val).ok();
        row.append_child(&rm).ok();
        list.append_child(&row).ok();
    }
    boxel.append_child(&list).ok();

    let add_row = doc.create_element("div").unwrap();
    add_row.set_attribute("class", "pane-alertbox-add").ok();
    let inp = doc
        .create_element("input")
        .unwrap()
        .dyn_into::<web_sys::HtmlInputElement>()
        .unwrap();
    inp.set_type("number");
    inp.set_step("0.05");
    inp.set_value(&fmt_val(last, Some("decimal")));
    inp.set_placeholder("Value (e.g. 0.5, 1.0)");
    let btn = doc
        .create_element("button")
        .unwrap()
        .dyn_into::<web_sys::HtmlButtonElement>()
        .unwrap();
    btn.set_text_content(Some("+ Add"));
    let uid2 = uid;
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
        if let Ok(Some(i)) = document().query_selector(&format!(
            ".pane-box[data-uid=\"{}\"] .pane-alertbox-add input",
            uid2
        )) {
            if let Ok(inp) = i.dyn_into::<web_sys::HtmlInputElement>() {
                if let Ok(v) = inp.value().trim().parse::<f64>() {
                    add_alert_line(uid2, v);
                } else {
                    let _ = inp.focus();
                }
            }
        }
    });
    btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();
    inp.set_attribute("data-uid", &uid.to_string()).ok();
    add_row.append_child(&inp).ok();
    add_row.append_child(&btn).ok();
    boxel.append_child(&add_row).ok();
}

fn add_alert_line(uid: u64, price: f64) {
    let mut ok = false;
    with_app(|app| {
        if let Some(inst) = app.insts.iter_mut().find(|i| i.uid == uid) {
            if inst
                .alert_lines
                .iter()
                .any(|l| (l.price - price).abs() < 1e-9)
            {
                return;
            }
            let color = alert_line_color(inst.alert_lines.len());
            let id = format!("al{}", app.alert_seq);
            app.alert_seq += 1;
            inst.alert_lines.push(AlertLine { id, price, color });
            ok = true;
        }
    });
    if ok {
        persist_indicators();
        render_all();
        let sel = format!(".pane-box[data-uid=\"{}\"] .pane-alertbox", uid);
        if let Ok(Some(b)) = document().query_selector(&sel) {
            if !b.class_name().contains("hidden") {
                render_alert_box(uid);
            }
        }
    }
}

fn remove_alert_line(uid: u64, id: &str) {
    with_app(|app| {
        if let Some(inst) = app.insts.iter_mut().find(|i| i.uid == uid) {
            inst.alert_lines.retain(|l| l.id != id);
        }
    });
    persist_indicators();
    render_all();
    let sel = format!(".pane-box[data-uid=\"{}\"] .pane-alertbox", uid);
    if let Ok(Some(b)) = document().query_selector(&sel) {
        if !b.class_name().contains("hidden") {
            render_alert_box(uid);
        }
    }
}

// ---------------------------------------------------------------------------
// Master render
// ---------------------------------------------------------------------------

fn render_all() {
    let has_data = read_app(|a| !a.candles.is_empty());
    if let Some(l) = by_id("loading") {
        // Keep the overlay in sync both ways: an empty series must surface the
        // loading/failure message again instead of leaving the bare canvas text.
        let _ = l.set_attribute("style", if has_data { "display:none" } else { "display:block" });
    }
    ensure_panes();
    oi_trend_tick();
    draw_main();
    draw_panes();
    update_legend();
    update_pane_readings();
    check_alert_lines();
}

// ---------------------------------------------------------------------------
// Legend
// ---------------------------------------------------------------------------

fn update_legend() {
    read_app(|app| {
        let legend = match by_id("indLegend") {
            Some(l) => l,
            None => return,
        };
        if app.insts.is_empty() {
            legend.set_class_name("hidden");
            return;
        }
        legend.set_class_name("");
        let idx = app.cross_idx.or_else(|| app.candles.len().checked_sub(1));
        let mut html = String::new();
        for inst in &app.insts {
            let color = inst
                .series
                .iter()
                .find_map(|s| s.color.clone())
                .unwrap_or_else(|| "#888".into());
            let reads = reading_values(app, inst, idx);
            let vals: String = reads
                .iter()
                .map(|(c, v)| {
                    format!(
                        "<span class='leg-val' style='color:{}'>{}</span>",
                        c,
                        fmt_val(*v, inst.format.as_deref())
                    )
                })
                .collect();
            html.push_str(&format!(
                "<span class='leg-item' data-uid='{}' style='cursor:pointer'><i style='background:{}'></i><span class='leg-name' data-uid='{}'>{}</span> {}<button class='leg-btn' data-act='settings' data-uid='{}' title='Settings'>{}</button><button class='leg-btn x' data-act='remove' data-uid='{}' title='Remove'>{}</button></span>",
                inst.uid,
                color,
                inst.uid,
                inst.name,
                vals,
                inst.uid,
                SVG_GEAR,
                inst.uid,
                SVG_X
            ));
        }
        legend.set_inner_html(&html);
    });
}

// ---------------------------------------------------------------------------
// Menu
// ---------------------------------------------------------------------------

fn build_menu() {
    let list = match by_id("indList") {
        Some(l) => l,
        None => return,
    };
    let mut html = String::new();
    let mut cats: Vec<String> = Vec::new();
    for entry in registry() {
        let d = entry.def;
        if d.hidden {
            continue;
        }
        if !cats.contains(&d.cat) {
            cats.push(d.cat.clone());
        }
        html.push_str(&format!(
            "<div class='ind-item' data-id='{}' data-cat='{}' title=\"{}\">{}</div>",
            d.id,
            d.cat,
            d.full_name.replace('"', "'"),
            d.name
        ));
    }
    let mut out = String::new();
    for cat in &cats {
        out.push_str(&format!("<div class='ind-cat'>{}</div>", cat));
        // simple approach: all items but filtered by JS-less CSS; we just list under each cat
        out.push_str("<div class='ind-group' data-cat='");
        out.push_str(cat);
        out.push_str("'></div>");
    }
    // Simpler: list items grouped is done below by rebuilding
    list.set_inner_html(&html);
    // attach click handlers
    let items = document().query_selector_all(".ind-item").unwrap();
    for k in 0..items.length() {
        if let Some(node) = items.get(k) {
            if let Ok(el) = node.dyn_into::<Element>() {
                let id = el.get_attribute("data-id").unwrap_or_default();
                let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                    with_app(|app| app.add(&id));
                    persist_indicators();
                    let menu = by_id("indMenu");
                    if let Some(m) = menu {
                        m.set_class_name("ind-menu hidden");
                    }
                    render_all();
                });
                el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                    .ok();
                cb.forget();
            }
        }
    }
    let _ = out;
}

// ---------------------------------------------------------------------------
// Settings panel
// ---------------------------------------------------------------------------

fn open_settings(uid: u64) {
    let panel = match by_id("indSettings") {
        Some(p) => p,
        None => return,
    };
    let info = read_app(|app| {
        app.insts
            .iter()
            .find(|i| i.uid == uid)
            .map(|i| (i.id.clone(), i.settings.clone()))
    });
    let (id, settings) = match info {
        Some(x) => x,
        None => return,
    };
    let entry = match registry().into_iter().find(|x| x.def.id == id) {
        Some(e) => e,
        None => return,
    };
    let mut html = format!(
        "<div class='set-head'>{} Settings <button id='setClose' class='set-x'>x</button></div><div class='set-body'>",
        entry.def.name
    );
    for input in &entry.def.inputs {
        let cur = settings.get(&input.key).cloned().unwrap_or(input.def.clone());
        match input.kind.as_str() {
            "number" => {
                html.push_str(&format!(
                    "<label class='set-row'>{}<input type='number' data-key='{}' value='{}' min='{}' max='{}' step='{}'></label>",
                    input.label,
                    input.key,
                    cur.as_f64().unwrap_or(0.0),
                    input.min.unwrap_or(0.0),
                    input.max.unwrap_or(1000.0),
                    input.step.unwrap_or(1.0)
                ));
            }
            "source" | "enum" => {
                html.push_str(&format!("<label class='set-row'>{}<select data-key='{}'>", input.label, input.key));
                let cur_s = cur.as_str().unwrap_or("").to_string();
                for opt in &input.options {
                    let sel = if opt.value == cur_s { " selected" } else { "" };
                    html.push_str(&format!("<option value='{}'{}>{}</option>", opt.value, sel, opt.label));
                }
                html.push_str("</select></label>");
            }
            _ => {
                let checked = cur.as_bool().unwrap_or(false);
                html.push_str(&format!(
                    "<label class='set-row'><input type='checkbox' data-key='{}'{}> {}</label>",
                    input.key,
                    if checked { " checked" } else { "" },
                    input.label
                ));
            }
        }
    }
    for st in &entry.def.style {
        let cur = settings.get(&st.key).cloned().unwrap_or(st.def.clone());
        if st.kind == "color" {
            html.push_str(&format!(
                "<label class='set-row'>{}<input type='color' data-key='{}' value='{}'></label>",
                st.label,
                st.key,
                cur.as_str().unwrap_or("#888888")
            ));
        } else {
            html.push_str(&format!(
                "<label class='set-row'>{}<input type='number' data-key='{}' value='{}' min='{}' max='{}' step='{}'></label>",
                st.label,
                st.key,
                cur.as_f64().unwrap_or(1.0),
                st.min.unwrap_or(0.0),
                st.max.unwrap_or(1000.0),
                st.step.unwrap_or(1.0)
            ));
        }
    }
    html.push_str("</div>");
    html.push_str("<div class='set-foot'><button id='setReset' class='set-btn warn'>Reset</button><span style='flex:1'></span><button id='setCancel' class='set-btn muted'>Cancel</button><button id='setApply' class='set-btn'>Apply</button></div>");
    panel.set_inner_html(&html);
    panel.set_class_name("ind-settings");
    panel.set_attribute("data-uid", &uid.to_string()).ok();

    // close
    if let Some(btn) = by_id("setClose") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            close_settings();
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Reset -> back to the indicator defaults, form re-rendered in place.
    if let Some(btn) = by_id("setReset") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            reset_settings(uid);
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Cancel -> discard the pending edits (the live inputs already applied, so
    // Cancel simply dismisses; Apply commits the final form state).
    if let Some(btn) = by_id("setCancel") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            close_settings();
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Apply -> read every control, commit, recompute and close.
    if let Some(btn) = by_id("setApply") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            apply_settings(uid);
        });
        btn.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // inputs
    let controls = document().query_selector_all("#indSettings [data-key]").unwrap();
    let uid_rc = Rc::new(uid);
    for k in 0..controls.length() {
        if let Some(node) = controls.get(k) {
            if let Ok(el) = node.dyn_into::<Element>() {
                let key = el.get_attribute("data-key").unwrap_or_default();
                let uid2 = uid_rc.clone();
                let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
                    let target = match e.target() {
                        Some(t) => t,
                        None => return,
                    };
                    if let Ok(input) = target.clone().dyn_into::<web_sys::HtmlInputElement>() {
                        let val = if input.type_() == "checkbox" {
                            json!(input.checked())
                        } else if input.type_() == "number" {
                            let raw = input.value();
                            if let Ok(n) = raw.parse::<f64>() {
                                json!(n)
                            } else {
                                json!(raw)
                            }
                        } else {
                            json!(input.value())
                        };
                        with_app(|app| {
                            for inst in app.insts.iter_mut() {
                                if inst.uid == *uid2 {
                                    inst.settings.insert(key.clone(), val.clone());
                                }
                            }
                            app.recompute(*uid2);
                        });
                        render_all();
                    } else if let Ok(sel) = target.dyn_into::<web_sys::HtmlSelectElement>() {
                        let val = json!(sel.value());
                        with_app(|app| {
                            for inst in app.insts.iter_mut() {
                                if inst.uid == *uid2 {
                                    inst.settings.insert(key.clone(), val.clone());
                                }
                            }
                            app.recompute(*uid2);
                        });
                        render_all();
                    }
                });
                el.add_event_listener_with_callback("input", cb.as_ref().unchecked_ref()).ok();
                el.add_event_listener_with_callback("change", cb.as_ref().unchecked_ref()).ok();
                cb.forget();
            }
        }
    }
}

/// Restore a deployed indicator's settings to the registry defaults and
/// re-render its settings form (the old app's Reset button).
fn reset_settings(uid: u64) {
    let def_id = read_app(|app| {
        app.insts
            .iter()
            .find(|i| i.uid == uid)
            .map(|i| i.id.clone())
    });
    let Some(def_id) = def_id else { return };
    let Some(entry) = registry().into_iter().find(|x| x.def.id == def_id) else {
        return;
    };
    let defaults = App::defaults_for(&entry.def);
    with_app(|app| {
        for inst in app.insts.iter_mut() {
            if inst.uid == uid {
                inst.settings = defaults.clone();
            }
        }
        app.recompute(uid);
    });
    persist_indicators();
    open_settings(uid);
    render_all();
}

/// Commit every control in the open settings form, recompute and close
/// (the old app's Apply button).
fn apply_settings(uid: u64) {
    let Some(panel) = by_id("indSettings") else { return };
    let controls = match panel.query_selector_all("[data-key]") {
        Ok(c) => c,
        Err(_) => return,
    };
    for k in 0..controls.length() {
        let Some(node) = controls.get(k) else { continue };
        let Ok(el) = node.dyn_into::<Element>() else { continue };
        let key = el.get_attribute("data-key").unwrap_or_default();
        let val = if let Ok(input) = el.clone().dyn_into::<web_sys::HtmlInputElement>() {
            if input.type_() == "checkbox" {
                json!(input.checked())
            } else if input.type_() == "number" {
                let raw = input.value();
                match raw.parse::<f64>() {
                    Ok(n) => json!(n),
                    Err(_) => json!(raw),
                }
            } else {
                json!(input.value())
            }
        } else if let Ok(sel) = el.dyn_into::<web_sys::HtmlSelectElement>() {
            json!(sel.value())
        } else {
            continue;
        };
        with_app(|app| {
            for inst in app.insts.iter_mut() {
                if inst.uid == uid {
                    inst.settings.insert(key.clone(), val.clone());
                }
            }
        });
    }
    with_app(|app| app.recompute(uid));
    persist_indicators();
    close_settings();
    render_all();
}

// ---------------------------------------------------------------------------
// Toolbar / controls
// ---------------------------------------------------------------------------

fn build_tf_grid() {
    let host = match by_id("tfGrid") {
        Some(h) => h,
        None => return,
    };
    let mut html = String::new();
    for tf in TIMEFRAMES {
        html.push_str(&format!(
            "<button class='tf-btn' data-tf='{}' title='{}'>{}</button>",
            tf.key, tf.label, tf.key
        ));
    }
    host.set_inner_html(&html);
    let btns = document().query_selector_all(".tf-btn").unwrap();
    for k in 0..btns.length() {
        if let Some(node) = btns.get(k) {
            if let Ok(el) = node.dyn_into::<Element>() {
                let key = el.get_attribute("data-tf").unwrap_or_default();
                let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                    with_app(|app| app.timeframe = key.clone());
                    update_tf_active();
                    load_chart();
                });
                el.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref())
                    .ok();
                cb.forget();
            }
        }
    }
    update_tf_active();
}

fn update_tf_active() {
    let cur = read_app(|a| a.timeframe.clone());
    if let Ok(btns) = document().query_selector_all(".tf-btn") {
        for k in 0..btns.length() {
            if let Some(node) = btns.get(k) {
                if let Ok(el) = node.dyn_into::<Element>() {
                    let key = el.get_attribute("data-tf").unwrap_or_default();
                    let cls = if key == cur { "tf-btn active" } else { "tf-btn" };
                    el.set_class_name(cls);
                }
            }
        }
    }
    if let Some(l) = by_id("chartTfLabel") {
        l.set_text_content(Some(&cur));
    }
}

fn hook_indicators_menu() {
    let btn = by_id("indMenuBtn");
    if let Some(b) = btn {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            if let Some(m) = by_id("indMenu") {
                let cls = m.class_name();
                if cls.contains("hidden") {
                    m.set_class_name("ind-menu");
                } else {
                    m.set_class_name("ind-menu hidden");
                }
            }
        });
        b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // search
    if let Some(inp) = by_id("indSearch").and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok()) {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            // handled by closure owning input? re-read
            filter_menu();
        });
        inp.add_event_listener_with_callback("input", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
}

fn filter_menu() {
    let q = by_id("indSearch")
        .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
        .map(|i| i.value().to_lowercase())
        .unwrap_or_default();
    if let Ok(items) = document().query_selector_all(".ind-item") {
        for k in 0..items.length() {
            if let Some(node) = items.get(k) {
                if let Ok(el) = node.dyn_into::<Element>() {
                    let name = el.text_content().unwrap_or_default().to_lowercase();
                    let id = el.get_attribute("data-id").unwrap_or_default().to_lowercase();
                    let show = q.is_empty() || name.contains(&q) || id.contains(&q);
                    let _ = el.set_attribute("style", if show { "" } else { "display:none" });
                }
            }
        }
    }
}

fn hook_toolbar() {
    // Remove Indicators
    if let Some(b) = by_id("btnRemoveAll") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            with_app(|app| {
                app.insts.clear();
                app.pane_canvas.clear();
                app.alert_at.clear();
            });
            persist_indicators();
            set_html("ind-panes", "<div class='ind-empty'>No pane indicators added. Use the Indicators menu to add one.</div>");
            if let Some(s) = by_id("indSettings") {
                s.set_class_name("ind-settings hidden");
            }
            render_all();
        });
        b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Refresh
    if let Some(b) = by_id("btnRefresh") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| load_chart());
        b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Best combo: structure bias (pastruct) + dynamic S/R trendline (autotrend)
    // + volume/liquidity trend core (vlcore). Re-deploying de-dupes the three
    // so repeated clicks never stack (old app's applyBestCombo).
    if let Some(b) = by_id("btnBestCombo") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            with_app(|app| {
                app.insts
                    .retain(|i| !matches!(i.id.as_str(), "pastruct" | "autotrend" | "vlcore"));
            });
            with_app(|app| {
                app.add_with(
                    "pastruct",
                    &[
                        ("lineMode", json!("zigzag")),
                        ("showMarkers", json!(true)),
                        ("markersOnly", json!(false)),
                    ],
                );
                app.add_with(
                    "autotrend",
                    &[
                        ("strength", json!(5.0)),
                        ("look", json!(60.0)),
                        ("fullSpan", json!(true)),
                    ],
                );
                app.add_with(
                    "vlcore",
                    &[
                        ("length", json!(21.0)),
                        ("atrLength", json!(14.0)),
                        ("gap", json!(1.0)),
                        ("confirm", json!(2.0)),
                        ("wickLen", json!(3.0)),
                        ("straightLine", json!(true)),
                        ("useVolume", json!(true)),
                    ],
                );
            });
            persist_indicators();
            render_all();
            if let Some(st) = by_id("status") {
                st.set_text_content(Some("Best Combo: Price Action Trend + Auto Trendline + Trend Core"));
            }
        });
        b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // OI Trend toggle: paints OI support/resistance walls, Max Pain and the
    // expiry range from the loaded option chain, plus the EMA-style trend-state
    // line (old app's `oitrend.js`, math in `algo_core::oi_trend`).
    if let Some(inp) =
        by_id("oiTrendToggle").and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
    {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let on = e
                .target()
                .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
                .map(|i| i.checked())
                .unwrap_or(false);
            with_app(|a| a.oi_enabled = on);
            ls_set("oitrend:enabled", if on { "1" } else { "0" });
            if on {
                crate::optionchain::ensure_loaded();
                crate::optionchain::sync_oi_trend_now();
            } else {
                oi_trend_clear();
            }
        });
        inp.add_event_listener_with_callback("change", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
        // Restore last session's toggle (old app's `LS_KEY 'oitrend:enabled'`).
        if ls_get("oitrend:enabled").as_deref() == Some("1") {
            inp.set_checked(true);
            with_app(|a| a.oi_enabled = true);
            crate::optionchain::ensure_loaded();
        }
    }
    // legend click -> settings / remove (buttons carry data-act; the row and
    // name carry data-uid, so walk up from the click target)
    if let Some(legend) = by_id("indLegend") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            let target = match e.target() {
                Some(t) => t,
                None => return,
            };
            let Ok(el) = target.dyn_into::<Element>() else { return };
            let mut node = Some(el);
            while let Some(n) = node {
                if let Some(act) = n.get_attribute("data-act") {
                    if let Some(u) = n
                        .get_attribute("data-uid")
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        match act.as_str() {
                            "remove" => {
                                with_app(|app| app.remove(u));
                                persist_indicators();
                                let open_uid = by_id("indSettings")
                                    .and_then(|p| p.get_attribute("data-uid"))
                                    .and_then(|s| s.parse::<u64>().ok());
                                if open_uid == Some(u) {
                                    close_settings();
                                }
                                render_all();
                            }
                            "settings" => open_settings(u),
                            _ => {}
                        }
                    }
                    return;
                }
                if let Some(uid) = n.get_attribute("data-uid") {
                    if let Ok(u) = uid.parse::<u64>() {
                        open_settings(u);
                    }
                    return;
                }
                node = n.parent_element();
            }
        });
        legend.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
    // Long / Short Position tools: arm a tool, then drag on the chart to draw
    // entry + profit target; the stop-loss mirrors automatically.
    for (id, dir) in [("posLongBtn", PosDir::Long), ("posShortBtn", PosDir::Short)] {
        if let Some(b) = by_id(id) {
            let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
                with_app(|a| {
                    a.pos_tool = if a.pos_tool == Some(dir) { None } else { Some(dir) };
                });
                update_pos_buttons();
                read_app(apply_pos_cursor);
            });
            b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
            cb.forget();
        }
    }
    // Clear every position on the active chart.
    if let Some(b) = by_id("posClearBtn") {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            with_app(|a| {
                a.positions.clear();
                a.pos_drag = None;
                a.pos_edit = None;
                a.pos_hover = None;
            });
            persist_positions();
            update_pos_buttons();
            render_all();
        });
        b.add_event_listener_with_callback("click", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
}

/// Canvas-local (x, y) of a mouse event plus whether it is inside the canvas.
/// Used by the window-level move listeners so a drag keeps tracking the cursor
/// even after it leaves the canvas (smooth TradingView-style drawing).
fn canvas_xy(e: &MouseEvent) -> Option<(f64, f64, bool)> {
    let c = by_id("chartCanvas")?.dyn_into::<HtmlCanvasElement>().ok()?;
    let rect = c.get_bounding_client_rect();
    let w = c.client_width() as f64;
    let h = c.client_height() as f64;
    let x = e.client_x() as f64 - rect.left();
    let y = e.client_y() as f64 - rect.top();
    let inside = x >= 0.0 && y >= 0.0 && x <= w && y <= h;
    Some((x, y, inside))
}

fn hook_canvas() {
    let container = match by_id("chart-container") {
        Some(c) => c,
        None => return,
    };
    let doc = document();
    let canvas = doc
        .create_element("canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();
    canvas.set_attribute("id", "chartCanvas").ok();
    container.set_inner_html("");
    let loading = doc.create_element("div").unwrap();
    loading.set_attribute("id", "loading").ok();
    loading.set_text_content(Some("Connect to load chart"));
    container.append_child(&canvas).ok();
    container.append_child(&loading).ok();

    // Crosshair preview. Deliberately render-free: the dedicated handler below
    // owns the single redraw per move, so a drag repaints once (smooth, no
    // double-render flicker). On the window so the crosshair also updates while
    // the cursor is dragged outside the canvas.
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
        let Some((x, y, inside)) = canvas_xy(&e) else { return };
        with_app(|app| {
            if app.pos_drag.is_some() || app.pos_edit.is_some() || !inside {
                app.cross = None;
                app.cross_idx = None;
                return;
            }
            app.cross = Some((x, y));
            app.pane_cross = None;
            let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
            let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
            let plot = plot_for(app, w, h);
            let idx = ((x - plot.left) / plot.bar_w + app.view_start).round();
            app.cross_idx = if idx >= 0.0 && (idx as usize) < app.candles.len() {
                Some(idx as usize)
            } else {
                None
            };
        });
    });
    window()
        .add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // mouseleave
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |_e: MouseEvent| {
        with_app(|app| {
            app.cross = None;
            app.cross_idx = None;
            app.pos_hover = None;
            apply_pos_cursor(app);
        });
        render_all();
    });
    canvas
        .add_event_listener_with_callback("mouseleave", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // mousedown
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
        let rect = by_id("chartCanvas")
            .map(|c| c.get_bounding_client_rect())
            .unwrap();
        let x = e.client_x() as f64 - rect.left();
        let y = e.client_y() as f64 - rect.top();
        with_app(|app| {
            let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
            let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
            let plot = plot_for(app, w, h);
            let (lo, hi) = price_extent(app, &plot);
            let can_draw = !app.candles.is_empty();
            if let Some(dir) = app.pos_tool.filter(|_| can_draw) {
                // Start a brand-new position: this point is the entry.
                let fi = index_at_x(&plot, app, x);
                let t = time_for_index(app, fi);
                let p = price_at_y(&plot, lo, hi, y);
                app.pos_drag = Some(PosDrag {
                    dir,
                    entry_time: t,
                    entry_price: p,
                    cur_time: t,
                    cur_price: p,
                });
                app.pos_edit = None;
                app.pos_hover = None;
                app.dragging = false;
            } else if let Some((idx, kind)) = pos_hit(app, &plot, lo, hi, x, y) {
                let orig = app.positions[idx].clone();
                let grab_price = price_at_y(&plot, lo, hi, y);
                app.pos_edit = Some(PosEdit { idx, kind, grab_price, orig });
                app.pos_drag = None;
                app.dragging = false;
            } else {
                app.dragging = true;
                app.drag_x = x;
                app.drag_start = app.view_start;
            }
            apply_pos_cursor(app);
        });
    });
    canvas
        .add_event_listener_with_callback("mousedown", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // mousemove: pan drag, position draw/edit, or hover hit-test. On the window
    // so a position drag keeps following the cursor past the canvas edge.
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
        let Some((x, y, inside)) = canvas_xy(&e) else { return };
        with_app(|app| {
            let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
            let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
            let plot = plot_for(app, w, h);
            let (lo, hi) = price_extent(app, &plot);
            let cur_price = price_at_y(&plot, lo, hi, y);
            let cur_time = time_for_index(app, index_at_x(&plot, app, x));
            if let Some(d) = app.pos_drag.as_mut() {
                d.cur_price = cur_price;
                d.cur_time = cur_time;
                app.cross = None;
                app.cross_idx = None;
            } else if app.pos_edit.is_some() {
                let edit = app.pos_edit.as_ref().unwrap();
                let idx = edit.idx;
                let kind = edit.kind;
                let orig = edit.orig.clone();
                let grab = edit.grab_price;
                let delta = cur_price - grab;
                let tick = ((hi - lo).abs() / 400.0).max(0.01);
                if idx < app.positions.len() {
                    match kind {
                        PosEditKind::Move => {
                            let t = &mut app.positions[idx];
                            t.entry_price = orig.entry_price + delta;
                            t.target_price = orig.target_price + delta;
                            t.stop_price = orig.stop_price + delta;
                        }
                        PosEditKind::Target => {
                            let e = orig.entry_price;
                            let v = match orig.dir {
                                PosDir::Long => cur_price.max(e + tick),
                                PosDir::Short => cur_price.min(e - tick),
                            };
                            app.positions[idx].target_price = v;
                        }
                        PosEditKind::Stop => {
                            let e = orig.entry_price;
                            let v = match orig.dir {
                                PosDir::Long => cur_price.min(e - tick),
                                PosDir::Short => cur_price.max(e + tick),
                            };
                            app.positions[idx].stop_price = v;
                        }
                    }
                }
                app.cross = None;
                app.cross_idx = None;
            } else if app.dragging {
                let dx = (x - app.drag_x) / plot.bar_w.max(0.01);
                app.view_start = (app.drag_start - dx).max(-1.0);
            } else {
                app.pos_hover = if app.pos_tool.is_none() && inside {
                    pos_hit(app, &plot, lo, hi, x, y)
                } else {
                    None
                };
            }
            apply_pos_cursor(app);
        });
        render_all();
    });
    window()
        .add_event_listener_with_callback("mousemove", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // mouseup
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |_e: MouseEvent| {
        let changed = with_app_ret(|app| {
            app.dragging = false;
            let mut changed = false;
            if app.pos_edit.take().is_some() {
                changed = true;
            }
            if let Some(d) = app.pos_drag.take() {
                let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
                let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
                let plot = plot_for(app, w, h);
                let (lo, hi) = price_extent(app, &plot);
                let tick = ((hi - lo).abs() / 400.0).max(0.01);
                let risk = (d.cur_price - d.entry_price).abs();
                if risk >= tick {
                    let (target, stop) = pos_levels(d.dir, d.entry_price, d.cur_price);
                    app.positions.push(PositionTool {
                        dir: d.dir,
                        entry_time: d.entry_time,
                        end_time: d.cur_time,
                        entry_price: d.entry_price,
                        target_price: target,
                        stop_price: stop,
                    });
                    changed = true;
                    // Auto-disarm so the chart returns to pan/crosshair mode.
                    app.pos_tool = None;
                }
            }
            changed
        });
        if changed {
            persist_positions();
            update_pos_buttons();
        }
        read_app(apply_pos_cursor);
        render_all();
    });
    window()
        .add_event_listener_with_callback("mouseup", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // double-click removes the position under the pointer
    let cb = Closure::<dyn FnMut(MouseEvent)>::new(move |e: MouseEvent| {
        let rect = by_id("chartCanvas")
            .map(|c| c.get_bounding_client_rect())
            .unwrap();
        let x = e.client_x() as f64 - rect.left();
        let y = e.client_y() as f64 - rect.top();
        let removed = with_app_ret(|app| {
            let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
            let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
            let plot = plot_for(app, w, h);
            let (lo, hi) = price_extent(app, &plot);
            if let Some((idx, _)) = pos_hit(app, &plot, lo, hi, x, y) {
                app.positions.remove(idx);
                app.pos_hover = None;
                true
            } else {
                false
            }
        });
        if removed {
            persist_positions();
            update_pos_buttons();
            render_all();
        }
    });
    canvas
        .add_event_listener_with_callback("dblclick", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // wheel zoom
    let cb = Closure::<dyn FnMut(WheelEvent)>::new(move |e: WheelEvent| {
        e.prevent_default();
        let rect = by_id("chartCanvas")
            .map(|c| c.get_bounding_client_rect())
            .unwrap();
        let x = e.client_x() as f64 - rect.left();
        let delta = e.delta_y();
        with_app(|app| {
            let w = by_id("chartCanvas").map(|c| c.client_width() as f64).unwrap_or(800.0);
            let h = by_id("chartCanvas").map(|c| c.client_height() as f64).unwrap_or(400.0);
            let plot = plot_for(app, w, h);
            let anchor = app.view_start + (x - plot.left) / plot.bar_w.max(0.01);
            let factor = if delta > 0.0 { 1.15 } else { 1.0 / 1.15 };
            let new_count = (app.view_count * factor).clamp(20.0, 600.0);
            let ratio = (anchor - app.view_start) / app.view_count.max(1.0);
            app.view_count = new_count;
            app.view_start = (anchor - ratio * new_count).max(-1.0);
        });
        render_all();
    });
    canvas
        .add_event_listener_with_callback("wheel", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // resize
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
        // Force the OI strip/overlay to re-measure on canvas-size changes.
        with_app(|a| {
            a.oi_sig.clear();
            a.oi_strip_last = 0.0;
        });
        render_all();
    });
    window()
        .add_event_listener_with_callback("resize", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();

    // The chart is rendered while its tab is hidden (display:none) whenever a
    // trade/strike "Chart" button opens it, so the canvas is measured against a
    // zero-width container. Re-measure and redraw the moment the chart tab
    // actually becomes visible, instead of leaving a squashed/zoomed canvas
    // until the next window resize or feed tick.
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
        let detail = e
            .dyn_ref::<CustomEvent>()
            .and_then(|c| c.detail().as_string())
            .unwrap_or_default();
        if detail == "chart" {
            render_all();
        }
    });
    document()
        .add_event_listener_with_callback("tabshown", cb.as_ref().unchecked_ref())
        .ok();
    cb.forget();
}

fn hook_realtime_toggle() {
    if let Some(inp) = by_id("rtToggle").and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok()) {
        let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |_e: web_sys::Event| {
            let checked = by_id("rtToggle")
                .and_then(|e| e.dyn_into::<web_sys::HtmlInputElement>().ok())
                .map(|i| i.checked())
                .unwrap_or(true);
            with_app(|a| a.realtime = checked);
        });
        inp.add_event_listener_with_callback("change", cb.as_ref().unchecked_ref()).ok();
        cb.forget();
    }
}

fn start_ticker() {
    let cb = Closure::<dyn FnMut()>::new(move || {
        // bar countdown - only ticks while the market is open; intraday charts
        // show "Closed" instead of a countdown after hours (old app behaviour).
        let tf = read_app(|a| a.timeframe.clone());
        let step = tf_step(&tf);
        let now = (js_sys::Date::now() / 1000.0) as i64;
        let intraday = is_intraday(&tf);
        if let Some(el) = by_id("barCountdown") {
            if intraday && !is_market_open(now) {
                el.set_class_name("closed");
                el.set_text_content(Some("Closed"));
                let _ = el.set_attribute("title", "Market is closed");
            } else {
                el.set_class_name("");
                let next = next_bar_close(now, &tf, step);
                let remain = (next - now).max(0);
                let hh = remain / 3600;
                let mm = (remain % 3600) / 60;
                let ss = remain % 60;
                let text = if hh > 0 {
                    format!("{}:{:02}:{:02}", hh, mm, ss)
                } else {
                    format!("{:02}:{:02}", mm, ss)
                };
                el.set_text_content(Some(&text));
                let _ = el.set_attribute(
                    "title",
                    &format!("Bar closes in {}m {}s ({})", mm, ss, tf),
                );
            }
        }
        let bar = now / step.max(1);
        let (rt, last) = read_app(|a| (a.realtime, a.last_bar));
        if rt {
            // Live path: fold the current tick into the forming bar and roll it
            // onto the new bucket purely from the WebSocket feed (no history
            // request). When the feed is down there is no tick to roll with, so
            // fall back to a one-off history fetch to keep the chart advancing.
            let had_tick = apply_live_tick();
            if bar != last && last != 0 && !had_tick {
                with_app(|a| a.last_bar = bar);
                load_chart_preserve(true);
            }
        }
    });
    window()
        .set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), 1000)
        .ok();
    cb.forget();
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// 0=Sunday .. 6=Saturday for a UTC epoch second.
fn utc_weekday(now: i64) -> i64 {
    (now.div_euclid(86400) + 4).rem_euclid(7)
}

/// NSE cash-market open/close for the UTC day containing `now` (9:15-15:30 IST).
fn market_bounds(now: i64) -> (i64, i64) {
    let day = now.div_euclid(86400) * 86400;
    (day + 3 * 3600 + 45 * 60, day + 10 * 3600)
}

fn is_market_open(now: i64) -> bool {
    let wd = utc_weekday(now);
    if wd == 0 || wd == 6 {
        return false;
    }
    let (o, c) = market_bounds(now);
    now >= o && now < c
}

/// Next bar-close boundary for the countdown, clamped to the market close for
/// intraday timeframes (mirrors the old app's `nextBarCloseFromNow`).
fn next_bar_close(now: i64, tf: &str, step: i64) -> i64 {
    if is_intraday(tf) && step > 0 {
        let bar = ((now + step - 1) / step) * step;
        let (_, close) = market_bounds(now);
        return if bar > close { close } else { bar };
    }
    let day = now.div_euclid(86400) * 86400;
    let close = day + 10 * 3600; // 15:30 IST
    match tf {
        "day" => {
            if now < close { close } else { close + 86400 }
        }
        "week" => {
            let wd = utc_weekday(now);
            let mut d = (8 - wd) % 7;
            if d == 0 {
                d = 7;
            }
            day + d * 86400 + 10 * 3600
        }
        "month" => {
            let (y, m, _) = civil_from_days(now.div_euclid(86400));
            let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            days_from_civil(ny, nm, 1) * 86400 + 10 * 3600
        }
        "year" => {
            let (y, _, _) = civil_from_days(now.div_euclid(86400));
            days_from_civil(y + 1, 1, 1) * 86400 + 10 * 3600
        }
        _ => close,
    }
}

fn tf_step(tf: &str) -> i64 {
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
        "day" => 86400,
        "week" => 604800,
        "month" => 2592000,
        _ => 300,
    }
}

// ---------------------------------------------------------------------------
// Persistence (deployed indicators survive a page reload)
// ---------------------------------------------------------------------------

fn persist_indicators() {
    let Ok(Some(st)) = window().local_storage() else {
        return;
    };
    let value = read_app(|app| {
        let arr: Vec<Value> = app
            .insts
            .iter()
            .map(|i| {
                let alerts: Vec<Value> = i
                    .alert_lines
                    .iter()
                    .map(|a| json!({ "id": a.id, "price": a.price, "color": a.color }))
                    .collect();
                json!({ "id": i.id, "settings": i.settings, "alerts": alerts })
            })
            .collect();
        Value::Array(arr)
    });
    let s = serde_json::to_string(&value).unwrap_or_else(|_| "[]".into());
    let _ = st.set_item(SAVE_KEY, &s);
}

fn restore_indicators() {
    let Ok(Some(st)) = window().local_storage() else {
        return;
    };
    let raw = st.get_item(SAVE_KEY).ok().flatten().unwrap_or_default();
    if raw.is_empty() {
        return;
    }
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(arr) = parsed.as_array() else { return };
    for item in arr {
        let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let owned: Vec<(String, Value)> = item
            .get("settings")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let refs: Vec<(&str, Value)> = owned.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        with_app(|app| app.add_with(id, &refs));
        let uid = read_app(|app| app.insts.last().map(|i| i.uid).unwrap_or(0));
        if uid == 0 {
            continue;
        }
        if let Some(al) = item.get("alerts").and_then(|v| v.as_array()) {
            with_app(|app| {
                if let Some(inst) = app.insts.iter_mut().find(|i| i.uid == uid) {
                    for a in al {
                        if let Some(price) = a.get("price").and_then(|v| v.as_f64()) {
                            let color = a
                                .get("color")
                                .and_then(|v| v.as_str())
                                .unwrap_or("#ff5252")
                                .to_string();
                            let id = a
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            inst.alert_lines.push(AlertLine { id, price, color });
                        }
                    }
                }
            });
        }
    }
    let max_al = read_app(|app| {
        app.insts
            .iter()
            .flat_map(|i| i.alert_lines.iter())
            .filter_map(|a| a.id.trim_start_matches("al").parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    });
    with_app(|app| app.alert_seq = app.alert_seq.max(max_al + 1));
}

// ---------------------------------------------------------------------------
// Long / Short Position persistence (per exchange+security)
// ---------------------------------------------------------------------------

const POS_SAVE_KEY: &str = "algo_chart_positions";

fn pos_key(app: &App) -> String {
    format!("{}:{}", app.exch, app.sec_id)
}

fn persist_positions() {
    let key = read_app(pos_key);
    let arr: Vec<Value> = read_app(|a| a.positions.iter().map(pos_to_json).collect());
    let mut map: serde_json::Map<String, Value> = ls_get(POS_SAVE_KEY)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    map.insert(key, Value::Array(arr));
    if let Ok(s) = serde_json::to_string(&Value::Object(map)) {
        ls_set(POS_SAVE_KEY, &s);
    }
}

fn restore_positions() {
    let key = read_app(pos_key);
    let list: Vec<Value> = ls_get(POS_SAVE_KEY)
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .and_then(|m| m.get(&key).cloned())
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    let parsed: Vec<PositionTool> = list.iter().filter_map(pos_from_json).collect();
    with_app(|a| a.positions = parsed);
    update_pos_buttons();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    build_tf_grid();
    build_menu();
    hook_indicators_menu();
    hook_toolbar();
    hook_canvas();
    hook_realtime_toggle();
    start_ticker();
    restore_indicators();
    restore_positions();
    update_pos_buttons();
    optionchain::boot_option_chain();
    let _ = by_id("indSectionCount").map(|c| c.set_text_content(Some("0")));
    load_chart();
    chart_feed_connect();
}

/// Set the candlestick timeframe without triggering a load, so a follow-up
/// `select_symbol` loads the new symbol at that timeframe. Used by the option
/// chain's MCX fallback (intraday -> daily).
pub(crate) fn prepare_chart_timeframe(tf: &str) {
    with_app(|app| app.timeframe = tf.to_string());
    update_tf_active();
}

/// Sidebar entry point: switch the chart to another instrument. `sec_id` is an
/// f64 so JS can pass a plain number (i64 would require a BigInt).
#[wasm_bindgen]
pub fn select_symbol(sec_id: f64, exch: &str, inst_type: &str, name: &str) {
    crate::optionchain::on_oc_symbol_change();
    with_app(|app| {
        app.sec_id = sec_id as i64;
        app.exch = exch.to_string();
        app.inst_type = inst_type.to_string();
        app.symbol_name = name.to_string();
        // overlays are symbol-specific: drop them on switch
        app.trade_lines.clear();
        app.oc_lines.clear();
        app.dir_overlay = None;
        // The previous symbol's last price must never bleed into the new series.
        app.live_ltp = 0.0;
        app.live_vol = 0.0;
        app.local_bar = false;
        app.bar_vol_base = 0.0;
        // position drawings are symbol-specific: drop the in-flight state, then
        // restore any saved drawings for the newly selected instrument.
        app.pos_drag = None;
        app.pos_edit = None;
        app.pos_hover = None;
        app.pos_tool = None;
    });
    restore_positions();
    update_pos_buttons();
    if let Some(el) = by_id("chartSymbolLabel") {
        el.set_text_content(Some(name));
    }
    load_chart();
    chart_feed_connect();
}

// ---------------------------------------------------------------------------
// Chart overlay integration API (JS modules: strategies, paper trade, OI trend)
// ---------------------------------------------------------------------------

fn parse_price_lines(json: &str) -> Vec<PriceLine> {
    let mut out = Vec::new();
    let parsed: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let items = if let Some(a) = parsed.as_array() {
        a.clone()
    } else if parsed.is_object() {
        vec![parsed]
    } else {
        return out;
    };
    for it in items {
        let Some(price) = it.get("price").and_then(|v| v.as_f64()) else {
            continue;
        };
        out.push(PriceLine {
            price,
            color: it
                .get("color")
                .and_then(|v| v.as_str())
                .unwrap_or("#f0c000")
                .to_string(),
            line_width: it.get("lineWidth").and_then(|v| v.as_f64()).unwrap_or(1.0),
            line_style: it
                .get("lineStyle")
                .and_then(|v| v.as_i64())
                .or_else(|| it.get("style").and_then(|v| v.as_i64()))
                .unwrap_or(2) as i32,
            title: it
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        });
    }
    out
}

/// Replace the strategy/paper-trade level lines drawn on the main chart (old
/// app's `IndChart.setTradeLines`).
#[wasm_bindgen]
pub fn set_trade_lines(json: &str) {
    let lines = parse_price_lines(json);
    with_app(|app| app.trade_lines = lines);
    render_all();
}

#[wasm_bindgen]
pub fn clear_trade_lines() {
    with_app(|app| app.trade_lines.clear());
    render_all();
}

/// Parse the Renko settings payload (`{mode, boxSize, atrLength, percentage,
/// wicks, source}`, or a wrapper `{renko:{...}}`). Missing / invalid fields keep
/// the defaults so a partial payload can never produce a zero-size brick.
fn parse_renko_config(json: &str) -> RenkoConfig {
    let mut cfg = RenkoConfig::default();
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return cfg;
    };
    let r = v.get("renko").unwrap_or(&v);
    if let Some(m) = r.get("mode").and_then(|x| x.as_str()) {
        cfg.mode = RenkoMode::parse(m);
    }
    if let Some(b) = r.get("boxSize").and_then(|x| x.as_f64()) {
        if b > 0.0 {
            cfg.box_size = b;
        }
    }
    if let Some(a) = r.get("atrLength").and_then(|x| x.as_f64()) {
        if a >= 1.0 {
            cfg.atr_length = a as usize;
        }
    }
    if let Some(p) = r.get("percentage").and_then(|x| x.as_f64()) {
        if p > 0.0 {
            cfg.percentage = p;
        }
    }
    if let Some(w) = r.get("wicks").and_then(|x| x.as_bool()) {
        cfg.wicks = w;
    }
    if let Some(s) = r.get("source").and_then(|x| x.as_str()) {
        cfg.source = RenkoSource::parse(s);
    }
    cfg
}

/// Switch the chart type (`candles` / `heikin_ashi` / `renko`) and apply the
/// Renko settings, rebuilding the display series from the raw candles. The
/// open indicators are recomputed on the new series, exactly as TradingView
/// does when the chart type changes.
#[wasm_bindgen]
pub fn set_chart_kind(kind: &str, json: &str) {
    let cfg = parse_renko_config(json);
    let k = ChartKind::parse(kind);
    with_app(|app| {
        app.chart_kind = k;
        app.renko = cfg;
        app.rebuild_view();
        app.recompute_all();
    });
    render_all();
}

/// Current chart kind + Renko settings as JSON, so the UI can restore its state.
#[wasm_bindgen]
pub fn chart_kind_json() -> String {
    read_app(|app| {
        let c = &app.renko;
        json!({
            "kind": app.chart_kind.as_str(),
            "renko": {
                "mode": c.mode.as_str(),
                "boxSize": c.box_size,
                "atrLength": c.atr_length,
                "percentage": c.percentage,
                "wicks": c.wicks,
                "source": c.source.as_str(),
            }
        })
        .to_string()
    })
}

/// Replace the option-chain level lines (separate registry from trade lines so
/// the two never wipe each other; old app's `setOcLevelLines`).
#[wasm_bindgen]
pub fn set_oc_level_lines(json: &str) {
    let lines = parse_price_lines(json);
    with_app(|app| app.oc_lines = lines);
    render_all();
}

#[wasm_bindgen]
pub fn clear_oc_level_lines() {
    with_app(|app| app.oc_lines.clear());
    render_all();
}

/// Direction-state line pushed by the OI Trend overlay. `json` is an array of
/// `{time, value}` points. Pass an empty string / empty array to clear.
#[wasm_bindgen]
pub fn set_dir_series(json: &str, color: &str, line_width: f64) {
    let parsed: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut data: Vec<Point> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for it in arr {
            if let (Some(time), Some(value)) = (
                it.get("time").and_then(|v| v.as_i64()),
                it.get("value").and_then(|v| v.as_f64()),
            ) {
                data.push(Point {
                    time,
                    value,
                    color: it.get("color").and_then(|v| v.as_str()).map(|s| s.to_string()),
                });
            }
        }
    }
    with_app(|app| {
        match &mut app.dir_overlay {
            Some(d) => {
                d.data = data;
                d.color = color.to_string();
                d.line_width = line_width;
            }
            None => {
                app.dir_overlay = Some(DirOverlay {
                    data,
                    color: color.to_string(),
                    line_width,
                    markers: Vec::new(),
                });
            }
        }
    });
    render_all();
}

/// Trend arrows / labels for the direction overlay (merged alongside the
/// indicator-owned marker arrows).
#[wasm_bindgen]
pub fn set_dir_markers(json: &str) {
    let parsed: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut markers: Vec<Marker> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for it in arr {
            if let Some(time) = it.get("time").and_then(|v| v.as_i64()) {
                markers.push(Marker {
                    time,
                    position: it
                        .get("position")
                        .and_then(|v| v.as_str())
                        .unwrap_or("aboveBar")
                        .to_string(),
                    color: it
                        .get("color")
                        .and_then(|v| v.as_str())
                        .unwrap_or("#22e08a")
                        .to_string(),
                    shape: it
                        .get("shape")
                        .and_then(|v| v.as_str())
                        .unwrap_or("arrowUp")
                        .to_string(),
                    text: it
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    size: it.get("size").and_then(|v| v.as_f64()).unwrap_or(1.0),
                });
            }
        }
    }
    with_app(|app| {
        if let Some(d) = &mut app.dir_overlay {
            d.markers = markers;
        } else {
            app.dir_overlay = Some(DirOverlay {
                data: Vec::new(),
                color: "#22e08a".into(),
                line_width: 2.0,
                markers,
            });
        }
    });
    render_all();
}

#[wasm_bindgen]
pub fn clear_dir_overlay() {
    with_app(|app| app.dir_overlay = None);
    render_all();
}

// ---------------------------------------------------------------------------
// OI Trend + Levels overlay (Rust port of the old app's `oitrend.js`)
// ---------------------------------------------------------------------------

/// True while the OI Trend toggle is on.
pub(crate) fn oi_trend_is_on() -> bool {
    read_app(|a| a.oi_enabled)
}

/// Remove every OI-trend overlay line / direction glyph.
pub(crate) fn oi_trend_clear() {
    with_app(|app| {
        app.oc_lines.clear();
        app.dir_overlay = None;
        app.oi_sig.clear();
    });
    render_oi_legend_dom("");
    render_all();
}

/// Reset all option-chain-derived chart state (PCR/IV series, OI levels &&
/// overlay, direction glyphs + legend). Called when the charted symbol changes
/// so stale analytics never bleed into the new instrument.
pub(crate) fn oc_analytics_reset() {
    with_app(|app| {
        app.oc_pcr.clear();
        app.oc_iv.clear();
        app.oc_lines.clear();
        app.dir_overlay = None;
        app.oi_records = None;
        app.oi_sig.clear();
        app.refresh_special_series();
    });
    render_oi_legend_dom("");
    render_all();
}

const LEGEND_BASE_STYLE: &str = "position:absolute;left:10px;top:8px;z-index:6;pointer-events:none;font:600 11px/1.5 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,sans-serif;background:rgba(10,10,28,0.82);border:1px solid rgba(120,120,200,0.4);border-radius:6px;padding:4px 10px;color:#e0e0ff;white-space:nowrap;";

/// Create/refresh the OI-trend legend chip in the chart corner. `html` empty
/// hides it. The markup is cached so identical frames never touch the DOM.
fn render_oi_legend_dom(html: &str) {
    if read_app(|a| a.oi_legend_html.as_str() == html) {
        return;
    }
    let Some(cont) = by_id("chart-container") else {
        return;
    };
    let el = match by_id("oiTrendLegend") {
        Some(e) => e,
        None => {
            let Ok(e) = document().create_element("div") else {
                return;
            };
            e.set_id("oiTrendLegend");
            let _ = cont.append_child(&e);
            e
        }
    };
    let style = if html.is_empty() {
        format!("{LEGEND_BASE_STYLE}display:none;")
    } else {
        format!("{LEGEND_BASE_STYLE}display:block;")
    };
    let _ = el.set_attribute("style", &style);
    if !html.is_empty() {
        el.set_inner_html(html);
    }
    with_app(|a| a.oi_legend_html = html.to_string());
}

/// Coarse candle/chain signature. Direction/level overlays are rebuilt only
/// when the forming candle or the chain snapshot actually changed.
fn oi_frame_sig(candles: &[Candle], spot: f64, rows: usize) -> String {
    match candles.last() {
        Some(c) => format!(
            "{}:{}:{:.4}:{:.0}:{:.4}:{}",
            candles.len(),
            c.time,
            c.close,
            c.volume,
            spot,
            rows
        ),
        None => format!("0:0:0:0:{spot:.4}:{rows}"),
    }
}

/// Compact OI formatter matching the old strip (`1.23L` / `34.5k`).
fn fmt_oi(v: f64) -> String {
    let a = v.abs();
    if a >= 1e7 {
        format!("{:.2}Cr", v / 1e7)
    } else if a >= 1e5 {
        format!("{:.2}L", v / 1e5)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// Build the horizontal price lines for the OI Trend level module: up to three
/// call/put walls per side, Max Pain, expected-move band, PCR-at-spot.
fn build_oc_lines(lvl: &LevelData, spot: f64) -> Vec<PriceLine> {
    let mut lines: Vec<PriceLine> = Vec::new();
    let mut push_walls = |kind: &str, color: &str, tag: &str| {
        for w in lvl.walls.iter().filter(|w| w.kind == kind).take(3) {
            let arrow = if w.chg > 0.0 { " ↑" } else { "" };
            lines.push(PriceLine {
                price: w.strike,
                color: color.to_string(),
                line_width: if w.fresh { 2.0 } else { 1.0 },
                line_style: 2,
                title: format!("{tag} {:.0} | {tag} OI {}{arrow}", w.strike, fmt_oi(w.oi)),
            });
        }
    };
    push_walls("res", "#ff5252", "RES");
    push_walls("sup", "#00d4aa", "SUP");
    if let Some(mp) = lvl.max_pain {
        lines.push(PriceLine {
            price: mp,
            color: "#b39ddb".to_string(),
            line_width: 1.0,
            line_style: 3,
            title: format!("MAX PAIN {mp:.0}"),
        });
    }
    if let (Some(hi), Some(lo)) = (lvl.exp_hi, lvl.exp_lo) {
        let hi_title = match lvl.exp_move {
            Some(m) => format!("EXP MAX {hi:.0} ({m:.0})"),
            None => format!("EXP MAX {hi:.0}"),
        };
        lines.push(PriceLine {
            price: hi,
            color: "#4fc3f7".to_string(),
            line_width: 1.0,
            line_style: 2,
            title: hi_title,
        });
        lines.push(PriceLine {
            price: lo,
            color: "#4fc3f7".to_string(),
            line_width: 1.0,
            line_style: 2,
            title: format!("EXP MIN {lo:.0}"),
        });
    }
    if let Some(pcr) = lvl.pcr {
        if spot > 0.0 {
            let mut title = format!("PCR {pcr:.2}");
            if let Some(c) = lvl.pcr_chg {
                if c.is_finite() {
                    title.push_str(&format!(" (chg {}{:.2})", if c < 0.0 { "" } else { "+" }, c));
                }
            }
            lines.push(PriceLine {
                price: spot,
                color: "rgba(255,255,255,0.6)".to_string(),
                line_width: 1.0,
                line_style: 1,
                title,
            });
        }
    }
    lines
}

/// Trend-state line plus the arrow / consolidation-glyph / flip markers the old
/// app drew on top of the candles.
fn build_dir_overlay(candles: &[Candle], reg: &RegimeResult, cls: &ClassifyResult) -> DirOverlay {
    let data: Vec<Point> = reg
        .data
        .iter()
        .map(|p| Point {
            time: p.time,
            value: p.value,
            color: None,
        })
        .collect();

    let mut markers: Vec<Marker> = Vec::new();
    if let Some(last) = candles.last() {
        let t = last.time;
        if cls.kind == "consolidation" {
            markers.push(Marker {
                time: t,
                position: "belowBar".to_string(),
                color: cls.color.clone(),
                shape: "circle".to_string(),
                text: "◀".to_string(),
                size: 1.0,
            });
            let head = if cls.label.is_empty() {
                "Consolidation Liquidity Grabbing Phase".to_string()
            } else {
                cls.label.clone()
            };
            let text = if cls.info.is_empty() {
                head
            } else {
                format!("{head} | {}", cls.info)
            };
            markers.push(Marker {
                time: t,
                position: "aboveBar".to_string(),
                color: cls.color.clone(),
                shape: "circle".to_string(),
                text,
                size: 1.0,
            });
        } else {
            let up = cls.arrow.as_deref() == Some("up");
            let (pos, opp) = if up {
                ("belowBar", "aboveBar")
            } else {
                ("aboveBar", "belowBar")
            };
            let label = if !cls.label.is_empty() {
                cls.label.clone()
            } else if cls.kind == "reversal" {
                "Reversal Point".to_string()
            } else if cls.strength.as_deref() == Some("weak") {
                "Trend Continue (weak)".to_string()
            } else {
                "Trend Continue".to_string()
            };
            markers.push(Marker {
                time: t,
                position: pos.to_string(),
                color: cls.color.clone(),
                shape: if up { "arrowUp" } else { "arrowDown" }.to_string(),
                text: label,
                size: 1.4,
            });
            if !cls.info.is_empty() {
                markers.push(Marker {
                    time: t,
                    position: opp.to_string(),
                    color: "rgba(255,255,255,0.75)".to_string(),
                    shape: "circle".to_string(),
                    text: cls.info.clone(),
                    size: 1.0,
                });
            }
        }

        // A small flip glyph where the most recent regime segment began.
        let regs = &reg.regs;
        if let Some(&cur) = regs.last() {
            let mut back = 0usize;
            for r in regs.iter().rev() {
                if *r != cur {
                    break;
                }
                back += 1;
            }
            if back >= 1 && back <= 8 && regs.len() > back {
                let idx = regs.len() - 1 - back;
                if let Some(pt) = reg.data.get(idx) {
                    let up = cur == Regime::Up;
                    markers.push(Marker {
                        time: pt.time,
                        position: if up { "belowBar" } else { "aboveBar" }.to_string(),
                        color: cls.color.clone(),
                        shape: if up { "arrowUp" } else { "arrowDown" }.to_string(),
                        text: String::new(),
                        size: 1.1,
                    });
                }
            }
        }
    }

    DirOverlay {
        data,
        color: cls.color.clone(),
        line_width: 2.0,
        markers,
    }
}

/// Legend chip markup (direction head + the classify info readout).
fn oi_legend_html(cls: &ClassifyResult, ctx: &oit::Context) -> String {
    let up = cls.arrow.as_deref() == Some("up");
    let head = match cls.kind.as_str() {
        "consolidation" => {
            let label = if cls.label.is_empty() {
                "CONSOLIDATION".to_string()
            } else {
                cls.label.clone()
            };
            format!("· {label}")
        }
        "reversal" => {
            let label = if cls.label.is_empty() {
                "REVERSAL POINT (OI wall)".to_string()
            } else {
                cls.label.clone()
            };
            format!("{} {label}", if up { "↑" } else { "↓" })
        }
        _ => {
            let strength = cls
                .strength
                .as_ref()
                .map(|s| format!(" ({s})"))
                .unwrap_or_default();
            format!(
                "{} {} · {}{strength}",
                if up { "↑" } else { "↓" },
                if up { "UP" } else { "DOWN" },
                cls.label
            )
        }
    };

    let mut parts: Vec<String> = Vec::new();
    if let Some(sc) = cls.score {
        parts.push(format!("Score {}{:.2}", if sc > 0.0 { "+" } else { "" }, sc));
    }
    if let Some(pcr) = ctx.pcr {
        let mut p = format!("PCR {pcr:.2}");
        if let Some(c) = ctx.pcr_chg {
            if c.is_finite() {
                p += &format!(" ({}{:.2})", if c < 0.0 { "" } else { "+" }, c);
            }
        }
        parts.push(p);
    }
    parts.push(format!(
        "Vol {}",
        match ctx.vol.dir {
            1 => "rising",
            -1 => "falling",
            _ => "flat",
        }
    ));
    let oi = ctx.oi.clone().unwrap_or_default();
    if oi.net.abs() >= 0.05 || oi.box_oi >= 0.2 {
        let f = |v: f64| {
            if v >= 0.99 {
                "1".to_string()
            } else {
                format!("{v:.2}")
            }
        };
        parts.push(format!("OI sup {}/res {}", f(oi.sup), f(oi.res)));
    }

    let parts_html = if parts.is_empty() {
        String::new()
    } else {
        format!(
            "<span style='color:#9fa8da;margin-left:10px'>{}</span>",
            parts.join("  ·  ")
        )
    };
    format!(
        "<b style='color:{};font-size:12px'>{head}</b>{parts_html}",
        cls.color
    )
}

/// Rebuild levels + direction overlay from the cached chain snapshot and the
/// live candle series. Skips work when the frame signature is unchanged.
fn oi_trend_recompute() {
    let (records, spot, dte, candles) = match read_app(|a| {
        (
            a.oi_records.clone(),
            a.oi_spot,
            a.oi_dte,
            a.candles.clone(),
        )
    }) {
        (Some(r), s, d, c) if !r.is_empty() => (r, s, d, c),
        _ => return,
    };

    let sig = oi_frame_sig(&candles, spot, records.len());
    if read_app(|a| a.oi_sig == sig) {
        return;
    }

    let lvl = oit::level_data(
        &records,
        spot,
        &LevelOpts {
            dte_days: if dte > 0.0 { dte } else { 7.0 },
            ..Default::default()
        },
    );

    // Option-premium chart (old `_chartKind === 'opt'`): the level lines and
    // direction glyphs are cleared and the strike-ordered OI profile strip is
    // shown instead.
    if crate::optionchain::chart_open_now() {
        let rows = oit::oi_rows(&records, spot, 0.12);
        with_app(|app| {
            app.oc_lines.clear();
            app.dir_overlay = None;
            app.oi_sig = sig;
        });
        render_oi_legend_dom("");
        draw_oi_strip(&lvl, &rows, spot);
        return;
    }
    hide_oi_strip();

    let lines = build_oc_lines(&lvl, spot);

    let mut overlay: Option<DirOverlay> = None;
    let mut legend = String::new();
    if candles.len() >= 40 {
        let reg = oit::regime_line(&candles, &RegimeOpts::default());
        let ctx = oit::context_of(&candles, &lvl, &ContextOpts::default());
        let cls = oit::classify(&reg.last, &ctx, &ClassifyOpts::default());
        legend = oi_legend_html(&cls, &ctx);
        overlay = Some(build_dir_overlay(&candles, &reg, &cls));
    }

    with_app(|app| {
        app.oc_lines = lines;
        app.dir_overlay = overlay;
        app.oi_sig = sig;
    });
    render_oi_legend_dom(&legend);
}

const STRIP_HEAD_STYLE: &str = "display:flex;gap:10px;flex-wrap:wrap;font-size:10px;color:#9fa8da;align-items:center;line-height:1.4;padding:0 2px 4px;";

fn strip_host_style(display: &str) -> String {
    format!(
        "display:{display};border-top:1px solid #1e1e40;background:#0b0b1e;padding:4px 6px 2px;"
    )
}

fn hide_oi_strip() {
    if let Some(el) = by_id("oiStripHost") {
        let hidden = format!("display:none;border-top:1px solid #1e1e40;background:#0b0b1e;padding:4px 6px 2px;");
        if el.get_attribute("style").as_deref() != Some(hidden.as_str()) {
            let _ = el.set_attribute("style", &hidden);
        }
    }
}

/// Create the strip host/canvas/header once, right under the chart area
/// (between `.chart-wrap` and the indicator panes).
fn ensure_oi_strip() -> Option<(Element, HtmlCanvasElement, Element)> {
    let host = match by_id("oiStripHost") {
        Some(h) => h,
        None => {
            // Insert after the flex-grown chart column so the fixed-height strip
            // shrinks the chart instead of overflowing it.
            let wrap = document().query_selector(".chart-wrap").ok().flatten()?;
            let parent = wrap.parent_element()?;
            let h = document().create_element("div").ok()?;
            h.set_id("oiStripHost");
            let head = document().create_element("div").ok()?;
            head.set_id("oiStripHead");
            let _ = head.set_attribute("style", STRIP_HEAD_STYLE);
            let cv = document().create_element("canvas").ok()?;
            cv.set_id("oiStripCv");
            let _ = cv.set_attribute("style", "display:block;width:100%;height:96px;");
            let _ = h.append_child(&head);
            let _ = h.append_child(&cv);
            let _ = parent.insert_before(&h, wrap.next_sibling().as_ref());
            h
        }
    };
    let head = by_id("oiStripHead")?;
    let cv = by_id("oiStripCv")?.dyn_into::<HtmlCanvasElement>().ok()?;
    Some((host, cv, head))
}

fn fmt_num(v: f64) -> String {
    if !v.is_finite() {
        return "--".to_string();
    }
    if v.abs() >= 1000.0 {
        let neg = v < 0.0;
        let digits = format!("{:.0}", v.abs());
        let mut out = String::new();
        for (i, ch) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        if neg {
            out.insert(0, '-');
        }
        out
    } else {
        format!("{:.2}", (v * 100.0).round() / 100.0)
    }
}

fn chip_html(label: &str, val: &str, label_color: &str, bold_color: &str) -> String {
    format!(
        "<span style='white-space:nowrap;color:{label_color}'>{label} <b style='color:{bold_color}'>{val}</b></span>"
    )
}

/// Strike-ordered CE (left) / PE (right) OI profile drawn under the option
/// premium chart. Mirrors `drawStrip()` in the old app (700ms throttle).
fn draw_oi_strip(lvl: &LevelData, rows: &[oit::OiRow], spot: f64) {
    let Some((host, cv, head)) = ensure_oi_strip() else {
        return;
    };
    let _ = host.set_attribute("style", &strip_host_style("block"));

    if rows.is_empty() {
        head.set_inner_html(&chip_html(
            "OI",
            "no active-OI strikes in range",
            "#9fa8da",
            "#ffb74d",
        ));
        return;
    }

    let now = js_sys::Date::now();
    let redraw = now - read_app(|a| a.oi_strip_last) >= 700.0;

    if redraw {
        let tot_ce: f64 = rows.iter().map(|r| r.ce_oi).sum();
        let tot_pe: f64 = rows.iter().map(|r| r.pe_oi).sum();
        let mut html = chip_html("SPOT", &fmt_num(spot), "#ffffff", "#ffd54f");
        if let Some(mp) = lvl.max_pain {
            html += &chip_html("MAX PAIN", &fmt_num(mp), "#b39ddb", "#b39ddb");
        }
        if let Some(pcr) = lvl.pcr {
            let mut t = format!("{pcr:.2}");
            if let Some(c) = lvl.pcr_chg {
                if c.is_finite() {
                    t += &format!(" chg {}{:.2}", if c < 0.0 { "" } else { "+" }, c);
                }
            }
            html += &chip_html(
                "PCR",
                &t,
                "#9fa8da",
                if pcr >= 1.0 { "#00d4aa" } else { "#ff5252" },
            );
        }
        if let (Some(hi), Some(lo)) = (lvl.exp_hi, lvl.exp_lo) {
            html += &chip_html(
                "EXP",
                &format!("{} - {}", fmt_num(lo), fmt_num(hi)),
                "#4fc3f7",
                "#4fc3f7",
            );
        }
        html += &chip_html("CE OI", &fmt_oi(tot_ce), "#ff5252", "#ff8a80");
        html += &chip_html("PE OI", &fmt_oi(tot_pe), "#00d4aa", "#80cbc4");
        head.set_inner_html(&html);
    }

    let w = (cv.client_width().max(120)) as f64;
    if !redraw && (read_app(|a| a.oi_strip_w) - w).abs() < 0.5 {
        return;
    }
    let h = (cv.client_height().max(60)) as f64;
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0);
    let pw = (w * dpr).round() as u32;
    let ph = (h * dpr).round() as u32;
    if cv.width() != pw {
        cv.set_width(pw);
    }
    if cv.height() != ph {
        cv.set_height(ph);
    }
    let ctx = match cv.get_context("2d").ok().flatten() {
        Some(o) => match o.dyn_into::<CanvasRenderingContext2d>() {
            Ok(c) => c,
            Err(_) => return,
        },
        None => return,
    };
    let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
    ctx.clear_rect(0.0, 0.0, w, h);

    let (plot_l, plot_r, plot_t, plot_b) = (4.0, w - 4.0, 3.0, h - 5.0);
    let min_s = rows.first().unwrap().strike;
    let max_s = rows.last().unwrap().strike;
    let span = if (max_s - min_s).abs() < f64::EPSILON {
        1.0
    } else {
        max_s - min_s
    };
    let y_of = |s: f64| plot_b - ((s - min_s) / span) * (plot_b - plot_t);
    let center_x = (plot_l + plot_r) / 2.0;
    let half_w = ((center_x - plot_l).min(plot_r - center_x) - 4.0).max(8.0);
    let bar_h = ((plot_b - plot_t) / (rows.len().max(1) as f64) * 0.55)
        .min(2.6)
        .max(1.2);

    let mut max_oi = 1.0_f64;
    let mut hi_ce = 0usize;
    let mut hi_pe = 0usize;
    for (i, r) in rows.iter().enumerate() {
        if r.ce_oi > max_oi {
            max_oi = r.ce_oi;
        }
        if r.pe_oi > max_oi {
            max_oi = r.pe_oi;
        }
        if r.ce_oi > rows[hi_ce].ce_oi {
            hi_ce = i;
        }
        if r.pe_oi > rows[hi_pe].pe_oi {
            hi_pe = i;
        }
    }

    // Centre axis.
    set_stroke(&ctx, "rgba(255,255,255,0.22)");
    ctx.set_line_width(1.0);
    let _ = ctx.set_line_dash(&Array::new());
    ctx.begin_path();
    let cx = center_x.round() + 0.5;
    ctx.move_to(cx, plot_t);
    ctx.line_to(cx, plot_b);
    ctx.stroke();

    // Extremity strike labels.
    ctx.set_text_align("left");
    ctx.set_text_baseline("middle");
    ctx.set_font("8px sans-serif");
    set_fill(&ctx, "rgba(255,255,255,0.35)");
    let _ = ctx.fill_text(&format!("{min_s:.0}"), plot_l, plot_b);
    ctx.set_text_align("right");
    let _ = ctx.fill_text(&format!("{max_s:.0}"), plot_r, plot_t + 1.0);

    for (i, r) in rows.iter().enumerate() {
        let y = y_of(r.strike).round();
        let c_bar = ((r.ce_oi / max_oi) * half_w).max(1.5);
        let p_bar = ((r.pe_oi / max_oi) * half_w).max(1.5);
        set_fill(
            &ctx,
            if i == hi_ce {
                "rgba(255,138,128,0.95)"
            } else {
                "rgba(255,82,82,0.7)"
            },
        );
        let _ = ctx.fill_rect(center_x - c_bar, y - bar_h / 2.0, c_bar, bar_h);
        set_fill(
            &ctx,
            if i == hi_pe {
                "rgba(128,203,196,0.95)"
            } else {
                "rgba(0,212,170,0.7)"
            },
        );
        let _ = ctx.fill_rect(center_x, y - bar_h / 2.0, p_bar, bar_h);
    }

    let dash_line = |y: f64, color: &str, dash: &[f64], alpha: f64| {
        let arr = Array::new();
        for d in dash {
            arr.push(&JsValue::from_f64(*d));
        }
        let _ = ctx.set_line_dash(&arr);
        set_stroke(&ctx, color);
        ctx.set_global_alpha(alpha);
        ctx.begin_path();
        ctx.move_to(plot_l, y);
        ctx.line_to(plot_r, y);
        ctx.stroke();
        let _ = ctx.set_line_dash(&Array::new());
        ctx.set_global_alpha(1.0);
    };

    if spot > 0.0 && spot >= min_s && spot <= max_s {
        let ys = y_of(spot).round();
        dash_line(ys, "#ffd54f", &[5.0, 4.0], 0.9);
        set_fill(&ctx, "#ffd54f");
        ctx.set_font("bold 9px sans-serif");
        ctx.set_text_align("left");
        ctx.set_text_baseline("bottom");
        let _ = ctx.fill_text(&format!("SPOT {}", fmt_num(spot)), plot_l + 2.0, ys - 1.0);
    }
    if let Some(mp) = lvl.max_pain {
        if mp >= min_s && mp <= max_s {
            let ym = y_of(mp).round();
            dash_line(ym, "#b39ddb", &[2.0, 3.0], 0.7);
            ctx.set_font("9px sans-serif");
            ctx.set_text_align("right");
            ctx.set_text_baseline("top");
            set_fill(&ctx, "#b39ddb");
            let _ = ctx.fill_text(&format!("MP {}", fmt_num(mp)), plot_r - 2.0, ym + 1.0);
        }
    }
    let sel_s = crate::optionchain::selected_strike_now();
    if sel_s > 0.0 && sel_s >= min_s && sel_s <= max_s {
        let ys2 = y_of(sel_s).round();
        set_fill(&ctx, "rgba(255,255,255,0.6)");
        let _ = ctx.fill_rect(center_x - 0.5, ys2 - 3.0, 1.0, 6.0);
    }

    with_app(|a| {
        a.oi_strip_last = now;
        a.oi_strip_w = w;
    });
}

/// Per-frame hook: while the OI toggle is on, keep the direction overlay +
/// legend glued to the forming candle (old app's 1s `onTick`). Cheap no-op when
/// disabled, when no chain is cached, or when the frame is unchanged.
pub(crate) fn oi_trend_tick() {
    if !oi_trend_is_on() {
        return;
    }
    if read_app(|a| a.oi_records.is_none()) {
        return;
    }
    oi_trend_recompute();
}

/// Recompute the OI Trend + Levels overlay from a chain snapshot. Called by the
/// option-chain tab after every render; a no-op while the toggle is off.
///
/// `records` is the full chain (all strikes visible to the scrip master), not
/// just the ATM window, so wall/Max-Pain detection matches the old app.
pub(crate) fn oi_trend_apply(records: Vec<OiRecord>, spot: f64, dte_days: f64) {
    if !oi_trend_is_on() {
        return;
    }
    with_app(|app| {
        app.oi_records = Some(records);
        app.oi_spot = spot;
        app.oi_dte = if dte_days > 0.0 { dte_days } else { 7.0 };
        app.oi_sig.clear();
    });
    oi_trend_recompute();
    render_all();
}

/// Enable/disable the OI Trend overlay from outside the toggle handler (the
/// old-app global `OITrend.setEnabled`).
#[wasm_bindgen]
pub fn set_oi_trend_enabled(on: bool) {
    with_app(|app| app.oi_enabled = on);
    if on {
        crate::optionchain::ensure_loaded();
        crate::optionchain::sync_oi_trend_now();
    } else {
        oi_trend_clear();
    }
}

/// Push-style realtime hook. The sidebar calls this on every quote update
/// (`render`) so the forming candle tracks the tape immediately instead of
/// waiting for the 1s fallback ticker. No-op when the chart's realtime toggle
/// is off or no live price is cached yet.
#[wasm_bindgen]
pub fn live_tick() {
    if !read_app(|a| a.realtime) {
        return;
    }
    apply_live_tick();
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod pos_tests {
    use super::*;

    fn candle(t: i64, px: f64) -> Candle {
        Candle { time: t, open: px, high: px + 1.0, low: px - 1.0, close: px, volume: 10.0 }
    }

    fn app_with(n: usize) -> App {
        let mut a = App::new();
        a.candles =
            (0..n).map(|i| candle(1_700_000_000 + i as i64 * 300, 100.0 + i as f64)).collect();
        a.view_start = 0.0;
        a.view_count = n as f64;
        a
    }

    #[test]
    fn time_index_round_trip() {
        let app = app_with(50);
        for i in 0..50 {
            let t = app.candles[i].time;
            let fi = frac_index(&app, t);
            assert!((fi - i as f64).abs() < 1e-6, "fi {fi} i {i}");
            assert_eq!(time_for_index(&app, fi), t);
        }
        let last = app.candles[49].time;
        assert_eq!(time_for_index(&app, 50.0), last + 300);
        assert_eq!(time_for_index(&app, 51.5), last + 750);
    }

    #[test]
    fn price_y_round_trip() {
        let plot = Plot { left: 4.0, right: 796.0, top: 10.0, bottom: 410.0, bar_w: 8.0 };
        let (lo, hi) = (100.0, 200.0);
        for px in [100.0, 123.4, 150.0, 199.9, 200.0] {
            let y = y_for(&plot, lo, hi, px);
            let back = price_at_y(&plot, lo, hi, y);
            assert!((back - px).abs() < 1e-9, "px {px} back {back}");
        }
        assert!((price_at_y(&plot, lo, hi, plot.top) - hi).abs() < 1e-9);
        assert!((price_at_y(&plot, lo, hi, plot.bottom) - lo).abs() < 1e-9);
        assert!(price_at_y(&plot, lo, hi, plot.bottom + 50.0) < lo);
    }

    #[test]
    fn pos_levels_tracks_cursor_and_keeps_profit_on_the_right_side() {
        // Cursor on the profit side -> it lands on the target, stop mirrors.
        let (t, s) = pos_levels(PosDir::Long, 100.0, 130.0);
        assert!((t - 130.0).abs() < 1e-9 && (s - 70.0).abs() < 1e-9, "long-up {t}/{s}");
        let (t, s) = pos_levels(PosDir::Short, 100.0, 70.0);
        assert!((t - 70.0).abs() < 1e-9 && (s - 130.0).abs() < 1e-9, "short-down {t}/{s}");
        // Cursor on the loss side -> it lands on the stop, target mirrors. The box
        // still tracks the cursor (this is the "inverted" feel being fixed).
        let (t, s) = pos_levels(PosDir::Long, 100.0, 80.0);
        assert!((t - 120.0).abs() < 1e-9 && (s - 80.0).abs() < 1e-9, "long-down {t}/{s}");
        let (t, s) = pos_levels(PosDir::Short, 100.0, 130.0);
        assert!((t - 70.0).abs() < 1e-9 && (s - 130.0).abs() < 1e-9, "short-up {t}/{s}");
        // Profit always on the correct side of entry, regardless of drag direction.
        for (dir, cur) in [
            (PosDir::Long, 130.0),
            (PosDir::Long, 80.0),
            (PosDir::Short, 70.0),
            (PosDir::Short, 130.0),
        ] {
            let (t, s) = pos_levels(dir, 100.0, cur);
            match dir {
                PosDir::Long => assert!(t > 100.0 && s < 100.0),
                PosDir::Short => assert!(t < 100.0 && s > 100.0),
            }
            assert!((t - 100.0).abs() == (100.0 - s).abs(), "risk != reward");
        }
    }

    #[test]
    fn position_json_round_trip() {
        let p = PositionTool {
            dir: PosDir::Short,
            entry_time: 1_700_000_000,
            end_time: 1_700_000_300,
            entry_price: 21500.5,
            target_price: 21450.0,
            stop_price: 21551.0,
        };
        let back = pos_from_json(&pos_to_json(&p)).expect("parse");
        assert_eq!(back.dir, p.dir);
        assert_eq!(back.entry_time, p.entry_time);
        assert_eq!(back.end_time, p.end_time);
        assert!((back.entry_price - p.entry_price).abs() < 1e-9);
        assert!((back.target_price - p.target_price).abs() < 1e-9);
        assert!((back.stop_price - p.stop_price).abs() < 1e-9);
        let v = json!({ "dir": "long", "entryTime": 5, "entryPrice": 1.0, "targetPrice": 2.0, "stopPrice": 0.5 });
        assert_eq!(pos_from_json(&v).unwrap().end_time, 5);
        assert!(pos_from_json(&json!({ "dir": "long" })).is_none());
    }

    #[test]
    fn hit_test_finds_target_stop_entry_and_body() {
        let mut app = app_with(50);
        app.positions.push(PositionTool {
            dir: PosDir::Long,
            entry_time: app.candles[10].time,
            end_time: app.candles[30].time,
            entry_price: 150.0,
            target_price: 190.0,
            stop_price: 110.0,
        });
        let plot = plot_for(&app, 800.0, 420.0);
        let (lo, hi) = price_extent(&app, &plot);
        let x = x_for_time(&app, &plot, app.candles[10].time);
        let y_t = y_for(&plot, lo, hi, 190.0);
        let y_s = y_for(&plot, lo, hi, 110.0);
        let y_e = y_for(&plot, lo, hi, 150.0);
        assert_eq!(pos_hit(&app, &plot, lo, hi, x, y_t), Some((0, PosEditKind::Target)));
        assert_eq!(pos_hit(&app, &plot, lo, hi, x, y_s), Some((0, PosEditKind::Stop)));
        assert_eq!(pos_hit(&app, &plot, lo, hi, x, y_e), Some((0, PosEditKind::Move)));
        // inside the body, away from every level
        let y_mid = (y_e + y_t) / 2.0;
        assert_eq!(pos_hit(&app, &plot, lo, hi, x, y_mid), Some((0, PosEditKind::Move)));
        // far away -> nothing
        assert_eq!(pos_hit(&app, &plot, lo, hi, x - 400.0, y_e), None);
    }

    #[test]
    fn rebuild_view_applies_the_chart_kind() {
        let mut a = app_with(30);
        a.raw = a.candles.clone();
        // Candles: identity.
        a.chart_kind = ChartKind::Candles;
        a.rebuild_view();
        assert_eq!(a.candles.len(), a.raw.len());
        assert!((a.candles[5].close - a.raw[5].close).abs() < 1e-9);
        // Line: identity OHLC (drawn as a polyline), same length.
        a.chart_kind = ChartKind::Line;
        a.rebuild_view();
        assert_eq!(a.candles.len(), a.raw.len());
        assert!((a.candles[5].close - a.raw[5].close).abs() < 1e-9);
        // Heikin Ashi: rewritten OHLC, same length / timestamps.
        a.chart_kind = ChartKind::HeikinAshi;
        a.rebuild_view();
        assert_eq!(a.candles.len(), a.raw.len());
        assert_eq!(a.candles[5].time, a.raw[5].time);
        let ha_close = (a.raw[5].open + a.raw[5].high + a.raw[5].low + a.raw[5].close) / 4.0;
        assert!((a.candles[5].close - ha_close).abs() < 1e-9);
        // Renko: rebuilt into bricks.
        a.chart_kind = ChartKind::Renko;
        a.renko = RenkoConfig { box_size: 2.0, ..Default::default() };
        a.rebuild_view();
        assert!(!a.candles.is_empty());
        for b in &a.candles {
            assert!(((b.close - b.open).abs() - 2.0).abs() < 1e-9 || (b.close - b.open).abs() < 1e-9);
        }
    }

    #[test]
    fn parse_renko_config_defaults_and_overrides() {
        let d = parse_renko_config("not json");
        assert_eq!(d.mode, RenkoMode::Traditional);
        assert!((d.box_size - 10.0).abs() < 1e-9);
        let c = parse_renko_config(
            r#"{"renko":{"mode":"atr","atrLength":7,"percentage":0.5,"wicks":true,"source":"high/low","boxSize":-1}}"#,
        );
        assert_eq!(c.mode, RenkoMode::Atr);
        assert_eq!(c.atr_length, 7);
        assert!((c.percentage - 0.5).abs() < 1e-9);
        assert!(c.wicks);
        assert_eq!(c.source, RenkoSource::HighLow);
        assert!((c.box_size - 10.0).abs() < 1e-9, "non-positive box keeps the default");
    }
}

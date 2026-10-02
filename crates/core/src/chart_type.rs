//! Chart-type transforms: raw OHLC candles -> the series the chart draws (and
//! feeds to indicators), matching TradingView's chart-type behaviour.
//!
//! * `HeikinAshi` keeps the candle timestamps and rewrites OHLC with the
//!   Heikin-Ashi formulas.
//! * `Renko` rebuilds the series into price bricks. Bricks are uniformly spaced
//!   (the chart already lays bars out by index), so only the brick's timestamp
//!   and OHLCV matter. Brick size supports the three TradingView modes
//!   (Traditional / ATR / Percentage), a Close or High-Low source, and optional
//!   wicks.

use crate::model::Candle;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChartKind {
    Candles,
    Line,
    HeikinAshi,
    Renko,
}

impl ChartKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ChartKind::Candles => "candles",
            ChartKind::Line => "line",
            ChartKind::HeikinAshi => "heikin_ashi",
            ChartKind::Renko => "renko",
        }
    }

    pub fn parse(s: &str) -> ChartKind {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "line" => ChartKind::Line,
            "heikin_ashi" | "heikinashi" | "ha" => ChartKind::HeikinAshi,
            "renko" => ChartKind::Renko,
            _ => ChartKind::Candles,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenkoMode {
    Traditional,
    Atr,
    Percentage,
}

impl RenkoMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RenkoMode::Traditional => "traditional",
            RenkoMode::Atr => "atr",
            RenkoMode::Percentage => "percentage",
        }
    }

    pub fn parse(s: &str) -> RenkoMode {
        match s.trim().to_ascii_lowercase().as_str() {
            "atr" => RenkoMode::Atr,
            "percentage" | "percent" | "pct" => RenkoMode::Percentage,
            _ => RenkoMode::Traditional,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenkoSource {
    Close,
    HighLow,
}

impl RenkoSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RenkoSource::Close => "close",
            RenkoSource::HighLow => "highlow",
        }
    }

    pub fn parse(s: &str) -> RenkoSource {
        match s.trim().to_ascii_lowercase().replace([' ', '/'], "").as_str() {
            "highlow" | "hl" => RenkoSource::HighLow,
            _ => RenkoSource::Close,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenkoConfig {
    pub mode: RenkoMode,
    /// Fixed brick size (price units) for `Traditional`.
    pub box_size: f64,
    /// ATR lookback for `Atr`.
    pub atr_length: usize,
    /// Brick size as a percent of price for `Percentage`.
    pub percentage: f64,
    pub wicks: bool,
    pub source: RenkoSource,
}

impl Default for RenkoConfig {
    fn default() -> Self {
        RenkoConfig {
            mode: RenkoMode::Traditional,
            box_size: 10.0,
            atr_length: 14,
            percentage: 1.0,
            wicks: false,
            source: RenkoSource::Close,
        }
    }
}

/// Build the display series for a chart kind from the raw candles. `cfg` is
/// ignored unless the kind is `Renko`.
pub fn build(kind: ChartKind, candles: &[Candle], cfg: &RenkoConfig) -> Vec<Candle> {
    match kind {
        ChartKind::Candles | ChartKind::Line => candles.to_vec(),
        ChartKind::HeikinAshi => heikin_ashi(candles),
        ChartKind::Renko => renko(candles, cfg),
    }
}

/// Heikin-Ashi transform. Timestamps and volume are carried over unchanged.
pub fn heikin_ashi(candles: &[Candle]) -> Vec<Candle> {
    let mut out = Vec::with_capacity(candles.len());
    let mut prev_open = 0.0f64;
    let mut prev_close = 0.0f64;
    for (i, c) in candles.iter().enumerate() {
        let ha_close = (c.open + c.high + c.low + c.close) / 4.0;
        let ha_open = if i == 0 {
            (c.open + c.close) / 2.0
        } else {
            (prev_open + prev_close) / 2.0
        };
        let ha_high = c.high.max(ha_open).max(ha_close);
        let ha_low = c.low.min(ha_open).min(ha_close);
        out.push(Candle {
            time: c.time,
            open: ha_open,
            high: ha_high,
            low: ha_low,
            close: ha_close,
            volume: c.volume,
        });
        prev_open = ha_open;
        prev_close = ha_close;
    }
    out
}

/// Wilder-smoothed ATR over `length`, evaluated at bar `idx` (inclusive).
fn wilder_atr(candles: &[Candle], length: usize, idx: usize) -> f64 {
    if candles.is_empty() {
        return 0.0;
    }
    let length = length.max(1);
    let idx = idx.min(candles.len() - 1);
    let tr = |i: usize| -> f64 {
        if i == 0 {
            (candles[0].high - candles[0].low).abs()
        } else {
            let prev = candles[i - 1].close;
            (candles[i].high - candles[i].low)
                .abs()
                .max((candles[i].high - prev).abs())
                .max((candles[i].low - prev).abs())
        }
    };
    if idx == 0 {
        return tr(0);
    }
    let seed_n = length.min(idx + 1);
    let mut atr: f64 = (0..seed_n).map(tr).sum::<f64>() / seed_n as f64;
    if idx + 1 <= length {
        return atr;
    }
    for i in length..=idx {
        atr = (atr * (length as f64 - 1.0) + tr(i)) / length as f64;
    }
    atr
}

fn brick_size(cfg: &RenkoConfig, candles: &[Candle], idx: usize, base: f64) -> f64 {
    match cfg.mode {
        RenkoMode::Traditional => cfg.box_size,
        RenkoMode::Percentage => base.abs().max(1e-9) * cfg.percentage / 100.0,
        RenkoMode::Atr => wilder_atr(candles, cfg.atr_length, idx),
    }
    .max(1e-9)
}

/// Renko transform (classic gap-free bricks):
/// * continuation bricks chain off the previous brick's close (one box moves),
/// * reversal bricks chain off the previous brick's open (the classic two-box
///   reversal), so bricks tile the price axis without gaps.
///
/// Timestamps follow the raw bar that produced the brick, nudged forward by a
/// second when several bricks land on the same bar so the series stays strictly
/// increasing (position anchors / timeline search rely on it).
pub fn renko(candles: &[Candle], cfg: &RenkoConfig) -> Vec<Candle> {
    if candles.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Candle> = Vec::new();
    let base = candles[0].close;
    let mut last: Option<Candle> = None;
    let mut last_time = candles[0].time - 1;
    let mut run_hi = candles[0].high;
    let mut run_lo = candles[0].low;

    for (idx, c) in candles.iter().enumerate() {
        run_hi = run_hi.max(c.high);
        run_lo = run_lo.min(c.low);
        let triggers: [Option<f64>; 2] = match cfg.source {
            RenkoSource::Close => [Some(c.close), None],
            RenkoSource::HighLow => [Some(c.high), Some(c.low)],
        };
        for t in triggers.into_iter().flatten() {
            loop {
                let reference = last.as_ref().map(|b| b.close).unwrap_or(base);
                let boxsz = brick_size(cfg, candles, idx, reference);
                let (open, close) = match &last {
                    None => {
                        if t >= base + boxsz {
                            (base, base + boxsz)
                        } else if t <= base - boxsz {
                            (base, base - boxsz)
                        } else {
                            break;
                        }
                    }
                    Some(b) => {
                        if b.close >= b.open {
                            if t >= b.close + boxsz {
                                (b.close, b.close + boxsz)
                            } else if t <= b.open - boxsz {
                                (b.open, b.open - boxsz)
                            } else {
                                break;
                            }
                        } else if t <= b.close - boxsz {
                            (b.close, b.close - boxsz)
                        } else if t >= b.open + boxsz {
                            (b.open, b.open + boxsz)
                        } else {
                            break;
                        }
                    }
                };
                last_time += 1;
                let time = c.time.max(last_time);
                last_time = time;
                let (high, low) = if cfg.wicks {
                    (open.max(close).max(run_hi), open.min(close).min(run_lo))
                } else {
                    (open.max(close), open.min(close))
                };
                let brick = Candle {
                    time,
                    open,
                    high,
                    low,
                    close,
                    volume: c.volume,
                };
                out.push(brick.clone());
                last = Some(brick);
                // A brick consumed the range seen so far; the next brick's wick
                // starts from this bar's own extremes.
                run_hi = c.high;
                run_lo = c.low;
            }
        }
    }

    // A flat / too-quiet series produces no bricks; show a single flat brick so
    // the chart is not blank.
    if out.is_empty() {
        let vol: f64 = candles.iter().map(|c| c.volume).sum();
        out.push(Candle {
            time: candles[0].time,
            open: base,
            high: base,
            low: base,
            close: base,
            volume: vol,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(time: i64, o: f64, h: f64, l: f64, cl: f64) -> Candle {
        Candle { time, open: o, high: h, low: l, close: cl, volume: 1.0 }
    }

    #[test]
    fn candles_kind_is_identity() {
        let src = vec![c(1, 1.0, 2.0, 0.5, 1.5)];
        let built = build(ChartKind::Candles, &src, &RenkoConfig::default());
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].time, src[0].time);
        assert!((built[0].open - src[0].open).abs() < 1e-9);
        assert!((built[0].high - src[0].high).abs() < 1e-9);
        assert!((built[0].low - src[0].low).abs() < 1e-9);
        assert!((built[0].close - src[0].close).abs() < 1e-9);
    }

    #[test]
    fn line_kind_is_identity_but_parses() {
        let src = vec![c(1, 1.0, 2.0, 0.5, 1.5), c(2, 1.5, 3.0, 1.0, 2.5)];
        let built = build(ChartKind::Line, &src, &RenkoConfig::default());
        assert_eq!(built.len(), src.len());
        assert!((built[1].close - src[1].close).abs() < 1e-9);
        assert_eq!(ChartKind::parse("Line"), ChartKind::Line);
        assert_eq!(ChartKind::Line.as_str(), "line");
    }

    #[test]
    fn heikin_ashi_matches_the_standard_formulas() {
        let src = vec![
            c(1, 10.0, 12.0, 9.0, 11.0),  // ha_close 10.5, ha_open 10.5
            c(2, 11.0, 13.0, 10.0, 12.0), // ha_close 11.5, ha_open 10.5
        ];
        let ha = heikin_ashi(&src);
        assert_eq!(ha.len(), 2);
        assert!((ha[0].close - 10.5).abs() < 1e-9);
        assert!((ha[0].open - 10.5).abs() < 1e-9);
        assert!((ha[0].high - 12.0).abs() < 1e-9);
        assert!((ha[0].low - 9.0).abs() < 1e-9);
        // open = (prev_open + prev_close)/2 = (10.5+10.5)/2 = 10.5
        assert!((ha[1].open - 10.5).abs() < 1e-9);
        assert!((ha[1].close - 11.5).abs() < 1e-9);
        assert_eq!(ha[1].time, 2);
        assert_eq!(ha[1].volume, 1.0);
    }

    #[test]
    fn heikin_ashi_second_open_uses_previous_ha_bar() {
        let src = vec![
            c(1, 100.0, 110.0, 90.0, 100.0), // ha_close 100, ha_open 100
            c(2, 100.0, 130.0, 100.0, 120.0), // ha_close 112.5, ha_open 100
            c(3, 120.0, 140.0, 110.0, 130.0), // ha_open (100+112.5)/2 = 106.25
        ];
        let ha = heikin_ashi(&src);
        assert!((ha[1].close - 112.5).abs() < 1e-9);
        assert!((ha[2].open - 106.25).abs() < 1e-9);
    }

    #[test]
    fn renko_traditional_builds_continuous_bricks() {
        let mut src = Vec::new();
        for (i, p) in [100.0, 101.0, 102.0, 105.0, 110.0, 111.0, 112.0, 108.0, 104.0, 100.0, 96.0, 90.0]
            .iter()
            .enumerate()
        {
            src.push(c(i as i64 + 1, *p, p + 1.0, p - 1.0, *p));
        }
        let cfg = RenkoConfig { box_size: 5.0, ..Default::default() };
        let bricks = renko(&src, &cfg);
        assert!(!bricks.is_empty());
        // Every brick is exactly one box tall (no wicks).
        for b in &bricks {
            assert!((b.high - b.low - 5.0).abs() < 1e-9 || (b.high - b.low).abs() < 1e-9);
            assert!(((b.close - b.open).abs() - 5.0).abs() < 1e-9);
        }
        // Up bricks chain close -> open and reversals chain open -> open.
        for w in bricks.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if b.close > b.open {
                if a.close > a.open {
                    assert!((b.open - a.close).abs() < 1e-9, "up continuation");
                } else {
                    assert!((b.open - a.open).abs() < 1e-9, "up reversal");
                }
            }
        }
    }

    #[test]
    fn renko_reversal_needs_two_boxes() {
        // Rise to 110 (two up bricks), then fall only one box to 105: no
        // reversal brick. Fall to 100 (two boxes from the top): one down brick.
        let src = vec![
            c(1, 100.0, 100.0, 100.0, 100.0),
            c(2, 105.0, 105.0, 105.0, 105.0),
            c(3, 110.0, 110.0, 110.0, 110.0),
            c(4, 105.0, 105.0, 105.0, 105.0),
        ];
        let cfg = RenkoConfig { box_size: 5.0, ..Default::default() };
        let bricks = renko(&src, &cfg);
        assert_eq!(bricks.len(), 2, "no reversal until two boxes");
        assert!(bricks.iter().all(|b| b.close > b.open));
        let mut src2 = src.clone();
        src2.push(c(5, 100.0, 100.0, 100.0, 100.0));
        let bricks2 = renko(&src2, &cfg);
        assert_eq!(bricks2.len(), 3);
        assert!(bricks2[2].close < bricks2[2].open, "down reversal brick");
    }

    #[test]
    fn renko_high_low_source_triggers_on_extremes() {
        // Close never moves a full box, but the high pierces it.
        let src = vec![
            c(1, 100.0, 100.5, 99.5, 100.0),
            c(2, 100.0, 106.0, 100.0, 101.0),
        ];
        let close_cfg = RenkoConfig { box_size: 5.0, source: RenkoSource::Close, ..Default::default() };
        let hl_cfg = RenkoConfig { box_size: 5.0, source: RenkoSource::HighLow, ..Default::default() };
        let close_bricks = renko(&src, &close_cfg);
        assert_eq!(close_bricks.len(), 1);
        assert!((close_bricks[0].close - close_bricks[0].open).abs() < 1e-9, "flat fallback");
        let hl = renko(&src, &hl_cfg);
        assert_eq!(hl.len(), 1);
        assert!((hl[0].close - hl[0].open - 5.0).abs() < 1e-9, "high/low mode forms a real brick");
    }

    #[test]
    fn renko_atr_mode_is_dynamic() {
        let mut src = Vec::new();
        let mut p = 100.0;
        for i in 0..40 {
            p += if i % 2 == 0 { 3.0 } else { -1.0 };
            src.push(c(i as i64 + 1, p, p + 2.0, p - 2.0, p));
        }
        let cfg = RenkoConfig { mode: RenkoMode::Atr, atr_length: 5, ..Default::default() };
        let bricks = renko(&src, &cfg);
        assert!(!bricks.is_empty());
        // Brick heights vary with ATR (never a constant fixed box).
        let heights: Vec<f64> = bricks.iter().map(|b| (b.close - b.open).abs()).collect();
        let first = heights[0];
        assert!(heights.iter().any(|h| (h - first).abs() > 1e-9), "ATR box changes over time");
    }

    #[test]
    fn renko_percentage_mode_scales_with_price() {
        let src = vec![
            c(1, 100.0, 100.0, 100.0, 100.0),
            c(2, 103.0, 103.0, 103.0, 103.0),
        ];
        let cfg = RenkoConfig { mode: RenkoMode::Percentage, percentage: 2.0, ..Default::default() };
        let bricks = renko(&src, &cfg);
        assert_eq!(bricks.len(), 1);
        assert!((bricks[0].open - 100.0).abs() < 1e-9);
        assert!((bricks[0].close - 102.0).abs() < 1e-9);
    }

    #[test]
    fn renko_wicks_extend_to_the_extremes() {
        let src = vec![
            c(1, 100.0, 100.0, 100.0, 100.0),
            c(2, 100.0, 108.0, 100.0, 105.0),
        ];
        let no = RenkoConfig { box_size: 5.0, wicks: false, ..Default::default() };
        let yes = RenkoConfig { box_size: 5.0, wicks: true, ..Default::default() };
        let b0 = renko(&src, &no);
        let b1 = renko(&src, &yes);
        assert!((b0[0].high - 105.0).abs() < 1e-9);
        assert!((b1[0].high - 108.0).abs() < 1e-9, "wick reaches the high");
    }

    #[test]
    fn renko_timestamps_are_strictly_increasing() {
        let mut src = Vec::new();
        for i in 0..20 {
            let p = 100.0 + (i as f64) * 6.0;
            src.push(c(1000 + i as i64, p, p, p, p));
        }
        let cfg = RenkoConfig { box_size: 5.0, ..Default::default() };
        let bricks = renko(&src, &cfg);
        for w in bricks.windows(2) {
            assert!(w[1].time > w[0].time, "strictly increasing times");
        }
    }
}

pub mod engines;
pub mod chart_type;
pub mod indicators;
pub mod math;
pub mod model;
pub mod oi_trend;
pub mod option;
pub mod patterns;

pub use model::*;

/// Compute one indicator by id for the given candles and settings.
pub fn compute(id: &str, candles: &[Candle], settings: &Settings) -> Vec<SeriesOut> {
    match indicators::find(id) {
        Some(entry) => {
            let mut outs = (entry.compute)(candles, settings);
            // Straight-line indicators share one colour rule everywhere: the line
            // is green while it rises (bullish) and red while it falls (bearish).
            indicators::color_straight_line(id, &mut outs, settings);
            outs
        }
        None => Vec::new(),
    }
}

/// Realtime helper: recompute only the last point of every series where possible.
/// For correctness this simply recomputes the whole series (the web engine only
/// calls it on ticks); callers can diff if they need speed.
pub fn recompute(id: &str, candles: &[Candle], settings: &Settings) -> Vec<SeriesOut> {
    compute(id, candles, settings)
}

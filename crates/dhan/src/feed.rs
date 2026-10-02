//! Live Market Feed (WebSocket) client and binary packet parser.
//!
//! Protocol reference: DhanHQ "Live Market Feed".
//! Requests are JSON, responses are little-endian binary packets with an 8-byte
//! header `[code:u8][len:i16][segment:u8][securityId:i32]`.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::error::{DhanError, Result};
use crate::models::ExchangeSegment;

// Trailing slash is REQUIRED: without it `http::Uri` yields an empty path and
// the websocket request line becomes `GET ?version=...` (no leading `/`), which
// Dhan rejects with a bare HTTP 400. With the slash it is `GET /?version=...`.
pub const FEED_HOST: &str = "wss://api-feed.dhan.co/";

/// Dhan resets the socket with "Connection reset without closing handshake" when
/// the full ~2.6k-instrument set is blasted as dozens of subscribe frames the
/// instant the handshake completes. Give the feed a beat to settle, then space
/// the subscribe frames out instead of firing them all back-to-back.
const SUBSCRIBE_SETTLE: Duration = Duration::from_millis(300);
const SUBSCRIBE_PACE: Duration = Duration::from_millis(150);

/// Turn a tungstenite handshake failure into a readable error. Dhan answers a
/// rejected feed with HTTP 400 plus a short body explaining why (bad token,
/// missing Data-API entitlement, connection-slot exhaustion), which is useless
/// to the operator if we only surface the status line.
fn feed_error(e: tokio_tungstenite::tungstenite::Error) -> DhanError {
    if let tokio_tungstenite::tungstenite::Error::Http(resp) = &e {
        let status = resp.status();
        let body = resp
            .body()
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        // Response headers reveal WHO rejected the handshake: Cloudflare/Dhan
        // edge (cf-ray, server) vs the app layer, and any Retry-After hint.
        let mut hdrs = Vec::new();
        for name in ["server", "cf-ray", "retry-after", "content-type", "via", "x-cache"] {
            if let Some(v) = resp.headers().get(name).and_then(|v| v.to_str().ok()) {
                hdrs.push(format!("{name}={v}"));
            }
        }
        return DhanError::WebSocket(format!(
            "HTTP {status} body={body} headers=[{}]",
            hdrs.join(", ")
        ));
    }
    DhanError::WebSocket(e.to_string())
}

/// Which packet type to subscribe to (Annexure - Feed Request Code).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedMode {
    Ticker,
    Quote,
    Full,
    FullDepth,
}

impl Default for FeedMode {
    fn default() -> Self {
        FeedMode::Ticker
    }
}

impl FeedMode {
    pub fn request_code(&self) -> u32 {
        match self {
            FeedMode::Ticker => 15,
            FeedMode::Quote => 17,
            FeedMode::Full => 21,
            FeedMode::FullDepth => 23,
        }
    }

    /// Matching unsubscribe request code (Annexure - Feed Request Code).
    pub fn unsubscribe_code(&self) -> u32 {
        match self {
            FeedMode::Ticker => 16,
            FeedMode::Quote => 18,
            FeedMode::Full => 22,
            FeedMode::FullDepth => 24,
        }
    }
}

/// A live connection can be told to add/remove instruments without dropping the
/// socket (Dhan allows up to 5 concurrent feeds; churning them trips error 805).
#[derive(Debug, Clone)]
pub enum FeedCommand {
    Subscribe(Vec<FeedSubscription>),
    Unsubscribe(Vec<FeedSubscription>),
}

#[derive(Debug, Clone, Serialize)]
pub struct FeedSubscription {
    #[serde(rename = "ExchangeSegment")]
    pub exchange_segment: ExchangeSegment,
    #[serde(rename = "SecurityId")]
    pub security_id: String,
    /// Packet mode for this instrument: watchlist symbols subscribe in Ticker
    /// mode (LTP only - Dhan delivers no index ticks in Full mode), while option
    /// strikes subscribe in Full mode (volume/OI/depth for IV + order pricing).
    /// Not serialized: the mode selects the RequestCode of the message carrying
    /// the instrument, mirroring the old app's per-instrument subscription list.
    #[serde(skip)]
    pub mode: FeedMode,
}

impl FeedSubscription {
    pub fn new(segment: ExchangeSegment, security_id: impl ToString) -> Self {
        Self {
            exchange_segment: segment,
            security_id: security_id.to_string(),
            mode: FeedMode::default(),
        }
    }

    /// Same as [`Self::new`] but with an explicit packet mode.
    pub fn with_mode(
        segment: ExchangeSegment,
        security_id: impl ToString,
        mode: FeedMode,
    ) -> Self {
        Self {
            exchange_segment: segment,
            security_id: security_id.to_string(),
            mode,
        }
    }
}

/// Build one JSON subscribe message. Dhan accepts at most 100 instruments per
/// message, so [`subscribe_messages`] chunks larger sets.
pub fn subscribe_message(mode: FeedMode, subs: &[FeedSubscription]) -> String {
    serde_json::json!({
        "RequestCode": mode.request_code(),
        "InstrumentCount": subs.len(),
        "InstrumentList": subs,
    })
    .to_string()
}

/// Chunk subscriptions into <=100-instrument subscribe messages.
pub fn subscribe_messages(mode: FeedMode, subs: &[FeedSubscription]) -> Vec<String> {
    subs.chunks(100)
        .map(|c| subscribe_message(mode, c))
        .collect()
}

/// Build one JSON unsubscribe message (same shape, matching request code).
pub fn unsubscribe_message(mode: FeedMode, subs: &[FeedSubscription]) -> String {
    serde_json::json!({
        "RequestCode": mode.unsubscribe_code(),
        "InstrumentCount": subs.len(),
        "InstrumentList": subs,
    })
    .to_string()
}

/// Chunk unsubscriptions into <=100-instrument messages.
pub fn unsubscribe_messages(mode: FeedMode, subs: &[FeedSubscription]) -> Vec<String> {
    subs.chunks(100)
        .map(|c| unsubscribe_message(mode, c))
        .collect()
}

/// Every mode, in a stable order, so grouped messages are deterministic.
const FEED_MODES: [FeedMode; 4] = [
    FeedMode::Ticker,
    FeedMode::Quote,
    FeedMode::Full,
    FeedMode::FullDepth,
];

/// Build subscribe messages for a mixed-mode instrument list, one RequestCode
/// group per mode. Dhan accepts different RequestCodes on the same socket, which
/// is how the old app kept the watchlist on Ticker and the option strikes on
/// Full.
pub fn subscribe_grouped(subs: &[FeedSubscription]) -> Vec<String> {
    let mut out = Vec::new();
    for mode in FEED_MODES {
        let group: Vec<FeedSubscription> =
            subs.iter().filter(|s| s.mode == mode).cloned().collect();
        if !group.is_empty() {
            out.extend(subscribe_messages(mode, &group));
        }
    }
    out
}

/// Mixed-mode counterpart of [`unsubscribe_messages`].
pub fn unsubscribe_grouped(subs: &[FeedSubscription]) -> Vec<String> {
    let mut out = Vec::new();
    for mode in FEED_MODES {
        let group: Vec<FeedSubscription> =
            subs.iter().filter(|s| s.mode == mode).cloned().collect();
        if !group.is_empty() {
            out.extend(unsubscribe_messages(mode, &group));
        }
    }
    out
}

/// The disconnect request (RequestCode 12).
pub fn disconnect_message() -> String {
    serde_json::json!({ "RequestCode": 12 }).to_string()
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DepthLevel {
    pub bid_qty: i32,
    pub ask_qty: i32,
    pub bid_orders: i16,
    pub ask_orders: i16,
    pub bid_price: f32,
    pub ask_price: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FeedPacket {
    /// Response code 1 (index) / 2 (ticker): price + time only.
    Ticker {
        segment: u8,
        security_id: i32,
        ltp: f32,
        ltt: i64,
    },
    Quote {
        segment: u8,
        security_id: i32,
        ltp: f32,
        last_qty: i16,
        ltt: i64,
        atp: f32,
        volume: i32,
        total_sell_qty: i32,
        total_buy_qty: i32,
        day_open: f32,
        day_close: f32,
        day_high: f32,
        day_low: f32,
    },
    Full {
        segment: u8,
        security_id: i32,
        ltp: f32,
        last_qty: i16,
        ltt: i64,
        atp: f32,
        volume: i32,
        total_sell_qty: i32,
        total_buy_qty: i32,
        oi: i32,
        oi_day_high: i32,
        oi_day_low: i32,
        day_open: f32,
        day_close: f32,
        day_high: f32,
        day_low: f32,
        depth: Vec<DepthLevel>,
    },
    Oi {
        segment: u8,
        security_id: i32,
        oi: i32,
    },
    PrevClose {
        segment: u8,
        security_id: i32,
        prev_close: f32,
        prev_oi: i32,
    },
    MarketStatus {
        segment: u8,
        security_id: i32,
    },
    Disconnect {
        code: i16,
    },
    /// Connection-state change from the supervisor: `up=true` right after a
    /// successful handshake + subscribe, `up=false` when a socket drops before
    /// the next reconnect attempt. Lets the broker watchdog tell "connected but
    /// silent" apart from "reconnecting in backoff" so it never resets a growing
    /// backoff (which used to hammer Dhan while a client id was rate-limited).
    Link {
        up: bool,
    },
    /// A websocket Ping from Dhan (sent every ~10s). It carries no market data
    /// but proves the socket is alive even when the market is closed and no ticks
    /// flow, so the broker stays quiet instead of tearing the connection down.
    Heartbeat,
    Unknown {
        code: u8,
        len: usize,
    },
}

fn f32_at(b: &[u8], off: usize) -> Option<f32> {
    b.get(off..off + 4)
        .map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn i32_at(b: &[u8], off: usize) -> Option<i32> {
    b.get(off..off + 4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
fn i16_at(b: &[u8], off: usize) -> Option<i16> {
    b.get(off..off + 2)
        .map(|s| i16::from_le_bytes([s[0], s[1]]))
}

fn parse_depth(b: &[u8], start: usize) -> Vec<DepthLevel> {
    let mut out = Vec::with_capacity(5);
    for i in 0..5 {
        let off = start + i * 20;
        let (bid_qty, ask_qty, bid_orders, ask_orders, bid_price, ask_price) = (
            i32_at(b, off),
            i32_at(b, off + 4),
            i16_at(b, off + 8),
            i16_at(b, off + 10),
            f32_at(b, off + 12),
            f32_at(b, off + 16),
        );
        if let (Some(bq), Some(aq), Some(bo), Some(ao), Some(bp), Some(ap)) =
            (bid_qty, ask_qty, bid_orders, ask_orders, bid_price, ask_price)
        {
            out.push(DepthLevel {
                bid_qty: bq,
                ask_qty: aq,
                bid_orders: bo,
                ask_orders: ao,
                bid_price: bp,
                ask_price: ap,
            });
        }
    }
    out
}

/// Human-readable meaning for a feed disconnect reason code (Data API errors).
pub fn disconnect_reason(code: i16) -> &'static str {
    match code {
        800 => "internal server error",
        804 => "requested number of instruments exceeds limit",
        805 => "too many requests or connections",
        806 => "Data APIs not subscribed",
        807 => "access token is expired",
        808 => "authentication failed - client id or access token invalid",
        809 => "access token is invalid",
        810 => "client id is invalid",
        811 => "invalid expiry date",
        812 => "invalid date format",
        813 => "invalid security id",
        814 => "invalid request",
        _ => "unknown",
    }
}

/// Parse a single binary feed packet. Returns `None` for a truncated header.
pub fn parse_packet(data: &[u8]) -> Option<FeedPacket> {
    if data.len() < 8 {
        return None;
    }
    let code = data[0];
    let len = i16::from_le_bytes([data[1], data[2]]) as usize;
    let segment = data[3];
    let security_id = i32_at(data, 4)?;

    let pkt = match code {
        1 | 2 => FeedPacket::Ticker {
            segment,
            security_id,
            ltp: f32_at(data, 8).unwrap_or(0.0),
            ltt: i32_at(data, 12).map(|v| v as i64).unwrap_or(0),
        },
        4 => FeedPacket::Quote {
            segment,
            security_id,
            ltp: f32_at(data, 8).unwrap_or(0.0),
            last_qty: i16_at(data, 12).unwrap_or(0),
            ltt: i32_at(data, 14).map(|v| v as i64).unwrap_or(0),
            atp: f32_at(data, 18).unwrap_or(0.0),
            volume: i32_at(data, 22).unwrap_or(0),
            total_sell_qty: i32_at(data, 26).unwrap_or(0),
            total_buy_qty: i32_at(data, 30).unwrap_or(0),
            day_open: f32_at(data, 34).unwrap_or(0.0),
            day_close: f32_at(data, 38).unwrap_or(0.0),
            day_high: f32_at(data, 42).unwrap_or(0.0),
            day_low: f32_at(data, 46).unwrap_or(0.0),
        },
        5 => FeedPacket::Oi {
            segment,
            security_id,
            oi: i32_at(data, 8).unwrap_or(0),
        },
        6 => FeedPacket::PrevClose {
            segment,
            security_id,
            prev_close: f32_at(data, 8).unwrap_or(0.0),
            prev_oi: i32_at(data, 12).unwrap_or(0),
        },
        7 => FeedPacket::MarketStatus {
            segment,
            security_id,
        },
        8 => FeedPacket::Full {
            segment,
            security_id,
            ltp: f32_at(data, 8).unwrap_or(0.0),
            last_qty: i16_at(data, 12).unwrap_or(0),
            ltt: i32_at(data, 14).map(|v| v as i64).unwrap_or(0),
            atp: f32_at(data, 18).unwrap_or(0.0),
            volume: i32_at(data, 22).unwrap_or(0),
            total_sell_qty: i32_at(data, 26).unwrap_or(0),
            total_buy_qty: i32_at(data, 30).unwrap_or(0),
            oi: i32_at(data, 34).unwrap_or(0),
            oi_day_high: i32_at(data, 38).unwrap_or(0),
            oi_day_low: i32_at(data, 42).unwrap_or(0),
            day_open: f32_at(data, 46).unwrap_or(0.0),
            day_close: f32_at(data, 50).unwrap_or(0.0),
            day_high: f32_at(data, 54).unwrap_or(0.0),
            day_low: f32_at(data, 58).unwrap_or(0.0),
            depth: parse_depth(data, 62),
        },
        50 => FeedPacket::Disconnect {
            code: i16_at(data, 8).unwrap_or(0),
        },
        _ => FeedPacket::Unknown { code, len },
    };
    Some(pkt)
}

/// A live feed connection with automatic reconnect.
pub struct MarketFeed {
    client_id: String,
    access_token: String,
    url: String,
    /// Instruments and their per-instrument packet mode. The mode rides on each
    /// [`FeedSubscription`] so one socket can carry the watchlist on Ticker and
    /// the option strikes on Full (old app behaviour).
    subscriptions: Vec<FeedSubscription>,
}

impl MarketFeed {
    pub fn new(
        client_id: impl Into<String>,
        access_token: impl Into<String>,
        subscriptions: Vec<FeedSubscription>,
    ) -> Self {
        let client_id = client_id.into();
        let access_token = access_token.into();
        let url = format!(
            "{}?version=2&token={}&clientId={}&authType=2",
            FEED_HOST, access_token, client_id
        );
        Self {
            client_id,
            access_token,
            url,
            subscriptions,
        }
    }

    pub fn subscriptions(&self) -> &[FeedSubscription] {
        &self.subscriptions
    }

    /// Connect, subscribe, and forward parsed packets until `tx` is dropped.
    ///
    /// `cmd_rx` carries live subscribe/unsubscribe requests so instruments can be
    /// added without reopening the socket. Reconnects with capped exponential
    /// backoff and replays the full subscription set after a drop.
    pub async fn run(
        self,
        tx: mpsc::Sender<FeedPacket>,
        mut cmd_rx: mpsc::UnboundedReceiver<FeedCommand>,
    ) {
        let mut subs = self.subscriptions.clone();
        // A duplicated (security_id, segment) makes Dhan reject/reset the whole
        // subscribe batch, so collapse the set once before it is ever sent.
        {
            let mut seen = std::collections::HashSet::new();
            subs.retain(|s| {
                seen.insert(format!("{:?}:{}", s.exchange_segment, s.security_id))
            });
        }
        // Log shape (never the value) so we can tell a well-formed JWT from a
        // truncated/garbled paste, and spot an over-long handshake URL.
        let special: std::collections::BTreeSet<char> = self
            .access_token
            .chars()
            .filter(|c| !c.is_ascii_alphanumeric() && !matches!(c, '.' | '_' | '-' | '~'))
            .collect();
        tracing_feed(&format!(
            "feed creds: url_len={} token_len={} client_id_len={} token_dots={} url_special_chars={:?}",
            self.url.len(),
            self.access_token.len(),
            self.client_id.len(),
            self.access_token.matches('.').count(),
            special
        ));
        // Dhan keeps a rejected connection's slot for a while; a handshake that
        // comes back 400 usually means the account is at its concurrent-feed
        // limit, so back off generously (up to 90s) to give slots time to free.
        let mut backoff = 2u64;
        loop {
            // A closed command channel means the owner stopped the feed: exit
            // even if the last connect attempt failed, otherwise a feed stuck on
            // handshake errors would keep its Dhan connection slot forever.
            if tx.is_closed() || cmd_rx.is_closed() {
                return;
            }
            match self.run_once(&tx, &mut subs, &mut cmd_rx, &mut backoff).await {
                Ok(()) => return, // tx closed / shutdown requested
                Err(e) => {
                    tracing_feed(&format!("feed error: {e}; reconnecting in {backoff}s"));
                    // Tell the broker the socket is down so its watchdog treats
                    // this as "reconnecting in backoff" and leaves it alone.
                    let _ = tx.send(FeedPacket::Link { up: false }).await;
                    // Sleep in short slices so a stop request cancels the wait
                    // promptly instead of after a full 90s backoff.
                    let mut left = backoff;
                    while left > 0 {
                        if cmd_rx.is_closed() {
                            tracing_feed("feed shutdown requested; stopping");
                            return;
                        }
                        let slice = left.min(2);
                        tokio::time::sleep(Duration::from_secs(slice)).await;
                        left -= slice;
                    }
                    backoff = (backoff * 2).min(90);
                }
            }
        }
    }

    fn same(a: &FeedSubscription, b: &FeedSubscription) -> bool {
        a.security_id == b.security_id && a.exchange_segment == b.exchange_segment
    }

    async fn run_once(
        &self,
        tx: &mpsc::Sender<FeedPacket>,
        subs: &mut Vec<FeedSubscription>,
        cmd_rx: &mut mpsc::UnboundedReceiver<FeedCommand>,
        backoff: &mut u64,
    ) -> Result<()> {
        let mut req = self
            .url
            .as_str()
            .into_client_request()
            .map_err(feed_error)?;
        req.headers_mut()
            .insert("User-Agent", HeaderValue::from_static("algo-rs/0.1"));
        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(feed_error)?;
        // Backoff is deliberately NOT reset here: Dhan can accept the handshake
        // and then immediately reset the socket (client-id block or connection
        // slot limit). Resetting on every handshake made a blocked feed reconnect
        // every 2s forever, which kept hammering Dhan so the block never cleared.
        // It is reset below, once real market data actually arrives.
        let (mut write, mut read) = ws.split();
        let mut got_data = false;

        // Let the freshly-upgraded socket settle before the first subscribe, then
        // send the <=100-instrument batches with a gap: Dhan resets a connection
        // that is hit with the whole set in one burst.
        tokio::time::sleep(SUBSCRIBE_SETTLE).await;
        let mut first = true;
        for msg in subscribe_grouped(subs) {
            if !first {
                tokio::time::sleep(SUBSCRIBE_PACE).await;
            }
            first = false;
            write
                .send(Message::Text(msg))
                .await
                .map_err(|e| DhanError::WebSocket(e.to_string()))?;
        }
        tracing_feed(&format!("feed connected; {} instruments subscribed", subs.len()));
        let _ = tx.send(FeedPacket::Link { up: true }).await;

        // Proactive keepalive: Dhan pings us, but we also ping it so liveness
        // does not depend on the exchange's cadence. A quiet holiday market then
        // stays demonstrably alive instead of being torn down as "silent".
        let mut keepalive = tokio::time::interval(Duration::from_secs(5));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        keepalive.tick().await; // consume the immediate first tick

        loop {
            tokio::select! {
                item = read.next() => {
                    let Some(item) = item else {
                        return Err(DhanError::WebSocket("feed stream ended".into()));
                    };
                    let msg = item.map_err(|e| DhanError::WebSocket(e.to_string()))?;
                    match msg {
                        Message::Binary(bytes) => {
                            if let Some(pkt) = parse_packet(&bytes) {
                                let stop = matches!(pkt, FeedPacket::Disconnect { .. });
                                if let FeedPacket::Disconnect { code } = &pkt {
                                    tracing_feed(&format!(
                                        "feed disconnect code {code}: {}",
                                        disconnect_reason(*code)
                                    ));
                                }
                                // Real data arrived, so this connection is
                                // genuinely working: safe to reset the backoff.
                                if !stop && !got_data {
                                    *backoff = 2;
                                    got_data = true;
                                }
                                if tx.send(pkt).await.is_err() {
                                    let _ = write.send(Message::Text(disconnect_message())).await;
                                    return Ok(());
                                }
                                if stop {
                                    return Err(DhanError::WebSocket("feed disconnected".into()));
                                }
                            }
                        }
                        Message::Ping(payload) => {
                            let _ = write.send(Message::Pong(payload)).await;
                            // Surface liveness so a quiet-but-open socket (closed
                            // market) is not mistaken for a stall.
                            let _ = tx.send(FeedPacket::Heartbeat).await;
                        }
                        Message::Pong(_) => {
                            // Reply to our own keepalive ping: the socket is alive.
                            let _ = tx.send(FeedPacket::Heartbeat).await;
                        }
                        Message::Text(text) => {
                            // The feed only sends binary data packets; a text frame is a
                            // status/error notice (e.g. an expired token or a rejected
                            // subscription). Surface it instead of silently ignoring it,
                            // and reconnect when it reads like a rejection.
                            let lower = text.to_ascii_lowercase();
                            let reject = ["error", "unauthor", "invalid", "forbidden", "expired", "token", "disconnect"]
                                .iter()
                                .any(|m| lower.contains(m));
                            tracing::warn!("dhan feed text frame (len={}): {}", text.len(), text);
                            if reject {
                                return Err(DhanError::WebSocket("feed reported an error".into()));
                            }
                        }
                        Message::Close(_) => {
                            return Err(DhanError::WebSocket("feed closed".into()));
                        }
                        _ => {}
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(FeedCommand::Subscribe(new)) => {
                            let mut added = Vec::new();
                            for s in new {
                                match subs.iter_mut().find(|e| Self::same(e, &s)) {
                                    // Already subscribed in the same mode: nothing
                                    // to do. A different mode is an upgrade
                                    // (e.g. Ticker -> Full for a promoted strike),
                                    // so re-send it with the new RequestCode.
                                    Some(e) if e.mode == s.mode => {}
                                    Some(e) => {
                                        e.mode = s.mode;
                                        added.push(s);
                                    }
                                    None => {
                                        subs.push(s.clone());
                                        added.push(s);
                                    }
                                }
                            }
                            if !added.is_empty() {
                                let mut first = true;
                                for msg in subscribe_grouped(&added) {
                                    if !first {
                                        tokio::time::sleep(SUBSCRIBE_PACE).await;
                                    }
                                    first = false;
                                    write.send(Message::Text(msg)).await
                                        .map_err(|e| DhanError::WebSocket(e.to_string()))?;
                                }
                                tracing_feed(&format!("feed subscribed {} more", added.len()));
                            }
                        }
                        Some(FeedCommand::Unsubscribe(old)) => {
                            subs.retain(|e| !old.iter().any(|o| Self::same(e, o)));
                            let mut first = true;
                            for msg in unsubscribe_grouped(&old) {
                                if !first {
                                    tokio::time::sleep(SUBSCRIBE_PACE).await;
                                }
                                first = false;
                                write.send(Message::Text(msg)).await
                                    .map_err(|e| DhanError::WebSocket(e.to_string()))?;
                            }
                        }
                        None => {
                            // The control channel was dropped: the owner is
                            // shutting this feed down. Close politely (RequestCode
                            // 12) so Dhan frees the connection slot immediately
                            // instead of leaving it to time out - leaked slots
                            // are what make the next handshake come back 400.
                            let _ = write.send(Message::Text(disconnect_message())).await;
                            let _ = write.close().await;
                            return Ok(());
                        }
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = write.send(Message::Ping(Vec::new())).await {
                        return Err(DhanError::WebSocket(format!("keepalive ping failed: {e}")));
                    }
                }
            }
        }
    }
}

/// Log a feed lifecycle message, redacting any `token=` query value so the
/// access token can never leak into logs.
fn tracing_feed(msg: &str) {
    let redacted = match msg.find("token=") {
        Some(i) => {
            let mut s = msg[..i].to_string();
            s.push_str("token=<redacted>");
            if let Some(j) = msg[i..].find('&') {
                s.push_str(&msg[i + j..]);
            }
            s
        }
        None => msg.to_string(),
    };
    tracing::warn!("{}", redacted);
}

impl std::fmt::Debug for MarketFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _ = (&self.client_id, &self.access_token);
        f.debug_struct("MarketFeed")
            .field("subscriptions", &self.subscriptions.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(code: u8, payload_len: usize, segment: u8, security_id: i32) -> Vec<u8> {
        let mut b = Vec::new();
        b.push(code);
        b.extend_from_slice(&((payload_len + 8) as i16).to_le_bytes());
        b.push(segment);
        b.extend_from_slice(&security_id.to_le_bytes());
        b
    }

    #[test]
    fn parses_ticker_packet() {
        let mut p = header(2, 8, 1, 11536);
        p.extend_from_slice(&4525.55f32.to_le_bytes());
        p.extend_from_slice(&1_700_000_000i32.to_le_bytes());
        match parse_packet(&p).unwrap() {
            FeedPacket::Ticker { segment, security_id, ltp, ltt } => {
                assert_eq!(segment, 1);
                assert_eq!(security_id, 11536);
                assert!((ltp - 4525.55).abs() < 0.01);
                assert_eq!(ltt, 1_700_000_000);
            }
            other => panic!("expected ticker, got {other:?}"),
        }
    }

    #[test]
    fn parses_quote_packet() {
        let mut p = header(4, 42, 1, 1333);
        p.extend_from_slice(&4520.0f32.to_le_bytes()); // ltp  @8
        p.extend_from_slice(&5i16.to_le_bytes()); // ltq        @12
        p.extend_from_slice(&111i32.to_le_bytes()); // ltt      @14
        p.extend_from_slice(&4515.0f32.to_le_bytes()); // atp    @18
        p.extend_from_slice(&123456i32.to_le_bytes()); // volume @22
        p.extend_from_slice(&10i32.to_le_bytes()); // sell       @26
        p.extend_from_slice(&20i32.to_le_bytes()); // buy          @30
        p.extend_from_slice(&4500.0f32.to_le_bytes()); // open     @34
        p.extend_from_slice(&4490.0f32.to_le_bytes()); // close    @38
        p.extend_from_slice(&4530.0f32.to_le_bytes()); // high     @42
        p.extend_from_slice(&4480.0f32.to_le_bytes()); // low      @46
        match parse_packet(&p).unwrap() {
            FeedPacket::Quote { ltp, last_qty, volume, day_high, day_low, .. } => {
                assert!((ltp - 4520.0).abs() < 0.01);
                assert_eq!(last_qty, 5);
                assert_eq!(volume, 123456);
                assert!((day_high - 4530.0).abs() < 0.01);
                assert!((day_low - 4480.0).abs() < 0.01);
            }
            other => panic!("expected quote, got {other:?}"),
        }
    }

    #[test]
    fn parses_full_packet_depth() {
        let mut p = header(8, 154, 2, 49081);
        p.extend_from_slice(&100.0f32.to_le_bytes()); // ltp @8
        p.extend_from_slice(&2i16.to_le_bytes()); // ltq    @12
        p.extend_from_slice(&1i32.to_le_bytes()); // ltt     @14
        p.extend_from_slice(&99.0f32.to_le_bytes()); // atp    @18
        p.extend_from_slice(&7i32.to_le_bytes()); // volume    @22
        p.extend_from_slice(&0i32.to_le_bytes()); // sell      @26
        p.extend_from_slice(&0i32.to_le_bytes()); // buy       @30
        p.extend_from_slice(&500i32.to_le_bytes()); // oi      @34
        p.extend_from_slice(&600i32.to_le_bytes()); // oi hi   @38
        p.extend_from_slice(&400i32.to_le_bytes()); // oi lo   @42
        p.extend_from_slice(&98.0f32.to_le_bytes()); // open    @46
        p.extend_from_slice(&97.0f32.to_le_bytes()); // close   @50
        p.extend_from_slice(&101.0f32.to_le_bytes()); // high   @54
        p.extend_from_slice(&96.0f32.to_le_bytes()); // low     @58
        for i in 0..5i32 {
            p.extend_from_slice(&(i + 1).to_le_bytes()); // bid qty
            p.extend_from_slice(&(i + 2).to_le_bytes()); // ask qty
            p.extend_from_slice(&(1i16).to_le_bytes()); // bid orders
            p.extend_from_slice(&(1i16).to_le_bytes()); // ask orders
            p.extend_from_slice(&(90.0 + i as f32).to_le_bytes()); // bid price
            p.extend_from_slice(&(91.0 + i as f32).to_le_bytes()); // ask price
        }
        match parse_packet(&p).unwrap() {
            FeedPacket::Full { oi, depth, day_high, .. } => {
                assert_eq!(oi, 500);
                assert_eq!(depth.len(), 5);
                assert_eq!(depth[0].bid_qty, 1);
                assert_eq!(depth[4].ask_qty, 6);
                assert!((day_high - 101.0).abs() < 0.01);
            }
            other => panic!("expected full, got {other:?}"),
        }
    }

    #[test]
    fn parses_disconnect_and_truncated() {
        let mut p = header(50, 2, 0, 0);
        p.extend_from_slice(&805i16.to_le_bytes());
        assert_eq!(parse_packet(&p), Some(FeedPacket::Disconnect { code: 805 }));
        assert_eq!(parse_packet(&[1, 2, 3]), None);
    }

    #[test]
    fn subscribe_messages_chunk_at_100() {
        let subs: Vec<_> = (1..=205)
            .map(|i| FeedSubscription::new(ExchangeSegment::NseEq, i))
            .collect();
        let msgs = subscribe_messages(FeedMode::Quote, &subs);
        assert_eq!(msgs.len(), 3);
        let first: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(first["RequestCode"], 17);
        assert_eq!(first["InstrumentCount"], 100);
        assert_eq!(first["InstrumentList"].as_array().unwrap().len(), 100);
    }

    #[test]
    fn grouped_subscribe_splits_by_mode() {
        let subs = vec![
            FeedSubscription::with_mode(ExchangeSegment::IdxI, 13, FeedMode::Ticker),
            FeedSubscription::with_mode(ExchangeSegment::NseEq, 11536, FeedMode::Ticker),
            FeedSubscription::with_mode(ExchangeSegment::NseFno, 49081, FeedMode::Full),
        ];
        let msgs = subscribe_grouped(&subs);
        assert_eq!(msgs.len(), 2);
        let first: serde_json::Value = serde_json::from_str(&msgs[0]).unwrap();
        assert_eq!(first["RequestCode"], 15);
        assert_eq!(first["InstrumentCount"], 2);
        // The mode must never leak into the wire payload.
        assert!(first["InstrumentList"][0].get("mode").is_none());
        let second: serde_json::Value = serde_json::from_str(&msgs[1]).unwrap();
        assert_eq!(second["RequestCode"], 21);
        assert_eq!(second["InstrumentCount"], 1);
    }

    #[test]
    fn unsubscribe_uses_matching_request_code() {
        let subs = vec![FeedSubscription::new(ExchangeSegment::NseEq, 13)];
        let m: serde_json::Value =
            serde_json::from_str(&unsubscribe_message(FeedMode::Full, &subs)).unwrap();
        assert_eq!(m["RequestCode"], 22);
        assert_eq!(m["InstrumentCount"], 1);
        let msgs = unsubscribe_messages(FeedMode::Ticker, &subs);
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn disconnect_reason_covers_known_codes() {
        assert_eq!(disconnect_reason(807), "access token is expired");
        assert_eq!(disconnect_reason(805), "too many requests or connections");
        assert_eq!(disconnect_reason(-1), "unknown");
    }

    #[test]
    fn feed_url_has_a_valid_request_target() {
        // Regression: a host without a trailing slash produced `GET ?version=...`
        // (no leading `/`), which Dhan answered with a bare HTTP 400.
        let url = format!(
            "{}?version=2&token=abc&clientId=1&authType=2",
            FEED_HOST
        );
        let req = url.into_client_request().unwrap();
        let pq = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default();
        assert!(
            pq.starts_with("/?"),
            "websocket request target must start with '/?', got {pq:?}"
        );
    }
}

// Top-bar DhanHQ connection controls.
//
// Mirrors the old Python app: Client ID + Access Token + Connect, an Auto Reset
// checkbox, and a live feed diagnostic line ("tick 3s ago | 228 subscribed |
// auto-reset on"). Talks to the Rust server's /api/connect, /api/status and
// /api/feed/* routes. The access token is never persisted to localStorage.

const LS_AUTORESET = "autoResetFeed";
const LS_CLIENTID = "dhanClientId";

const AUTO_RESET_STALE_SEC = 45;
const AUTO_RESET_MIN_INTERVAL = 600000;
const FEED_RESTART_MIN_INTERVAL = 30000;

export function bootConnection() {
  const $ = (id) => document.getElementById(id);
  let autoResetLastAt = 0;
  let feedRestartLastAt = 0;
  let resetCountdown = null;
  let feedWatch = null;
  // Native confirm()/alert() are blocked inside the sandboxed preview iframe, so
  // Reset Feed uses an in-page two-tap confirmation instead.
  let resetArmed = false;
  let resetArmTimer = null;

  // NSE cash hours: 09:15-15:30 IST == 03:45-10:00 UTC, Mon-Fri.
  function isMarketOpen(nowSec) {
    const d = new Date(nowSec * 1000);
    const day = d.getUTCDay();
    if (day === 0 || day === 6) return false;
    const open = new Date(nowSec * 1000);
    open.setUTCHours(3, 45, 0, 0);
    const close = new Date(nowSec * 1000);
    close.setUTCHours(10, 0, 0, 0);
    return nowSec >= Math.floor(open.getTime() / 1000) && nowSec < Math.floor(close.getTime() / 1000);
  }
  const nowSec = () => Math.floor(Date.now() / 1000);

  function restore() {
    try {
      const cid = localStorage.getItem(LS_CLIENTID);
      if (cid) $("clientId").value = cid;
    } catch (e) {}
    const cb = $("autoResetFeed");
    try {
      cb.checked = JSON.parse(localStorage.getItem(LS_AUTORESET) || "true");
    } catch (e) {
      cb.checked = true;
    }
    cb.addEventListener("change", () => {
      try {
        localStorage.setItem(LS_AUTORESET, JSON.stringify(cb.checked));
      } catch (e) {}
    });

    // A session may already exist on the server (page reload, second tab). Adopt
    // it so the button/status reflect reality and the self-heal paths below stay
    // active without a manual Connect tap.
    fetch("/api/status")
      .then((r) => r.json())
      .then((s) => {
        if (s && s.connected) {
          const btn = $("connectBtn");
          if (btn) btn.textContent = "Connected - Reconnect";
          const rb = $("resetFeedBtn");
          if (rb) rb.style.display = "";
          setStatus("Connected - waiting for live feed...", "warn");
        }
      })
      .catch(() => {});
  }

  function setStatus(text, cls) {
    const st = $("status");
    if (!st) return;
    st.textContent = text;
    st.className = cls || "";
  }

  // Reload every data panel off the freshly authenticated session. The chart
  // and the option-chain snapshot come straight from Dhan's REST API, so they
  // load as soon as the session exists - they must not wait for the websocket.
  // Re-pushing the market-watch subscription is what registers the option
  // strikes with the live feed.
  function reloadAll() {
    const refresh = $("btnRefresh");
    if (refresh) refresh.click();
    const mw = $("mwRefresh");
    if (mw) mw.click();
    const ocPane = document.getElementById("tab-optionchain");
    if (ocPane && ocPane.classList.contains("active")) {
      const oc = $("ocRefreshBtn");
      if (oc) oc.click();
    }
    window.dispatchEvent(new CustomEvent("dhan-connected"));
  }

  // A connect is only "done" once the broker socket is actually streaming.
  // Dhan can accept the handshake and then deliver nothing (per-account slot
  // limit, silent rejection), which is why the old UI needed 2-3 Connect taps:
  // the first tap showed "Connected" but no data ever arrived. Load the REST
  // panels immediately, then watch feed health and reload again the moment the
  // first real tick lands, nudging the server to restart a silent socket.
  function watchFeedForStreaming() {
    if (feedWatch) clearInterval(feedWatch);
    const startedAt = Date.now();
    let kickSent = false;
    let reloaded = false;
    reloadAll();
    feedWatch = setInterval(() => {
      fetch("/api/feed/status")
        .then((r) => r.json())
        .then((d) => {
          if (!d || d.status !== "success") return;
          if (d.feed_up) {
            clearInterval(feedWatch);
            feedWatch = null;
            if (!reloaded) {
              reloaded = true;
              reloadAll();
            }
            setStatus("Connected - live feed streaming", "ok");
            return;
          }
          if (!kickSent && Date.now() - startedAt > 8000) {
            kickSent = true;
            fetch("/api/feed/restart", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: "{}",
            }).catch(() => {});
          }
          if (Date.now() - startedAt > 45000) {
            clearInterval(feedWatch);
            feedWatch = null;
            setStatus(
              "Connected, but the live feed has not started - tap Reset Feed if data is missing",
              "warn"
            );
          }
        })
        .catch(() => {});
    }, 1500);
  }

  function connect() {
    const cid = $("clientId").value.trim();
    const tok = $("accessToken").value.trim();
    if (!cid || !tok) {
      setStatus("Please enter Client ID and Access Token", "error");
      return;
    }
    try { localStorage.setItem(LS_CLIENTID, cid); } catch (e) {}
    const btn = $("connectBtn");
    btn.disabled = true;
    btn.textContent = "Connecting...";
    setStatus("Connecting to Dhan...", "warn");
    // Tell the link monitor not to flash its disconnect banner/popups while a
    // legitimate connect attempt is in flight.
    window.__connecting = true;
    // Make the reset control reachable as soon as a connect is attempted, even
    // if auth later fails and /api/status never reports connected.
    const rb = $("resetFeedBtn");
    if (rb) rb.style.display = "";

    // Fire-and-forget: the button state is driven by /api/status polling so a
    // slow tunnel that holds the response cannot wedge the UI. We also read the
    // immediate response so an auth error surfaces at once instead of after the
    // 60s poll window.
    let polls = 0;
    let iv = null;
    fetch("/api/connect", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ client_id: cid, access_token: tok }),
    })
      .then(async (r) => {
        let d = null;
        try { d = await r.json(); } catch (e) { d = null; }
        if (d && d.status === "error") {
          if (iv) clearInterval(iv);
          window.__connecting = false;
          btn.disabled = false;
          btn.textContent = "Connect";
          setStatus(d.message || d.auth_error || "Connect failed", "error");
        }
      })
      .catch(() => {});

    iv = setInterval(() => {
      fetch("/api/status")
        .then((r) => r.json())
        .then((s) => {
          if (s && s.connected) {
            clearInterval(iv);
            window.__connecting = false;
            btn.disabled = false;
            btn.textContent = "Connected - Reconnect";
            const rb = $("resetFeedBtn");
            if (rb) rb.style.display = "";
            setStatus("Connected - starting live feed...", "warn");
            watchFeedForStreaming();
          } else if (s && s.auth_error) {
            clearInterval(iv);
            window.__connecting = false;
            btn.disabled = false;
            btn.textContent = "Connect";
            setStatus(s.auth_error, "error");
          } else if (++polls >= 30) {
            clearInterval(iv);
            window.__connecting = false;
            btn.disabled = false;
            btn.textContent = "Connect";
            setStatus("Connect timed out - tap Connect to retry", "error");
          }
        })
        .catch(() => {});
    }, 2000);
  }

  function resetFeed(auto) {
    if (resetCountdown) return;
    const btn = $("resetFeedBtn");
    if (!auto) {
      // Two-tap confirm (native confirm() is suppressed in the preview iframe).
      if (!resetArmed) {
        resetArmed = true;
        if (btn) btn.textContent = "Confirm Reset?";
        setStatus("Tap Reset Feed again to confirm - 90s cooldown", "warn");
        clearTimeout(resetArmTimer);
        resetArmTimer = setTimeout(() => {
          resetArmed = false;
          if (btn) btn.textContent = "Reset Feed";
        }, 5000);
        return;
      }
      resetArmed = false;
      clearTimeout(resetArmTimer);
      if (btn) btn.textContent = "Reset Feed";
    }
    if (btn) btn.disabled = true;
    setStatus(auto ? "Feed stalled - auto reset triggered" : "Stopping feed...", "warn");
    fetch("/api/feed/reset", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: "{}",
    })
      .then((r) => r.json())
      .then((d) => {
        let cool = d && d.cooldown ? d.cooldown : 90;
        setStatus("Feed reset. Waiting " + cool + "s for Dhan to release connection slots...", "warn");
        resetCountdown = setInterval(() => {
          $("feedStatus").textContent = "cooldown " + --cool + "s";
          if (cool <= 0) {
            clearInterval(resetCountdown);
            resetCountdown = null;
            if (btn) btn.disabled = false;
            setStatus("Slots released. Reconnecting...", "warn");
            if ($("clientId").value.trim() && $("accessToken").value.trim()) {
              connect();
            } else {
              setStatus("Reconnect when ready (enter Client ID + Token)", "error");
            }
          }
        }, 1000);
      })
      .catch(() => {
        if (btn) btn.disabled = false;
        setStatus("Reset failed", "error");
      });
  }

  function feedTick() {
    fetch("/api/feed/status")
      .then((r) => r.json())
      .then((d) => {
        if (!d || d.status !== "success") return;
        const el = $("feedStatus");
        if (resetCountdown) return; // don't clobber the cooldown counter
        const cb = $("autoResetFeed");
        const mktOpen = isMarketOpen(nowSec());
        const parts = [];
        let stale = false;
        // Liveness, not just trade ticks: on a holiday / off-hours the market
        // sends no ticks, but Dhan + our keepalive ping keep the socket alive.
        // A fresh heartbeat must therefore never count as "stale" - treating it
        // as stale is what made Auto Reset stop the feed and show a disconnect.
        const actAge =
          d.last_activity_age_sec != null ? d.last_activity_age_sec : d.last_tick_age_sec;
        if (actAge != null) {
          stale = mktOpen && actAge > AUTO_RESET_STALE_SEC;
          const color = !mktOpen
            ? "#888"
            : actAge < 10
            ? "#00d4aa"
            : actAge < 45
            ? "#ff9800"
            : "#ef5350";
          const label =
            d.last_tick_age_sec != null && d.last_tick_age_sec < 15 ? "tick" : "link";
          parts.push('<span style="color:' + color + '">' + label + ' ' + actAge + "s ago</span>");
        } else {
          stale = mktOpen;
          parts.push('<span style="color:' + (mktOpen ? "#ef5350" : "#888") + '">no data</span>');
        }
        if (!mktOpen) parts.push('<span style="color:#888">market closed</span>');
        if (d.subscribed != null) parts.push(d.subscribed + " subscribed");
        if (d.reconnect_parked_sec > 0)
          parts.push('<span style="color:#ff9800">parked ' + d.reconnect_parked_sec + "s</span>");
        if (cb && cb.checked) parts.push('<span style="color:#00d4aa">auto-reset on</span>');
        el.innerHTML = parts.join(" | ");
        el.title = JSON.stringify(d);

        // Self-heal a dead or silent supervisor while connected and the market
        // is open. `feed_up === false` covers a socket Dhan accepted but never
        // streamed on (the case that used to need repeated Connect taps).
        const btn = $("connectBtn");
        const connected = btn && btn.textContent.indexOf("Connected") !== -1;
        // Only treat "not streaming" as broken if the socket is also silent;
        // an alive socket with fresh heartbeats must not trigger a restart loop.
        const notStreaming =
          d.feed_up === false &&
          (d.last_activity_age_sec == null || d.last_activity_age_sec > 30);
        if (
          (d.ws_running === false || notStreaming) &&
          d.reconnect_parked_sec <= 0 &&
          mktOpen &&
          connected
        ) {
          const now = Date.now();
          if (now - feedRestartLastAt >= FEED_RESTART_MIN_INTERVAL) {
            feedRestartLastAt = now;
            fetch("/api/feed/restart", {
              method: "POST",
              headers: { "Content-Type": "application/json" },
              body: "{}",
            }).catch(() => {});
          }
        }
        if (cb && cb.checked && stale && !resetCountdown && mktOpen && connected) {
          const now = Date.now();
          if (now - autoResetLastAt >= AUTO_RESET_MIN_INTERVAL) {
            autoResetLastAt = now;
            resetFeed(true);
          }
        }
      })
      .catch(() => {});
  }

  restore();
  $("connectBtn").addEventListener("click", connect);
  $("resetFeedBtn").addEventListener("click", () => resetFeed(false));
  feedTick();
  setInterval(feedTick, 3000);
}

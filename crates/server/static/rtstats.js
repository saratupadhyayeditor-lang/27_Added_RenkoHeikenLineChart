// Trade Stats tab renderer (Realtime and Paper).
//
// The complete report logic - IST period windows, statistics, strategy-wise
// breakdown, most-profitable-time-of-day and every chart series - is computed
// in Rust (`crates/server/src/stats.rs`) and served by `GET /api/rt/stats`
// (realtime engine) / `GET /api/paper/stats` (paper engine). This module only
// fetches that JSON and paints it, mirroring the old Python app's Trade Stats /
// Realtime Market Trade Stats tabs.

import { istTime, istDateTime } from "./ist.js?v=1";

let booted = false;
const UI = { range: "all", mode: "", scope: "all" };
let LAST = null;
let lastRenderSig = "";
const CHARTS = {};

function el(id) {
  return document.getElementById(id);
}
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function n(v) {
  const x = Number(v);
  return Number.isFinite(x) ? x : 0;
}
function fmt2(v) {
  return Number(v).toLocaleString("en-IN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function fmtPct(v) {
  return Number(v).toFixed(1) + "%";
}
function money(v) {
  const x = Number(v);
  if (!Number.isFinite(x)) return "--";
  return (x < 0 ? "-₹" : "₹") + Math.abs(x).toLocaleString("en-IN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function signed(v) {
  const x = Number(v) || 0;
  return (x < 0 ? "-" : "+") + money(Math.abs(x));
}
function pnlCls(v) {
  return v > 0 ? "rt-pos" : v < 0 ? "rt-neg" : "";
}
function pad2(x) {
  return String(x).padStart(2, "0");
}
function fmtClock(ms) {
  const x = n(ms);
  if (!x) return "--";
  return istTime(x);
}
function fmtDT(ms) {
  const x = n(ms);
  if (!x) return "--";
  return istDateTime(x);
}
function engineFriendly(k) {
  if (k === "papertrade") return "AI Smart (base)";
  if (k === "realtime") return "Realtime";
  return k || "?";
}

// ---------------------------------------------------------------------------
// Charts
// ---------------------------------------------------------------------------
function drawChart(id, type, labels, values, colors, opts) {
  const canvas = el(id);
  if (!canvas) return;
  const empty = el(id + "Empty");
  const showEmpty = (msg) => {
    if (empty) { empty.textContent = msg; empty.style.display = "flex"; }
    if (CHARTS[id]) { try { CHARTS[id].destroy(); } catch (e) {} CHARTS[id] = null; }
    canvas.style.display = "none";
  };
  if (!labels || !labels.length || typeof window.Chart === "undefined") {
    showEmpty(typeof window.Chart === "undefined" ? "Chart library not loaded" : "Not enough data yet");
    return;
  }
  if (empty) empty.style.display = "none";
  canvas.style.display = "";
  if (CHARTS[id]) { try { CHARTS[id].destroy(); } catch (e) {} CHARTS[id] = null; }
  const isLine = type === "line";
  CHARTS[id] = new window.Chart(canvas.getContext("2d"), {
    type: isLine ? "line" : "bar",
    data: {
      labels,
      datasets: [{
        data: values,
        borderColor: isLine ? "#00d4aa" : undefined,
        backgroundColor: isLine ? "rgba(0,212,170,0.12)" : (colors || "rgba(0,212,170,0.6)"),
        borderWidth: isLine ? 1.5 : 1,
        fill: isLine,
        tension: 0.15,
        pointRadius: 0,
      }],
    },
    options: Object.assign({
      responsive: true,
      maintainAspectRatio: false,
      plugins: { legend: { display: false } },
      scales: {
        x: { ticks: { color: "#8a8ab0", maxRotation: 0, autoSkip: true, maxTicksLimit: 12, font: { size: 9 } }, grid: { color: "rgba(60,60,110,0.25)" } },
        y: { ticks: { color: "#8a8ab0", font: { size: 9 } }, grid: { color: "rgba(60,60,110,0.25)" } },
      },
    }, opts || {}),
  });
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------
function renderChips(d) {
  const host = el("rsChips");
  if (!host) return;
  let html = '<span style="font-size:10px;color:#888;margin-right:2px">Total executed trades</span>';
  (d.chips || []).forEach((c) => {
    const col = n(c.net) >= 0 ? "#00d4aa" : "#ef5350";
    html += `<button class="rs-chip" data-range="${esc(c.key)}" title="Click to view this period in detail below">` +
      `<span style="color:#888">${esc(c.label)}</span> &nbsp;<b>${n(c.n)}</b>` +
      `<span style="color:${col}"> &nbsp;${n(c.net) >= 0 ? "+" : "-"}${money(Math.abs(n(c.net)))}</span></button>`;
  });
  host.innerHTML = html;
  host.querySelectorAll(".rs-chip").forEach((b) => {
    b.onclick = () => {
      const r = el("rsRange");
      if (r) r.value = b.getAttribute("data-range");
      UI.range = b.getAttribute("data-range");
      refresh();
    };
  });
}

function card(label, value, color, title) {
  return `<div style="flex:1;min-width:120px;border:1px solid #1e1e40;border-radius:4px;padding:6px 9px;background:#101024" title="${esc(title || label)}">` +
    `<div style="font-size:9px;color:#888;text-transform:uppercase;letter-spacing:.4px">${esc(label)}</div>` +
    `<div style="font-size:15px;font-weight:700;color:${color || "#fff"}">${value}</div></div>`;
}

function renderStats(d) {
  const host = el("rsStats");
  if (!host) return;
  const s = d.stats || {};
  if (!n(s.n)) {
    host.innerHTML = `<div style="font-size:10px;color:#666;padding:6px 2px">No executed trades in <b>${esc(d.rangeLabel)}</b>.</div>`;
    return;
  }
  const netCol = n(s.net) >= 0 ? "#00d4aa" : "#ef5350";
  const wr = n(s.n) ? (n(s.wins) / n(s.n)) * 100 : 0;
  const pf = s.profitFactorInfinite ? "∞" : (s.profitFactor == null ? "--" : fmt2(s.profitFactor));
  const ddPct = n(s.maxDDPct) > 0 ? ` (${fmtPct(s.maxDDPct)})` : "";
  host.innerHTML =
    card("Trades (W/L)", `${n(s.n)} <span style="font-size:10px;color:#666">(${n(s.wins)}W / ${n(s.losses)}L)</span>`) +
    card("Win rate", fmtPct(wr), wr >= 50 ? "#00d4aa" : wr > 0 ? "#ffd700" : "#ef5350") +
    card("Net P&L", signed(s.net), netCol) +
    card("Profit factor", pf, s.profitFactorInfinite || (s.profitFactor != null && s.profitFactor >= 1) ? "#00d4aa" : "#ef5350") +
    card("Avg trade", signed(s.avg), netCol) +
    card("Avg win / loss", `${s.avgWin != null ? "+" + money(s.avgWin) : "--"} / ${s.avgLoss != null ? "-" + money(s.avgLoss) : "--"}`) +
    card("Best / Worst", `${s.best != null ? "+" + money(s.best) : "--"} / ${s.worst != null ? money(s.worst) : "--"}`) +
    card("Charges", "-" + money(s.charges), "#ff9800") +
    card("Max drawdown", "-" + money(s.maxDD) + ddPct, "#ef5350") +
    card("Best streak", `${n(s.maxStreak)}W / ${n(s.maxLoseStreak)}L`);
}

function renderTable(d) {
  const body = el("rsBody");
  if (!body) return;
  const rows = d.trades || [];
  const status = el("rsStatus");
  if (status) status.textContent = d.statusText || "";
  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="7" style="color:#666;font-size:10px;padding:8px">No executed trades in <b>${esc(d.rangeLabel)}</b> yet.</td></tr>`;
    return;
  }
  let html = rows.map((t) => {
    const net = n(t.net);
    const qtyTxt = t.lotSize && t.qty
      ? `${n(t.qty)} <span style="font-size:8px;color:#666">(${t.lots || Math.round(n(t.qty) / n(t.lotSize))}×${n(t.lotSize)})</span>`
      : n(t.qty);
    const sideCol = t.side === "BUY" ? "#00d4aa" : "#ef5350";
    const et = fmtClock(t.entryAt);
    const ct = fmtClock(t.at);
    return `<tr>
      <td><span style="color:#fff">${esc(t.symbol)}</span><br><span style="font-size:8px;color:#666">${esc(engineFriendly(t.engine))}${t.strategy ? " · " + esc(t.strategy) : ""} · <span style="color:${sideCol}">${t.side === "BUY" ? "LONG" : "SHORT"}</span></span></td>
      <td>${qtyTxt}</td>
      <td>${fmt2(t.entry)} &rarr; ${fmt2(t.exit)}</td>
      <td class="${pnlCls(net)}">${signed(net)}</td>
      <td style="color:#888">${n(t.charges) ? money(t.charges) : "--"}</td>
      <td style="color:#ff9800">${esc(t.reason)}</td>
      <td style="color:#888">${et !== "--" ? et + " → " : "-- → "}${ct}</td>
    </tr>`;
  }).join("");
  const more = n(d.tradesTotal) - n(d.tradesShown);
  if (more > 0) html += `<tr><td colspan="7" style="color:#666;font-size:9px;padding:6px">+ ${more} older trade${more === 1 ? "" : "s"} not shown (newest ${n(d.tradesShown)} listed)</td></tr>`;
  body.innerHTML = html;
}

function renderStrategy(d) {
  const body = el("rsStrategyBody");
  const note = el("rsStrategyNote");
  if (!body) return;
  const rows = d.strategy || [];
  if (!rows.length) {
    body.innerHTML = `<tr><td colspan="8" style="color:#666;font-size:10px;padding:8px">No executed trades in the selected period to break down.</td></tr>`;
    if (note) note.textContent = "";
    return;
  }
  body.innerHTML = rows.map((b) => {
    const net = n(b.net);
    const contrib = n(b.contrib);
    return `<tr${b.isFilter ? ' style="background:#0f2f28"' : ""}>
      <td>${b.isFilter ? '<span style="color:#00d4aa">&#9830;</span> ' : ""}<b style="color:#d0d0d0">${esc(b.name)}</b>${b.isFilter ? '<br><span style="font-size:8px;color:#666">indicator-filter run mode</span>' : ""}</td>
      <td>${n(b.count)} <span style="font-size:10px;color:#666">(${n(b.wins)}W / ${n(b.losses)}L)</span></td>
      <td>${fmtPct(n(b.wr))}</td>
      <td class="${pnlCls(net)}">${signed(net)}</td>
      <td class="${pnlCls(net)}">${signed(n(b.avg))}</td>
      <td style="color:#888">-${money(b.charges)}</td>
      <td class="${pnlCls(contrib)}">${contrib >= 0 ? "+" : ""}${fmtPct(contrib)}</td>
      <td style="color:#888">${fmtDT(b.last)}</td>
    </tr>`;
  }).join("");
  if (note) note.textContent = `${n(d.strategyCount)} strateg${n(d.strategyCount) === 1 ? "y" : "ies"} · net ${signed(d.strategyTotalNet)}`;
}

function renderTimeOfDay(d) {
  const ins = d.insight || {};
  const cardHost = el("rsInsightBest");
  const hourBody = el("rsHourBody");
  const weekBody = el("rsWeekBody");
  const bh = ins.bestHour;
  if (cardHost) {
    const hhTxt = bh != null ? `${pad2(bh)}:00 – ${pad2(bh)}:59` : null;
    cardHost.innerHTML =
      (hhTxt
        ? `<div style="font-size:10px;color:#888;margin-bottom:6px">Most profitable hour of the day: <b style="color:#00d4aa;font-size:14px">${hhTxt}</b> — ${n(ins.bestHourWins)} winning of ${n(ins.bestHourCount)} trades · win rate ${fmtPct(n(ins.bestHourWr))} · net ${signed(ins.bestHourNet)}</div>`
        : '<div style="font-size:10px;color:#666;margin-bottom:6px">No profitable hour found yet.</div>') +
      (ins.bestNetKey != null
        ? `<div style="font-size:9px;color:#888">Best by net P&L: <b style="color:#ffd700">${pad2(ins.bestNetKey)}:00</b> (${money(ins.bestNetVal)}) · Highest win rate: <b style="color:#ffd700">${ins.bestWrKey != null ? pad2(ins.bestWrKey) + ":00" : "—"}</b> (${ins.bestWrVal != null ? fmtPct(ins.bestWrVal) : "—"})</div>`
        : "");
  }
  const hrs = d.hour || [];
  if (hourBody) {
    if (!hrs.length) hourBody.innerHTML = '<div style="font-size:10px;color:#666;padding:4px 2px">No timed trades.</div>';
    else hourBody.innerHTML = `<table class="rs-table"><thead><tr><th>Hour</th><th>Trades</th><th>Wins</th><th>Losses</th><th>Win rate</th><th>Net P&L</th><th>Avg trade</th></tr></thead><tbody>` +
      hrs.map((b) => {
        const isBest = bh != null && n(b.hour) === n(bh);
        return `<tr${isBest ? ' style="background:#0f2f28"' : ""}>
          <td><b style="color:${isBest ? "#00d4aa" : "#d0d0d0"}">${pad2(n(b.hour))}:00</b>${isBest ? " ★" : ""}</td>
          <td>${n(b.count)}</td><td style="color:#00d4aa">${n(b.wins)}</td><td style="color:#ef5350">${n(b.losses)}</td>
          <td>${fmtPct(n(b.wr))}</td><td class="${pnlCls(n(b.net))}">${signed(b.net)}</td><td style="color:#888">${money(b.avg)}</td></tr>`;
      }).join("") + `</tbody></table>`;
  }
  const wks = d.weekday || [];
  if (weekBody) {
    if (!wks.length) weekBody.innerHTML = '<div style="font-size:10px;color:#666;padding:4px 2px">No timed trades.</div>';
    else weekBody.innerHTML = `<table class="rs-table"><thead><tr><th>Day</th><th>Trades</th><th>Wins</th><th>Losses</th><th>Win rate</th><th>Net P&L</th><th>Avg trade</th></tr></thead><tbody>` +
      wks.map((b) => `<tr><td><b style="color:#d0d0d0">${esc(b.name)}</b></td><td>${n(b.count)}</td><td style="color:#00d4aa">${n(b.wins)}</td><td style="color:#ef5350">${n(b.losses)}</td><td>${fmtPct(n(b.wr))}</td><td class="${pnlCls(n(b.net))}">${signed(b.net)}</td><td style="color:#888">${money(b.avg)}</td></tr>`).join("") +
      `</tbody></table>`;
  }

  const eq = d.equity || {};
  drawChart("rsEquity", "line", eq.labels, eq.values, null);
  const dist = d.dist || {};
  drawChart("rsDist", "bar", dist.labels, dist.values, dist.colors);
  const hs = d.hourSeries || {};
  drawChart("rsHour", "bar", hs.labels, hs.values, hs.colors);
  const ws = d.weekSeries || {};
  drawChart("rsWeek", "bar", ws.labels, ws.values, ws.colors);

  const scopeNote = el("rsInsightScopeNote");
  if (scopeNote) {
    const map = { all: "All history", "30d": "Last 30 days", "7d": "Last 7 days", today: "Today" };
    const sc = UI.scope;
    scopeNote.textContent = ` — ${map[sc] || sc}${n(ins.total) ? " · " + n(ins.total) + " trades" : ""}`;
  }
}

function render() {
  if (!LAST) return;
  renderChips(LAST);
  renderStats(LAST);
  renderTable(LAST);
  renderStrategy(LAST);
  renderTimeOfDay(LAST);
  const rl = el("rsRangeLabel");
  if (rl) {
    const s = LAST.stats || {};
    rl.textContent = `Showing ${LAST.rangeLabel}${n(s.n) ? ` · ${n(s.n)} trades · net ${signed(s.net)}` : ""}`;
  }
}

// ---------------------------------------------------------------------------
// CSV export
// ---------------------------------------------------------------------------
function exportCsv() {
  if (!LAST || !(LAST.trades || []).length) { alert("No trades in the selected period to export."); return; }
  const q = (s) => `"${String(s == null ? "" : s).replace(/"/g, '""')}"`;
  const head = ["Option", "Qty", "Entry", "Exit", "Side", "P&L (net)", "Charges", "Reason", "Entry time", "Exit time", "Engine", "Strategy"];
  const lines = [head.join(",")];
  LAST.trades.slice().reverse().forEach((t) => {
    lines.push([
      q(t.symbol), n(t.qty), n(t.entry), n(t.exit), q(t.side), n(t.net).toFixed(2),
      n(t.charges).toFixed(2), q(t.reason), t.entryAt ? q(fmtDT(t.entryAt)) : "", q(fmtDT(t.at)),
      q(engineFriendly(t.engine)), q(t.strategy || ""),
    ].join(","));
  });
  const blob = new Blob([lines.join("\n")], { type: "text/csv;charset=utf-8;" });
  const a = document.createElement("a");
  a.href = URL.createObjectURL(blob);
  a.download = `${window.__PAPER__ ? "paper" : "realtime"}-executed-trades-${UI.range}.csv`;
  document.body.appendChild(a);
  a.click();
  setTimeout(() => { document.body.removeChild(a); URL.revokeObjectURL(a.href); }, 200);
}

// ---------------------------------------------------------------------------
// Shell / wiring
// ---------------------------------------------------------------------------
function style() {
  const css = `
  #tab-rtstats { display: none; flex-direction: column; gap: 6px; padding: 8px 10px; overflow-y: auto; }
  #tab-rtstats.active { display: flex; }
  #tab-rtstats .rs-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  #tab-rtstats select { background:#1a1a35;border:1px solid #2d2d50;color:#d0d0d0;border-radius:3px;padding:3px 5px;font-size:10px; }
  #tab-rtstats label { font-size:10px;color:#888; }
  #tab-rtstats .rs-chip { font-size:10px;color:#d0d0d0;background:#0e0e24;border:1px solid #2d2d50;border-radius:3px;padding:3px 8px;cursor:pointer;font-family:inherit; }
  #tab-rtstats .rs-chip:hover { border-color:#00d4aa;background:#12122a; }
  #tab-rtstats .rs-table { width:100%;border-collapse:collapse;font-size:10.5px; }
  #tab-rtstats .rs-table th { text-align:left;color:#8888b8;font-weight:500;padding:5px 6px;border-bottom:1px solid #2a2a4a;position:sticky;top:0;background:#111127;z-index:1; }
  #tab-rtstats .rs-table td { padding:4px 6px;border-bottom:1px solid #1c1c38;white-space:nowrap;color:#d0d0d0; }
  #tab-rtstats .rs-table tbody tr:hover td { background:#1a1a35; }
  #tab-rtstats .rs-wrap { max-height:300px;overflow:auto;border:1px solid #1e1e40;border-radius:3px; }
  #tab-rtstats .rt-pos { color:#00d4aa; } #tab-rtstats .rt-neg { color:#ef5350; }
  #tab-rtstats h3 { font-size:11px;color:#888;text-transform:uppercase;margin:6px 0 2px; }
  #tab-rtstats .rs-chart { flex:1;min-width:270px;background:#0e0e24;border:1px solid #1e1e40;border-radius:4px;padding:8px;position:relative; }
  #tab-rtstats .rs-chart-title { font-size:10px;color:#66ccff;font-weight:700;margin-bottom:4px;text-transform:uppercase;letter-spacing:0.5px; }
  #tab-rtstats .rs-chart-body { position:relative;height:175px; }
  #tab-rtstats .rs-empt { position:absolute;top:0;left:0;right:0;bottom:0;display:none;align-items:center;justify-content:center;color:#666;font-size:10px;text-align:center;padding:0 10px; }
  `;
  const e = document.createElement("style");
  e.textContent = css;
  document.head.appendChild(e);
}

function shell() {
  const host = el("tab-rtstats");
  if (!host) return;
  host.innerHTML = `
    <div class="rs-toolbar">
      <h3 style="font-size:12px;color:#00d4aa;text-transform:uppercase;margin:0">Realtime Executed Trades Report</h3>
      <span style="font-size:9px;color:#666">Aggregates every executed trade from the Realtime Trading Engine (real Dhan orders). Net basis when charges are banked.</span>
      <span style="margin-left:auto"></span>
      <label>Mode
        <select id="rsMode">
          <option value="">All trades</option>
          <option value="filter">Indicator-filter mode</option>
          <option value="strategy">Strategy mode</option>
        </select>
      </label>
      <button class="btn-action" id="rsRefresh" style="width:auto;padding:3px 12px;margin:0;font-size:10px">Refresh</button>
      <span id="rsStatus" style="font-size:9px;color:#888"></span>
    </div>

    <div id="rsChips" style="display:flex;gap:5px;flex-wrap:wrap;align-items:center"></div>

    <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;border:1px solid #1e1e40;border-radius:4px;padding:6px 8px;background:#0e0e24">
      <label style="font-size:10px;color:#00d4aa;font-weight:700">Period</label>
      <select id="rsRange">
        <option value="1h">Last 1 hour</option>
        <option value="today">Today</option>
        <option value="week">This week</option>
        <option value="month">This month</option>
        <option value="6m">Last 6 months</option>
        <option value="year">This year</option>
        <option value="all" selected>All time</option>
      </select>
      <span id="rsRangeLabel" style="font-size:10px;color:#888"></span>
      <span style="flex:1"></span>
      <button class="btn-action" id="rsExportCsv" style="width:auto;padding:3px 10px;margin:0;font-size:10px">Export CSV</button>
    </div>

    <div id="rsStats" style="display:flex;gap:6px;flex-wrap:wrap"></div>

    <div style="display:flex;gap:8px;flex-wrap:wrap">
      <div class="rs-chart">
        <div class="rs-chart-title">Cumulative Net P&amp;L (equity curve)</div>
        <div class="rs-chart-body"><canvas id="rsEquity"></canvas><div class="rs-empt" id="rsEquityEmpty"></div></div>
      </div>
      <div class="rs-chart">
        <div class="rs-chart-title">Net P&amp;L distribution (per day / per trade)</div>
        <div class="rs-chart-body"><canvas id="rsDist"></canvas><div class="rs-empt" id="rsDistEmpty"></div></div>
      </div>
    </div>

    <h3>Executed Trades</h3>
    <div class="rs-wrap">
      <table class="rs-table">
        <thead><tr>
          <th>Option</th><th>Qty</th><th>Entry &rarr; Exit</th><th>P&amp;L (net)</th>
          <th>Charges (deducted)</th><th>Reason</th><th>Entry &rarr; Exit time</th>
        </tr></thead>
        <tbody id="rsBody"></tbody>
      </table>
    </div>

    <h3>Strategy-wise breakdown <span id="rsStrategyNote" style="font-size:9px;color:#888;text-transform:none;margin-left:8px"></span></h3>
    <div class="rs-wrap" style="max-height:230px">
      <table class="rs-table">
        <thead><tr>
          <th>Strategy</th><th>Trades (W/L)</th><th>Win rate</th><th>Net P&amp;L</th>
          <th>Avg trade</th><th>Charges</th><th>Contribution</th><th>Last trade</th>
        </tr></thead>
        <tbody id="rsStrategyBody"></tbody>
      </table>
    </div>

    <div style="display:flex;align-items:center;gap:8px;flex-wrap:wrap;margin-top:4px;border-top:1px solid #1e1e40;padding-top:6px">
      <h3 style="margin:0">Most Profitable Time of Day</h3>
      <label>Scope
        <select id="rsInsightScope">
          <option value="all" selected>All history</option>
          <option value="30d">Last 30 days</option>
          <option value="7d">Last 7 days</option>
          <option value="today">Today</option>
        </select>
      </label>
      <span id="rsInsightScopeNote" style="font-size:9px;color:#888"></span>
      <span style="font-size:9px;color:#666;margin-left:auto">Trades grouped by the hour they were entered — the star row is the most profitable hour.</span>
    </div>
    <div id="rsInsightBest" style="font-size:10px;color:#d0d0d0;border:1px solid #0f3d33;background:#0b1c17;border-radius:4px;padding:6px 10px"></div>
    <div style="display:flex;gap:8px;flex-wrap:wrap">
      <div class="rs-chart">
        <div class="rs-chart-title">Net P&amp;L by entry hour</div>
        <div class="rs-chart-body"><canvas id="rsHour"></canvas><div class="rs-empt" id="rsHourEmpty"></div></div>
      </div>
      <div class="rs-chart">
        <div class="rs-chart-title">Net P&amp;L by weekday</div>
        <div class="rs-chart-body"><canvas id="rsWeek"></canvas><div class="rs-empt" id="rsWeekEmpty"></div></div>
      </div>
    </div>
    <div style="display:flex;gap:10px;flex-wrap:wrap;min-width:0">
      <div style="flex:1;min-width:330px" id="rsHourBody"></div>
      <div style="flex:1;min-width:330px" id="rsWeekBody"></div>
    </div>
  `;
}

// Cheap identity of a report payload: when the 3s poll returns the same period
// and the same trades, the full set of tables/charts is already on screen and
// rebuilding every table's innerHTML is pure waste (the old behaviour froze the
// pane on a large ledger). Changes when the range/mode/scope, the stats totals,
// or the newest/oldest trade move.
function reportSig(d) {
  if (!d) return "";
  const s = d.stats || {};
  const tr = d.trades || [];
  const b = tr.length ? n(tr[0].at) : 0;
  const e = tr.length ? n(tr[tr.length - 1].at) : 0;
  return [d.engine, d.armed, d.mode, d.scope, d.range, d.rangeLabel, n(s.n), n(s.net), n(s.winRate), tr.length, b, e, n(d.strategyCount), n(d.strategyTotalNet)].join("|");
}

async function refresh() {
  const q = `range=${encodeURIComponent(UI.range)}&mode=${encodeURIComponent(UI.mode)}&scope=${encodeURIComponent(UI.scope)}`;
  try {
    // Paper report: rewrite to the paper engine (window.__PAPER__ set by host).
    const url = (window.__PAPER__ ? "/api/paper/stats" : "/api/rt/stats") + "?" + q;
    const r = await fetch(url);
    const next = await r.json();
    LAST = next;
    const sig = reportSig(next);
    if (sig === lastRenderSig) return;
    lastRenderSig = sig;
    render();
  } catch (e) {
    const s = el("rsStatus");
    if (s) s.textContent = "load failed";
  }
}

function active() {
  const e = el("tab-rtstats");
  return !!e && e.classList.contains("active");
}

function wire() {
  const mode = el("rsMode");
  if (mode) mode.onchange = () => { UI.mode = mode.value; refresh(); };
  const range = el("rsRange");
  if (range) range.onchange = () => { UI.range = range.value; refresh(); };
  const scope = el("rsInsightScope");
  if (scope) scope.onchange = () => { UI.scope = scope.value; refresh(); };
  const rf = el("rsRefresh");
  if (rf) rf.onclick = () => refresh();
  const ex = el("rsExportCsv");
  if (ex) ex.onclick = () => exportCsv();
  document.addEventListener("tabshown", (ev) => {
    if (ev.detail === "rtstats") refresh();
  });
}

export function bootRtStats() {
  if (booted) return;
  booted = true;
  style();
  shell();
  wire();
  refresh();
  setInterval(() => {
    if (active()) refresh();
  }, 3000);
}

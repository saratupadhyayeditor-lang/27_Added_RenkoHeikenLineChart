// Sidebar: instrument picker, MCX commodities, market watch and the shared
// live-quote engine. Talks to the Rust server (/api/symbols, /api/commodities,
// /ws) and drives the WASM chart via `api.select_symbol`. Quotes arrive only
// over the `/ws` push channel (the client declares its securities there); there
// is no REST quote polling.
//
// Behaviour mirrors the old Python app: a single quote store powers the symbol
// dropdown LTP text, the market-watch rows, the commodity rows and the chart
// header P&L, updated from one merged store.

export function bootSidebar(api) {
  const state = {
    symbols: [],
    watchlists: {},
    groups: {},
    quotes: {},
    selected: null,
    ws: null,
    wsRetry: 0,
    pollTimer: null,
    polling: false,
    started: false,
  };

  const $ = (id) => document.getElementById(id);
  const quoteKey = (opt) =>
    opt.getAttribute("data-exch") === "IDX_I"
      ? "IDX_I:" + opt.value
      : String(opt.value);

  // -------------------------------------------------------------------------
  // Boot: load catalog + commodities, then start the live feed.
  // -------------------------------------------------------------------------
  async function boot() {
    try {
      const res = await fetch("/api/symbols");
      const cat = await res.json();
      state.symbols = cat.symbols || [];
      state.watchlists = cat.watchlists || {};
    } catch (e) {
      console.error("symbols load failed", e);
    }
    try {
      const res = await fetch("/api/commodities");
      const d = await res.json();
      const list = (d.data && d.data.commodities) || [];
      inductCommodities(list);
    } catch (e) {
      console.error("commodities load failed", e);
    }
    computeGroups();
    buildSymbolOptions();
    buildWatchlist();
    renderCommodityRows();
    selectInitial();
    startFeed();
  }

  function inductCommodities(list) {
    if (!list.length) return;
    const existing = new Set(state.symbols.map((s) => String(s[1]) + "|" + s[2]));
    const rows = [];
    for (const c of list) {
      const sid = Number(c.security_id);
      if (!sid || existing.has(String(sid) + "|MCX_COMM")) continue;
      state.symbols.push([
        c.name,
        sid,
        "MCX_COMM",
        "FUTCOM",
        sid,
        "MCX_COMM",
        "Commodities (MCX)",
      ]);
      rows.push([c.name, sid]);
    }
    if (rows.length) state.watchlists["Commodities (MCX)"] = rows;
  }

  function computeGroups() {
    const groups = {};
    for (const s of state.symbols) {
      const g = s[6] || "F&O Stocks (NSE)";
      (groups[g] = groups[g] || []).push(s);
    }
    state.groups = groups;
  }

  // -------------------------------------------------------------------------
  // Instrument picker
  // -------------------------------------------------------------------------
  function buildSymbolOptions() {
    const sel = $("symbolSelect");
    sel.innerHTML = "";
    for (const [grp, items] of Object.entries(state.groups)) {
      const og = document.createElement("optgroup");
      og.label = grp;
      for (const s of items) {
        const [name, id, exch, inst, ocId, ocExch] = s;
        const opt = document.createElement("option");
        opt.value = id;
        opt.setAttribute("data-exch", exch);
        opt.setAttribute("data-inst", inst);
        opt.setAttribute("data-oc-id", ocId);
        opt.setAttribute("data-oc-exch", ocExch);
        opt.setAttribute("data-symbol-name", name);
        opt.textContent = name;
        og.appendChild(opt);
      }
      sel.appendChild(og);
    }
  }

  function filterSymbols() {
    const q = ($("symbolSearch").value || "").toLowerCase();
    const sel = $("symbolSelect");
    for (let _i = 0; _i < sel.options.length; _i++) {
      const opt = sel.options[_i];
      opt.style.display = !q || opt.textContent.toLowerCase().includes(q) ? "" : "none";
    }
  }

  function findOption(sid, exch) {
    const sel = $("symbolSelect");
    for (let _i = 0; _i < sel.options.length; _i++) {
      const opt = sel.options[_i];
      if (Number(opt.value) !== Number(sid)) continue;
      const ex = opt.getAttribute("data-exch") || "";
      if (!exch || ex === exch) return opt;
    }
    return null;
  }

  function selectInitial() {
    const opt = findOption(13, "IDX_I") || $("symbolSelect").options[0];
    if (!opt) return;
    state.selected = optionInfo(opt);
    $("chartSymbolLabel").textContent = state.selected.name;
    publishChartTarget(state.selected);
    publishSelection(state.quotes);
  }

  function optionInfo(opt) {
    return {
      id: Number(opt.value),
      exch: opt.getAttribute("data-exch") || "",
      inst: opt.getAttribute("data-inst") || "",
      name: opt.getAttribute("data-symbol-name") || opt.textContent.split("  ")[0],
    };
  }

  function onSymbolChange() {
    const sel = $("symbolSelect");
    const opt = sel.options[sel.selectedIndex];
    if (!opt) return;
    applySelection(opt);
  }

  function applySelection(opt) {
    applySelectionInfo(optionInfo(opt));
  }

  // Switch the chart to an explicit instrument descriptor. Split out from
  // `applySelection` so a watchlist row that is not present in the symbol picker
  // can still open its chart (previously those clicks silently did nothing).
  function applySelectionInfo(info) {
    state.selected = info;
    const lbl = $("chartSymbolLabel");
    if (lbl) lbl.textContent = info.name;
    const pnl = $("chartPnL");
    if (pnl) pnl.innerHTML = "";
    const tp = $("chartTradePnl");
    if (tp) tp.innerHTML = "";
    markSelectedRow(info.id, info.exch);
    publishChartTarget(info);
    api.select_symbol(info.id, info.exch, info.inst, info.name);
    activateChartTab();
  }

  // Dhan instrument type for a cash/index/commodity watchlist row. Derivative
  // rows carry their own type and are resolved through the symbol picker.
  function instForExch(exch) {
    switch ((exch || "").toUpperCase()) {
      case "IDX_I":
        return "INDEX";
      case "MCX_COMM":
        return "FUTCOM";
      case "NSE_FNO":
      case "BSE_FNO":
        return "OPTIDX";
      default:
        return "EQUITY";
    }
  }

  // Remember which instrument the main chart is showing so the trade-level
  // overlay poller (index.html) knows where to draw the live P&L / SL / trail-SL
  // lines. Also published by the option-chain chart opener.
  function publishChartTarget(info) {
    window.__chartTarget = {
      securityId: Number(info.id || 0),
      exchangeSegment: info.exch || "",
      instrument: info.inst || "",
      label: info.name || "",
    };
  }

  function markSelectedRow(sid, exch) {
    document.querySelectorAll("#marketWatch .mw-row").forEach((r) => {
      const same =
        Number(r.dataset.sid) === Number(sid) && (r.dataset.exch || "") === (exch || "");
      r.classList.toggle("sel", same);
    });
  }

  function activateChartTab() {
    // Drive the shared tab switcher so the chart button, its pane and the
    // `tabshown` event all stay in sync with a real user click.
    const btn = document.querySelector('.tab-btn[data-tab="chart"]');
    if (btn) {
      btn.click();
      return;
    }
    document.querySelectorAll(".tab-btn[data-tab]").forEach((b) =>
      b.classList.toggle("active", b.getAttribute("data-tab") === "chart")
    );
    document.querySelectorAll(".tab-content").forEach((c) =>
      c.classList.toggle("active", c.id === "tab-chart")
    );
  }

  // -------------------------------------------------------------------------
  // Market watch
  // -------------------------------------------------------------------------
  function buildWatchlist() {
    const wrap = $("marketWatch");
    wrap.innerHTML = "";
    for (const [cat, items] of Object.entries(state.watchlists)) {
      const catDiv = document.createElement("div");
      catDiv.className = "mw-cat";

      const head = document.createElement("div");
      head.className = "mw-head";
      head.innerHTML =
        '<span class="mw-caret">&#9654;</span> ' +
        cat +
        ' <span class="mw-count">' +
        items.length +
        "</span>";
      head.onclick = () => catDiv.classList.toggle("open");

      const body = document.createElement("div");
      body.className = "mw-body";

      const load = document.createElement("div");
      load.className = "mw-load";
      load.innerHTML =
        '<div class="mw-subhead">Loading quotes&#8230; <span class="mw-subcount"></span></div>';

      const bull = document.createElement("div");
      bull.className = "mw-sub";
      bull.innerHTML = '<div class="mw-subhead up">Bullish <span class="mw-subcount">0</span></div>';
      const bullBox = document.createElement("div");
      bullBox.className = "mw-bull";
      bull.appendChild(bullBox);

      const bear = document.createElement("div");
      bear.className = "mw-sub";
      bear.innerHTML = '<div class="mw-subhead down">Bearish <span class="mw-subcount">0</span></div>';
      const bearBox = document.createElement("div");
      bearBox.className = "mw-bear";
      bear.appendChild(bearBox);

      for (const [name, sid] of items) {
        const exch = commodityExch(sid);
        const row = document.createElement("div");
        row.className = "mw-row";
        row.dataset.sid = sid;
        row.dataset.name = name;
        row.dataset.exch = exch;
        row.innerHTML =
          '<span class="mw-name">' + name + '</span><span class="mw-ltp">--</span><span class="mw-chg">--</span>';
        row.onclick = () => selectWatchlistSymbol(sid, name, exch);
        load.appendChild(row);
      }
      body.appendChild(load);
      body.appendChild(bull);
      body.appendChild(bear);
      catDiv.appendChild(head);
      catDiv.appendChild(body);
      wrap.appendChild(catDiv);
    }
  }

  function commodityExch(sid) {
    const opt = findOption(sid, "MCX_COMM");
    return opt ? "MCX_COMM" : "NSE_EQ";
  }

  function renderCommodityRows() {
    const wrap = $("commodityWatch");
    const list = state.watchlists["Commodities (MCX)"] || [];
    wrap.innerHTML = "";
    if (!list.length) {
      wrap.innerHTML =
        '<div class="mw-row" style="opacity:.55;cursor:default"><span class="mw-name">No commodities</span><span class="mw-ltp">--</span><span class="mw-chg">--</span></div>';
      return;
    }
    for (const [name, sid] of list) {
      const row = document.createElement("div");
      row.className = "mw-row";
      row.dataset.sid = sid;
      row.dataset.name = name;
      row.dataset.exch = "MCX_COMM";
      row.innerHTML =
        '<span class="mw-name">' + name + '</span><span class="mw-ltp">--</span><span class="mw-chg">--</span>';
      row.onclick = () => selectWatchlistSymbol(sid, name, "MCX_COMM");
      wrap.appendChild(row);
    }
  }

  function selectWatchlistSymbol(sid, name, exch) {
    const opt = findOption(sid, exch);
    if (opt) {
      const sel = $("symbolSelect");
      sel.selectedIndex = opt.index;
      applySelection(opt);
      return;
    }
    // Watchlist-only row (or a commodity the picker does not list): open its
    // chart directly, keyed off the row's own exchange.
    applySelectionInfo({
      id: Number(sid),
      exch: exch || "NSE_EQ",
      inst: instForExch(exch),
      name: name || String(sid),
    });
  }

  function toggleAllWatchlist() {
    const cats = document.querySelectorAll("#marketWatch .mw-cat");
    const anyOpen = Array.from(cats).some((c) => c.classList.contains("open"));
    cats.forEach((c) => c.classList.toggle("open", !anyOpen));
    const btn = $("mwToggleBtn");
    if (btn) btn.textContent = anyOpen ? "Expand All" : "Collapse All";
  }

  function renderGroup(box, list) {
    const cur = Array.from(box.children);
    const same = cur.length === list.length && list.every((r, i) => r === cur[i]);
    if (same) return;
    box.replaceChildren(...list);
  }

  function updateWatchlist(qm) {
    const cats = document.querySelectorAll("#marketWatch .mw-cat");
    for (const cat of cats) {
      const rows = Array.from(cat.querySelectorAll(".mw-row"));
      const quoted = [];
      for (const row of rows) {
        const q = qm[String(row.dataset.sid)];
        if (q && q.change_pct !== undefined) {
          row.dataset.pct = q.change_pct;
          row.querySelector(".mw-ltp").textContent = (q.ltp || 0).toFixed(2);
          const pct = q.change_pct;
          const pts = q.change || 0;
          const chgEl = row.querySelector(".mw-chg");
          chgEl.textContent =
            (pts >= 0 ? "+" : "") +
            pts.toFixed(2) +
            " (" +
            (pct >= 0 ? "+" : "") +
            pct.toFixed(2) +
            "%)";
          chgEl.className = "mw-chg " + (pct >= 0 ? "up" : "down");
          quoted.push(row);
        }
      }
      const bullBox = cat.querySelector(".mw-bull");
      const bearBox = cat.querySelector(".mw-bear");
      const load = cat.querySelector(".mw-load");
      const bull = quoted
        .filter((r) => parseFloat(r.dataset.pct) >= 0)
        .sort((a, b) => parseFloat(b.dataset.pct) - parseFloat(a.dataset.pct));
      const bear = quoted
        .filter((r) => parseFloat(r.dataset.pct) < 0)
        .sort((a, b) => parseFloat(b.dataset.pct) - parseFloat(a.dataset.pct));
      renderGroup(bullBox, bull);
      renderGroup(bearBox, bear);
      if (load) {
        load.style.display = load.children.length > 1 ? "" : "none";
        const lc = load.querySelector(".mw-subcount");
        if (lc) lc.textContent = load.children.length - 1;
      }
      const bullCount = cat.querySelector(".mw-bull .mw-subcount");
      const bearCount = cat.querySelector(".mw-bear .mw-subcount");
      if (bullCount) bullCount.textContent = bull.length;
      if (bearCount) bearCount.textContent = bear.length;
    }
  }

  function updateCommodityWatch(qm) {
    document.querySelectorAll("#commodityWatch .mw-row[data-sid]").forEach((row) => {
      const q = qm[String(row.dataset.sid)];
      if (!q || q.change_pct === undefined) return;
      row.querySelector(".mw-ltp").textContent = (q.ltp || 0).toFixed(2);
      const pct = q.change_pct;
      const pts = q.change || 0;
      const chgEl = row.querySelector(".mw-chg");
      chgEl.textContent =
        (pts >= 0 ? "+" : "") + pts.toFixed(2) + " (" + (pct >= 0 ? "+" : "") + pct.toFixed(2) + "%)";
      chgEl.className = "mw-chg " + (pct >= 0 ? "up" : "down");
    });
  }

  // -------------------------------------------------------------------------
  // Live quote engine
  // -------------------------------------------------------------------------
  function mergeQuotes(qm) {
    if (!qm) return;
    for (const k in qm) {
      const q = qm[k];
      if (q) state.quotes[k] = q;
    }
  }

  // Expose the active chart instrument (plus its live ltp/oi/volume) so the
  // Realtime Trading Engine's Order Placement cards price the exact same symbol
  // the user is viewing, from the same live quote cache.
  function publishSelection(qm) {
    if (!state.selected) return;
    const key = state.selected.exch === "IDX_I" ? "IDX_I:" + state.selected.id : String(state.selected.id);
    const q = (qm && qm[key]) || state.quotes[key] || {};
    window.__chartSelection = {
      id: state.selected.id,
      exch: state.selected.exch,
      inst: state.selected.inst,
      name: state.selected.name,
      ltp: q.ltp || 0,
      oi: q.oi || 0,
      volume: q.volume || 0,
      changePct: q.change_pct,
    };
    window.dispatchEvent(new CustomEvent("chartselection", { detail: window.__chartSelection }));
  }

  function render() {
    // While the feed is not live, keep the last fetched LTP / day-change frozen
    // on screen (no repaint from cached, REST or daily-candle data). Painting
    // resumes on the next tick once the link is live again.
    if (window.__feedDown) return;
    const qm = state.quotes;
    publishSelection(qm);
    const sel = $("symbolSelect");
    if (sel && sel.options.length) {
      for (let _i = 0; _i < sel.options.length; _i++) {
      const opt = sel.options[_i];
        const q = qm[quoteKey(opt)];
        if (q && q.change_pct !== undefined) {
          const pct = q.change_pct;
          const pts = q.change || 0;
          const name = opt.getAttribute("data-symbol-name") || opt.textContent.split("  ")[0];
          opt.textContent =
            name.padEnd(14, " ") +
            "  " +
            (q.ltp || 0).toFixed(2) +
            "  " +
            (pct >= 0 ? "+" : "") +
            pct.toFixed(2) +
            "%  " +
            (pts >= 0 ? "+" : "") +
            pts.toFixed(2);
        }
      }
    }
    updateWatchlist(qm);
    updateCommodityWatch(qm);
    updateChartPnL(qm);
    if (api.update_oc_quotes) {
      try {
        api.update_oc_quotes(JSON.stringify(qm));
      } catch (e) {
        /* option-chain tab not ready */
      }
    }
  }

  function updateChartPnL(qm) {
    const el = $("chartPnL");
    if (!el || !state.selected) return;
    const key =
      state.selected.exch === "IDX_I"
        ? "IDX_I:" + state.selected.id
        : String(state.selected.id);
    const q = qm[key];
    if (!q || q.change_pct === undefined) return;
    const pct = q.change_pct;
    const pts = q.change || 0;
    const isIdx = state.selected.inst === "INDEX";
    const col = pct >= 0 ? "#00d4aa" : "#ff4d6a";
    if (isIdx) {
      el.innerHTML =
        '<span style="color:#aaa;margin:0 4px">|</span> <b style="color:' +
        col +
        '">' +
        (pts >= 0 ? "+" : "") +
        pts.toFixed(2) +
        ' pts</b> <span style="color:' +
        col +
        '">(' +
        (pct >= 0 ? "+" : "") +
        pct.toFixed(2) +
        "%)</span>";
    } else {
      el.innerHTML =
        '<span style="color:#aaa;margin:0 4px">|</span> LTP <b style="color:#fff">' +
        (q.ltp || 0).toFixed(2) +
        '</b> <span style="color:' +
        col +
        '">' +
        (pts >= 0 ? "+" : "") +
        pts.toFixed(2) +
        " (" +
        (pct >= 0 ? "+" : "") +
        pct.toFixed(2) +
        "%)</span>";
    }
  }

  function securitiesList() {
    const sel = $("symbolSelect");
    const out = [];
    if (!sel) return out;
    for (let _i = 0; _i < sel.options.length; _i++) {
      const opt = sel.options[_i];
      out.push({
        security_id: parseInt(opt.value, 10),
        exchange_segment: opt.getAttribute("data-exch"),
      });
    }
    const cmd = state.watchlists["Commodities (MCX)"] || [];
    for (const [, sid] of cmd) {
      out.push({ security_id: sid, exchange_segment: "MCX_COMM" });
    }
    // Option strikes registered by the option-chain tab stay priced live.
    const oc = window.__ocSecurities;
    if (Array.isArray(oc)) {
      for (const s of oc) out.push(s);
    }
    return out;
  }

  // The sidebar is fed exclusively by the `/ws` push channel: the client tells
  // the server which securities it is showing and the server subscribes them on
  // the live Dhan feed. There is no REST quote polling.
  function sendSubscription() {
    const ws = state.ws;
    if (!ws || ws.readyState !== 1) return;
    try {
      ws.send(JSON.stringify({ securities: securitiesList() }));
    } catch (e) {
      /* socket closing; onclose will reconnect */
    }
  }

  function connectStream() {
    try {
      const proto = location.protocol === "https:" ? "wss://" : "ws://";
      const ws = new WebSocket(proto + location.host + "/ws");
      state.ws = ws;
      ws.onopen = () => {
        state.wsRetry = 0;
        sendSubscription();
      };
      ws.onmessage = (ev) => {
        try {
          const m = JSON.parse(ev.data);
          if (m.type === "quotes") {
            mergeQuotes(m.data || {});
            render();
          }
        } catch (e) {
          /* ignore malformed frame */
        }
      };
      ws.onclose = () => {
        state.ws = null;
        const delay = Math.min(5000, 500 * Math.pow(2, state.wsRetry++));
        setTimeout(connectStream, delay);
      };
      ws.onerror = () => {
        try {
          ws.close();
        } catch (e) {}
      };
    } catch (e) {
      setTimeout(connectStream, 2000);
    }
  }

  function startFeed() {
    if (state.started) return;
    state.started = true;
    connectStream();
    // Newly added watchlist rows / option strikes are announced over the same
    // socket; a light 2s resend keeps the feed's subscription set in sync.
    state.pollTimer = setInterval(sendSubscription, 2000);
  }

  // -------------------------------------------------------------------------
  // Wiring
  // -------------------------------------------------------------------------
  function wire() {
    $("symbolSearch").addEventListener("input", filterSymbols);
    $("symbolSelect").addEventListener("change", onSymbolChange);
    $("mwToggleBtn").addEventListener("click", toggleAllWatchlist);
    $("mwRefresh").addEventListener("click", () => sendSubscription());
    $("cmdtyRefresh").addEventListener("click", () => sendSubscription());
    // Re-announce the on-screen securities the moment a Connect completes so the
    // server subscribes them to the live feed without waiting for the 2s sync.
    window.addEventListener("dhan-connected", () => sendSubscription());
    // The link monitor broadcasts every feed-state change. When it goes down the
    // panels are blanked immediately; a fresh /ws push repaints them once live.
    window.addEventListener("dhan-link", (ev) => {
      window.__feedDown = !(ev.detail && ev.detail.live);
    });
  }

  wire();
  boot();
}

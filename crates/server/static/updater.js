// App Update tab (manual GitHub-Releases updater).
//
// The desktop shell (`desktop/src/main.rs`) owns all the real work: checking
// GitHub Releases, downloading, sha256-verifying, swapping files and restarting.
// This module only paints the tab and forwards commands over wry IPC
// (`window.ipc.postMessage`). When the page is opened in a plain browser there
// is no IPC, so the tab shows a "desktop app only" notice and stays inert.
//
// The shell pushes state back by calling
//   window.__algoUpdater.status({ state, message, repo, current, latest, history })
// where `history` is a newest-first array of
//   { time_ms, from, to, status, message }.

import { istDateTime } from "./ist.js?v=1";

const $ = (id) => document.getElementById(id);

// Distribution repo baked into the desktop shell too (desktop/src/updater.rs).
// Pre-filled so a fresh install can update with no manual repo setup.
const DEFAULT_REPO =
  "saratupadhyayeditor-lang/27_Added_RenkoHeikenLineChart";

let booted = false;
let logLines = [];
let hasIpc = false;

const STATE_COLOR = { busy: "#ffb020", error: "#ff4d6a", info: "#00d4aa", ok: "#00d4aa" };

export function bootUpdater() {
  if (booted) return;
  const pane = $("tab-updater");
  if (!pane) return;
  booted = true;

  hasIpc = !!(window.ipc && window.ipc.postMessage);
  renderPane(pane);
  wire();
  defineBridge();

  if (hasIpc) {
    post({ cmd: "ready" });
  } else {
    setNotice("Ye updater sirf desktop app me available hai (browser me nahi).");
  }

  // Refresh repo/version/history whenever the user opens the tab.
  document.addEventListener("tabshown", (e) => {
    if (e.detail === "updater" && hasIpc) post({ cmd: "ready" });
  });
}

function post(obj) {
  try {
    window.ipc.postMessage(JSON.stringify(obj));
  } catch (e) {
    setNotice("IPC error: " + e);
  }
}

function renderPane(pane) {
  pane.innerHTML =
    '<div style="padding:14px;max-width:1080px">' +
    '<h3 style="font-size:12px;color:#00d4aa;text-transform:uppercase;margin:0 0 4px">App Update</h3>' +
    '<div style="font-size:10px;color:#888;margin-bottom:12px">' +
    "Manual updater - GitHub Releases se latest build download karke verify karta hai, phir app ko replace karke restart karta hai." +
    "</div>" +
    '<div style="display:flex;flex-wrap:wrap;gap:14px">' +
    "<!-- source card -->" +
    '<div style="flex:1;min-width:320px;background:#0e0e24;border:1px solid #1e1e40;border-radius:4px;padding:12px">' +
    '<h4 style="font-size:11px;color:#d0d0d0;margin:0 0 8px">1 · Update source</h4>' +
    '<label style="font-size:10px;color:#888;display:block;margin-bottom:3px">GitHub repo URL / owner-repo</label>' +
    '<input id="__updRepo" placeholder="https://github.com/' + DEFAULT_REPO + '" spellcheck="false" ' +
    'value="' + DEFAULT_REPO + '" ' +
    'style="width:100%;box-sizing:border-box;padding:7px 9px;border-radius:4px;border:1px solid #2d2d50;background:#12122a;color:#d0d0d0;font-size:12px">' +
    '<div style="display:flex;gap:8px;margin-top:10px">' +
    '<button id="__updSave" class="btn-action" style="flex:1;padding:7px 12px;font-size:11px;width:auto;margin:0">Save</button>' +
    '<button id="__updUpdate" class="btn-action" style="flex:2;padding:7px 12px;font-size:11px;width:auto;margin:0">Update App</button>' +
    "</div>" +
    '<div id="__updStatus" style="margin-top:10px;color:#9aa3b2;font-size:11px;min-height:16px;white-space:pre-wrap">Loading…</div>' +
    '<div style="margin-top:8px;font-size:11px;color:#888">Current: <b id="__updCurrent" style="color:#d0d0d0">--</b>' +
    ' &nbsp;·&nbsp; Latest: <b id="__updLatest" style="color:#d0d0d0">--</b></div>' +
    "</div>" +
    "<!-- live log card -->" +
    '<div style="flex:1;min-width:320px;background:#0e0e24;border:1px solid #1e1e40;border-radius:4px;padding:12px">' +
    '<h4 style="font-size:11px;color:#d0d0d0;margin:0 0 8px">2 · Live log</h4>' +
    '<pre id="__updLog" style="margin:0;height:220px;overflow:auto;background:#08081a;border:1px solid #1e1e40;' +
    'border-radius:3px;padding:8px;font:10px/1.5 ui-monospace,Consolas,monospace;color:#9aa3b2;white-space:pre-wrap"></pre>' +
    "</div>" +
    "</div>" +
    "<!-- history card -->" +
    '<div style="background:#0e0e24;border:1px solid #1e1e40;border-radius:4px;padding:12px;margin-top:14px">' +
    '<h4 style="font-size:11px;color:#d0d0d0;margin:0 0 8px;display:flex;align-items:center;gap:8px">3 · Update history' +
    '<button id="__updClear" class="btn-action warn" style="width:auto;padding:3px 10px;font-size:10px;margin:0 0 0 auto">Clear</button>' +
    "</h4>" +
    '<div id="__updHistory" style="max-height:300px;overflow-y:auto;border:1px solid #1e1e40;border-radius:3px"></div>' +
    '<div style="font-size:10px;color:#666;margin-top:8px">History disk par save hoti hai, app restart ke baad bhi rehti hai.</div>' +
    "</div>" +
    "</div>";
}

function wire() {
  $("__updSave").addEventListener("click", () => post({ cmd: "save_repo", repo: $("__updRepo").value }));
  $("__updUpdate").addEventListener("click", () => {
    logLines = [];
    $("__updLog").textContent = "";
    setStatus("busy", "checking…");
    post({ cmd: "update", repo: $("__updRepo").value });
  });
  $("__updClear").addEventListener("click", () => post({ cmd: "clear_history" }));
}

// The shell calls window.__algoUpdater.status(payload) after every event.
function defineBridge() {
  window.__algoUpdater = {
    status: function (p) {
      p = p || {};
      if (typeof p.repo === "string" && p.repo) $("__updRepo").value = p.repo;
      if (typeof p.current === "string" && p.current) $("__updCurrent").textContent = "v" + p.current;
      if (typeof p.latest === "string" && p.latest) $("__updLatest").textContent = p.latest;
      if (typeof p.message === "string" && p.message) {
        setStatus(p.state || "info", p.message);
        appendLog(p.message);
      }
      if (Array.isArray(p.history)) renderHistory(p.history);

      const busy = p.state === "busy";
      $("__updUpdate").disabled = busy || !hasIpc;
      $("__updSave").disabled = busy || !hasIpc;
    },
  };
}

function setStatus(state, message) {
  const el = $("__updStatus");
  if (!el) return;
  el.textContent = message;
  el.style.color = STATE_COLOR[state] || "#9aa3b2";
}

function setNotice(message) {
  setStatus("error", message);
  const up = $("__updUpdate");
  const sv = $("__updSave");
  if (up) up.disabled = true;
  if (sv) sv.disabled = true;
}

function appendLog(message) {
  const el = $("__updLog");
  if (!el) return;
  const t = new Date();
  const stamp = istDateTime(t.getTime());
  logLines.push("[" + stamp + "] " + message);
  if (logLines.length > 300) logLines.shift();
  el.textContent = logLines.join("\n");
  el.scrollTop = el.scrollHeight;
}

function renderHistory(list) {
  const box = $("__updHistory");
  if (!box) return;
  if (!list.length) {
    box.innerHTML =
      '<div style="padding:10px;font-size:11px;color:#666">Koi update history nahi.</div>';
    return;
  }
  box.innerHTML = "";
  for (const h of list) {
    const row = document.createElement("div");
    row.style.cssText =
      "display:flex;gap:8px;align-items:baseline;padding:7px 10px;border-bottom:1px solid #16162e;font-size:11px";

    const when = document.createElement("span");
    when.style.cssText = "color:#888;white-space:nowrap;font:10px ui-monospace,monospace";
    when.textContent = h.time_ms ? istDateTime(h.time_ms) : "--";

    const badge = document.createElement("span");
    const st = String(h.status || "info");
    badge.style.cssText =
      "font-weight:600;white-space:nowrap;color:" + (STATE_COLOR[st] || "#9aa3b2");
    badge.textContent = st.toUpperCase();

    const ver = document.createElement("span");
    ver.style.cssText = "color:#d0d0d0;white-space:nowrap";
    ver.textContent = "v" + (h.from || "?") + " → " + (h.to || "?");

    const msg = document.createElement("span");
    msg.style.cssText = "color:#9aa3b2;flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis";
    msg.title = h.message || "";
    msg.textContent = h.message || "";

    row.appendChild(when);
    row.appendChild(badge);
    row.appendChild(ver);
    row.appendChild(msg);
    box.appendChild(row);
  }
}

// The shell also, for its own self-test, checks that the tab exists; expose the
// element ids it probes.

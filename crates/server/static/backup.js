/* Backup & Restore system for the complete algo suite (Rust port).
 *
 * Port of the old Python app's `static/backup.js`. Captures the ENTIRE algo
 * system state in two parts:
 *   1. browser localStorage (chart/UI preferences, monitor lists), and
 *   2. the server-side durable engine state of BOTH engines - realtime and
 *      paper - read from `/api/{rt,paper}/state/export`. That is every engine's
 *      settings, indicator filters, saved templates, staging/final strategy
 *      lists, open positions and the full closed-trade ledger (trade stats).
 *
 * Features:
 *   - Export Now: write a full timestamped snapshot to the configured path
 *     (Windows PC path supported) via the Rust server.
 *   - Download Backup: same full snapshot as a .json download, works even when
 *     no path is configured.
 *   - Import / Restore: apply a backup file or a server-stored snapshot back
 *     into localStorage *and* both engines, then reload. A safety copy is taken
 *     before restoring.
 *   - Auto incremental backup: enable/disable toggle + minute/hour/daily/weekly
 *     schedule. A background timer fires when the scheduled time is reached and
 *     catches up immediately after the page loads if a schedule was missed while
 *     the app was closed. Old auto snapshots are auto-pruned (keep N).
 */

import { istDateTime } from "./ist.js?v=1";

const BACKUP_API = "/api/backup";
const POLL_MS = 60000; // auto-backup scheduler cadence
let _cfg = null;
let _busy = false;
let _timer = null;
let _dirHandle = null; // File System Access API directory handle for direct-to-PC saves
const _DIR_DB = "algodhan-backup-handles";

function $id(id) { return document.getElementById(id); }
function fmtTime(iso) {
  if (iso == null || iso === "") return "\u2014";
  try {
    const ms = typeof iso === "number" ? iso : Date.parse(iso);
    if (isNaN(ms)) return iso;
    return istDateTime(ms);
  } catch (e) { return iso; }
}
function sizeFmt(b) {
  if (!b && b !== 0) return "\u2014";
  if (b < 1024) return b + " B";
  if (b < 1048576) return (b / 1024).toFixed(1) + " KB";
  return (b / 1048576).toFixed(2) + " MB";
}
function esc(s) {
  return String(s == null ? "" : s).replace(/[&<>"']/g, function (c) {
    return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
  });
}

function api(path, opts) {
  opts = opts || {};
  return fetch(BACKUP_API + path, {
    method: opts.method || "GET",
    headers: opts.body ? { "Content-Type": "application/json" } : undefined,
    body: opts.body ? JSON.stringify(opts.body) : undefined,
  }).then(function (r) {
    return r.json().catch(function () { return { ok: false, message: "Bad server response" }; });
  });
}

/* ---- IndexedDB persistence for the chosen PC folder ----------------------- */

function idbOpen() {
  return new Promise(function (resolve, reject) {
    if (!window.indexedDB) { reject(new Error("IndexedDB unavailable")); return; }
    const req = window.indexedDB.open(_DIR_DB, 1);
    req.onupgradeneeded = function () {
      const db = req.result;
      if (!db.objectStoreNames.contains("handles")) db.createObjectStore("handles");
    };
    req.onsuccess = function () { resolve(req.result); };
    req.onerror = function () { reject(req.error); };
  });
}

function persistDirHandle(handle) {
  return idbOpen().then(function (db) {
    return new Promise(function (resolve) {
      const tx = db.transaction("handles", "readwrite");
      tx.objectStore("handles").put(handle, "backupDir");
      tx.oncomplete = function () { resolve(); };
      tx.onerror = function () { resolve(); };
    });
  }).catch(function () {});
}

function loadDirHandle() {
  return idbOpen().then(function (db) {
    return new Promise(function (resolve) {
      const tx = db.transaction("handles", "readonly");
      const get = tx.objectStore("handles").get("backupDir");
      get.onsuccess = function () { resolve(get.result || null); };
      get.onerror = function () { resolve(null); };
    });
  }).catch(function () { return null; });
}

function adoptDirHandle(handle) {
  if (!handle) return;
  const per = handle.queryPermission ? handle.queryPermission({ mode: "readwrite" }) : Promise.resolve("granted");
  per.then(function (state) {
    if (state === "granted") {
      _dirHandle = handle;
      const nameEl = $id("backupDirName");
      if (nameEl) nameEl.textContent = "Saving to: " + (handle.name || "selected folder") + " \u2713";
      const autoEl = $id("backupAutoDirName");
      if (autoEl) autoEl.textContent = "Saving to: " + (handle.name || "selected folder") + " \u2713";
    }
  }).catch(function () {});
}

/* ---- snapshot build/apply ------------------------------------------------- */

function gatherLS() {
  const out = {};
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      out[k] = localStorage.getItem(k);
    }
  } catch (e) {}
  return out;
}

/* Server-side engine state: the Rust app persists every engine's settings,
 * strategies, saved AST templates, positions and the full closed ledger (trade
 * statistics) in server files, NOT in localStorage. A complete backup must
 * capture both engines (real + paper) no matter which tab runs it. */
function collectEngineState() {
  const grab = (url) => fetch(url).then(function (r) { return r.json(); }).catch(function () { return null; });
  return Promise.all([
    grab("/api/rt/state/export"),
    grab("/api/paper/state/export"),
    grab("/api/paper2/state/export"),
  ]).then(function (res) {
    const engine = {};
    if (res[0] && res[0].ok && res[0].state) engine.realtime = res[0].state;
    if (res[1] && res[1].ok && res[1].state) engine.paper = res[1].state;
    if (res[2] && res[2].ok && res[2].state) engine.paper2 = res[2].state;
    return engine;
  });
}

/* Engine state does NOT live under /api/backup, so it must be posted to the
 * engine routes directly (a plain relative fetch, same origin as the page). */
function postEngineState(url, state) {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ state: state }),
  }).then(function (r) {
    return r.json().catch(function () { return { ok: false, message: "Bad server response" }; });
  });
}

function buildSnapshot(kind) {
  const ls = gatherLS();
  return collectEngineState().then(function (engine) {
    const engines = (engine.realtime ? 1 : 0) + (engine.paper ? 1 : 0) + (engine.paper2 ? 1 : 0);
    return {
      format: "algodhan_backup",
      version: 2,
      created_at: new Date().toISOString(),
      kind: kind || "manual",
      app: "Smart NTrader + Algo Suite",
      count: { keys: Object.keys(ls).length, engines: engines },
      data: { localStorage: ls, engine: engine },
      config: _cfg || null,
    };
  });
}

function applyData(data) {
  const payload = data && data.data;
  if (!payload || typeof payload !== "object") throw new Error("Invalid backup: missing data");
  let n = 0;
  const ls = payload.localStorage;
  if (ls && typeof ls === "object") {
    Object.keys(ls).forEach(function (k) { localStorage.setItem(k, ls[k]); n++; });
  }
  const engine = payload.engine;
  if (!ls && !engine) throw new Error("Invalid backup: no localStorage or engine state");
  const jobs = [];
  if (engine && engine.realtime) jobs.push(postEngineState("/api/rt/state/import", engine.realtime));
  if (engine && engine.paper) jobs.push(postEngineState("/api/paper/state/import", engine.paper));
  if (engine && engine.paper2) jobs.push(postEngineState("/api/paper2/state/import", engine.paper2));
  return Promise.all(jobs).then(function (results) {
    results.forEach(function (r) {
      if (r && r.ok) n += (r.restored || 0);
      else throw new Error((r && r.message) || "engine restore failed");
    });
    return n;
  });
}

/* ---- actions -------------------------------------------------------------- */

function saveConfig(partial) {
  return api("/config", { method: "POST", body: partial }).then(function (r) {
    if (r.ok) _cfg = r.config;
    return r;
  });
}

function backupNow(kind) {
  return buildSnapshot(kind || "manual")
    .then(function (snapshot) {
      return api("/snapshot", { method: "POST", body: { data: snapshot.data, kind: snapshot.kind } });
    })
    .then(function (r) {
      if (r.ok && r.config) _cfg = r.config;
      return r;
    });
}

function downloadBackup(snapshot) {
  const got = snapshot ? Promise.resolve(snapshot) : buildSnapshot("manual");
  return got.then(function (snap) {
    const blob = new Blob([JSON.stringify(snap, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    const ts = new Date().toISOString().replace(/[:T]/g, "").slice(0, 15);
    a.href = url;
    a.download = "algodhan_backup_" + ts + ".json";
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    setTimeout(function () { URL.revokeObjectURL(url); }, 2000);
    return snap;
  });
}

/* ---- direct save to a PC folder (File System Access API) ------------------ */

function chooseDir() {
  const nameEl = $id("backupDirName");
  if (!window.showDirectoryPicker) {
    if (nameEl) nameEl.textContent = "Not supported in this browser \u2014 exports will download instead.";
    statusLine("Folder picker not supported here \u2014 the export will download as a file.", true);
    return;
  }
  window.showDirectoryPicker({ mode: "readwrite", id: "algodhan-backup" })
    .then(function (handle) {
      _dirHandle = handle;
      persistDirHandle(handle);
      if (nameEl) nameEl.textContent = "Saving to: " + (handle.name || "selected folder") + " \u2713";
      const autoEl = $id("backupAutoDirName");
      if (autoEl) autoEl.textContent = "Saving to: " + (handle.name || "selected folder") + " \u2713";
      statusLine("Folder chosen: " + (handle.name || "PC folder") + " \u2014 Export Now and Auto Backup will save straight into it.", false);
    })
    .catch(function (err) {
      if (err && err.name === "AbortError") {
        statusLine("Folder selection cancelled.", false);
        return;
      }
      if (nameEl) nameEl.textContent = "Folder access blocked \u2014 exports will download instead.";
      statusLine("Could not open folder: " + (err && err.message ? err.message : "permission denied"), true);
    });
}

function saveToDir(snapshot) {
  if (!_dirHandle) return Promise.reject(new Error("No folder chosen"));
  const ts = new Date().toISOString().replace(/[:T]/g, "").slice(0, 15);
  const name = "algodhan_backup_" + ts + ".json";
  return _dirHandle.getFileHandle(name, { create: true })
    .then(function (fh) {
      return fh.createWritable().then(function (w) {
        return w.write(JSON.stringify(snapshot, null, 2)).then(function () { return w.close(); });
      });
    })
    .then(function () { return name; });
}

/* Keep only the newest `keep` auto backup files in the chosen PC folder,
 * mirroring the server-side pruning, so the folder never grows unbounded. */
function pruneDir(keep) {
  if (!_dirHandle || !_dirHandle.values) return Promise.resolve();
  keep = keep || 30;
  const files = [];
  const walker = _dirHandle.values();
  const next = function () {
    return walker.next().then(function (step) {
      if (step.done) {
        files.sort(function (a, b) { return b.mtime - a.mtime; });
        const extra = files.slice(keep);
        const jobs = extra.map(function (f) {
          return f.handle.remove().catch(function () {});
        });
        return Promise.all(jobs).then(function () {});
      }
      const entry = step.value;
      if (entry && entry.kind === "file" && /^backup_auto_.*\.json$/.test(entry.name)) {
        return entry.getFile().then(function (f) {
          files.push({ name: entry.name, handle: entry, mtime: f.lastModified });
          return next();
        }).catch(function () { return next(); });
      }
      return next();
    });
  };
  return next().catch(function () {});
}

/* Auto backup: write to the chosen PC folder when available, otherwise fall
 * back to the server path. Scheduling state is always recorded server-side. */
function autoBackup() {
  return buildSnapshot("auto").then(function (snapshot) {
    if (_dirHandle && window.showDirectoryPicker) {
      return saveToDir(snapshot).then(function (name) {
        const keep = parseInt(($id("backupKeep") && $id("backupKeep").value) || "30", 10) || 30;
        return pruneDir(keep).then(function () { return name; });
      }).then(function (name) {
        // Record schedule/lastResult server-side without writing a server file.
        return api("/snapshot", { method: "POST", body: { data: snapshot.data, kind: "auto", pc_only: true } })
          .then(function (r) {
            if (r.ok && r.config) _cfg = r.config;
            return { ok: true, file: name, dir: true, r: r };
          })
          .catch(function () { return { ok: true, file: name, dir: true, r: null }; });
      }).catch(function () {
        // PC save failed (permission etc.) -> fall back to server path.
        return backupNow("auto").then(function (r) {
          return { ok: r.ok, file: r.file, dir: false, r: r, message: r.message };
        });
      });
    }
    return backupNow("auto").then(function (r) {
      return { ok: r.ok, file: r.file, dir: false, r: r, message: r.message };
    });
  });
}

function readServerFile(name) {
  return api("/read?file=" + encodeURIComponent(name));
}

function importPayload(data) {
  // Optionally keep a server copy of the import.
  return api("/import", { method: "POST", body: { data: data.data } }).then(function () { return data; });
}

function doRestore(data, sourceLabel) {
  // Safety copy of the CURRENT state before it is overwritten, so a bad
  // restore can always be rolled back. Best-effort: works when a path is set.
  return backupNow("pre_restore")
    .catch(function () { return null; })
    .then(function () { return applyData(data); })
    .then(function (n) {
      statusLine("Restored " + n + " items from " + (sourceLabel || "backup") + " \u2014 reloading\u2026", false);
      setTimeout(function () { location.reload(); }, 1100);
    })
    .catch(function (e) {
      statusLine("Restore failed: " + (e && e.message ? e.message : e), true);
    });
}

function restoreServerFile(name) {
  return readServerFile(name).then(function (r) {
    if (!r.ok || !r.backup) { statusLine("Restore failed: " + (r.message || "cannot read backup"), true); return; }
    return doRestore(r.backup, name);
  });
}

function handleImportFile(file) {
  const reader = new FileReader();
  reader.onload = function () {
    let data;
    try { data = JSON.parse(reader.result); }
    catch (e) { statusLine("Import failed: not valid JSON", true); return; }
    if (!data || data.format !== "algodhan_backup" || !data.data) {
      statusLine("Import failed: not an algodhan backup file", true);
      return;
    }
    importPayload(data)
      .catch(function () { return null; })
      .then(function () { return backupNow("pre_restore").catch(function () { return null; }); })
      .then(function () { return applyData(data); })
      .then(function (n) {
        statusLine("Imported " + n + " items \u2014 reloading\u2026", false);
        setTimeout(function () { location.reload(); }, 1100);
      })
      .catch(function (e) {
        statusLine("Import failed: " + (e && e.message ? e.message : e), true);
      });
  };
  reader.readAsText(file);
}

/* ---- auto-backup scheduler ------------------------------------------------- */

function tick() {
  if (!_cfg) return;
  if (_busy) return;
  if (!_cfg.enabled) return;
  let next = _cfg.nextDue ? Date.parse(_cfg.nextDue) : 0;
  if (isNaN(next)) next = 0;
  if (Date.now() >= next) {
    _busy = true;
    autoBackup().then(function (res) {
      _busy = false;
      if (!res.ok) { statusLine("Auto backup failed: " + (res.message || "unknown"), true); }
      else if (res.dir) { statusLine("Auto backup saved to PC folder: " + res.file, false); }
      else { statusLine("Auto backup saved: " + res.file, false); }
      refresh();
    });
  }
}

/* ---- UI -------------------------------------------------------------------- */

function statusLine(msg, isErr) {
  const el = $id("backupStatus");
  if (!el) return;
  el.style.color = isErr ? "#ff4d6a" : "#00d4aa";
  el.textContent = msg;
}

function renderHistory(list) {
  const box = $id("backupHistory");
  if (!box) return;
  if (!list || !list.length) {
    box.innerHTML = '<div style="color:#666;font-size:11px;padding:8px">No server backups yet. Export now to save a backup directly to your PC.</div>';
    return;
  }
  let html = '<table class="account-table" style="width:100%;font-size:11px"><thead><tr><th>File</th><th>Size</th><th>Created</th><th></th></tr></thead><tbody>';
  list.slice(0, 25).forEach(function (f) {
    const kind = /^backup_auto_/.test(f.name) ? '<span style="color:#66ccff;font-size:9px">AUTO</span>' :
                 /^backup_pre_restore_/.test(f.name) ? '<span style="color:#b39ddb;font-size:9px">PRE</span>' : '';
    html += '<tr style="border-bottom:1px solid #1e1e40">' +
      '<td>' + esc(f.name) + ' ' + kind + '</td>' +
      '<td>' + sizeFmt(f.size) + '</td>' +
      '<td>' + fmtTime(f.mtime) + '</td>' +
      '<td><button class="btn-action" data-restore="' + esc(f.name) + '" style="padding:2px 8px;font-size:10px;width:auto;margin:0">Restore</button></td>' +
      '</tr>';
  });
  html += '</tbody></table>';
  box.innerHTML = html;
  box.querySelectorAll("[data-restore]").forEach(function (btn) {
    btn.addEventListener("click", function () {
      const name = btn.getAttribute("data-restore");
      if (confirm("Restore '" + name + "'? Current data will be replaced (a safety copy is saved first). Page will reload.")) {
        restoreServerFile(name);
      }
    });
  });
}

function setValIfBlurred(id, val) {
  const el = $id(id);
  if (!el) return;
  if (document.activeElement && document.activeElement === el) return; // don't clobber typing
  el.value = val;
}

/* Fade out every timeframe card except the selected one, and disable the
 * inputs inside the inactive cards so the schedule stays unambiguous. */
function applyTFState() {
  const cards = document.querySelectorAll(".backup-tf-card");
  const sel = document.querySelector('input[name="backupTF"]:checked');
  const selVal = sel ? sel.value : "minute";
  cards.forEach(function (card) {
    const radio = card.querySelector('input[name="backupTF"]');
    const active = radio && radio.value === selVal;
    card.style.opacity = active ? "1" : "0.35";
    card.style.borderColor = active ? "#00d4aa" : "#2d2d50";
    card.querySelectorAll("input,select").forEach(function (el) {
      if (el.type !== "radio") el.disabled = !active;
    });
  });
}

function render() {
  return api("/list").then(function (r) {
    if (!r.ok) { statusLine(r.message || "Cannot reach backup server", true); return; }
    _cfg = r.config;
    const c = r.config || {};
    $id("backupEnabled").checked = !!c.enabled;
    const sch = c.schedule || {};
    const schedType = ["minute", "hour", "daily", "weekly"].indexOf(sch.type) >= 0 ? sch.type : "minute";
    const radio = document.querySelector('input[name="backupTF"][value="' + schedType + '"]');
    if (radio) radio.checked = true;
    setValIfBlurred("backupMinInterval", sch.interval != null ? String(sch.interval) : "5");
    setValIfBlurred("backupHourInterval", sch.interval != null ? String(sch.interval) : "6");
    setValIfBlurred("backupTime", sch.time || "18:00");
    setValIfBlurred("backupWeekTime", sch.time || "18:00");
    setValIfBlurred("backupWeekday", String(sch.weekday || 0));
    setValIfBlurred("backupKeep", String(sch.keep || 30));
    applyTFState();
    const next = c.nextDue;
    if (c.enabled && next) {
      $id("backupNextDue").textContent = "Next auto backup: " + fmtTime(next);
    } else if (c.enabled) {
      $id("backupNextDue").textContent = "Next auto backup: as soon as a backup is due (immediate catch-up)";
    } else {
      $id("backupNextDue").textContent = "Auto backup disabled";
    }
    const lr = c.lastResult || {};
    $id("backupLastInfo").textContent = (lr.message || "ready") + (lr.at ? " \u00b7 " + fmtTime(lr.at) : "") + (lr.file ? " \u00b7 " + lr.file : "");
    const autoDot = $id("backupAutoDot");
    if (autoDot) { autoDot.style.background = c.enabled ? "#00d4aa" : "#666"; }
    statusLine("Backup server ready \u2014 " + (c.enabled ? "auto backup ON" : "auto backup OFF"), false);
    renderHistory(r.list);
  }).catch(function () { statusLine("Cannot reach backup server", true); });
}

function refresh() { return render(); }

function wire() {
  $id("backupChooseDirBtn").addEventListener("click", function () {
    chooseDir();
  });

  $id("backupExportBtn").addEventListener("click", function () {
    if (_busy) return;
    _busy = true;
    statusLine("Building full backup (settings + strategies + trade ledger)\u2026", false);
    buildSnapshot("manual").then(function (snapshot) {
      const countTxt = snapshot.count.keys + " keys + " + (snapshot.count.engines || 0) + " engines";
      if (_dirHandle && window.showDirectoryPicker) {
        return saveToDir(snapshot).then(function (name) {
          statusLine("Backup saved directly to PC folder: " + name, false);
          $id("backupLastInfo").textContent = "saved to PC \u00b7 " + fmtTime(snapshot.created_at);
        }).catch(function (err) {
          if (err && err.message === "No folder chosen") {
            downloadBackup(snapshot);
            statusLine("Downloaded backup (" + countTxt + ")", false);
          } else {
            statusLine("Direct save failed (" + (err && err.message || "error") + ") \u2014 downloaded instead.", true);
            downloadBackup(snapshot);
          }
        });
      }
      downloadBackup(snapshot);
      statusLine("Downloaded backup (" + countTxt + ")", false);
    }).catch(function (e) {
      statusLine("Backup failed: " + (e && e.message ? e.message : e), true);
    }).then(function () { _busy = false; });
  });

  $id("backupDownloadBtn").addEventListener("click", function () {
    downloadBackup().then(function (snap) {
      statusLine("Downloaded backup (" + snap.count.keys + " keys + " + (snap.count.engines || 0) + " engines)", false);
    }).catch(function (e) {
      statusLine("Download failed: " + (e && e.message ? e.message : e), true);
    });
  });

  $id("backupImportBtn").addEventListener("click", function () {
    $id("backupFileInput").click();
  });
  $id("backupFileInput").addEventListener("change", function (ev) {
    const f = ev.target.files && ev.target.files[0];
    if (f) handleImportFile(f);
    ev.target.value = "";
  });

  $id("backupAutoSavePcBtn").addEventListener("click", function () {
    chooseDir();
  });

  document.querySelectorAll('input[name="backupTF"]').forEach(function (radio) {
    radio.addEventListener("change", applyTFState);
  });

  $id("backupSaveCfg").addEventListener("click", function () {
    const radio = document.querySelector('input[name="backupTF"]:checked');
    const schedType = radio ? radio.value : "minute";
    let interval = null;
    let time = "18:00";
    let weekday = 0;
    if (schedType === "minute") {
      interval = parseInt($id("backupMinInterval").value, 10) || 5;
    } else if (schedType === "hour") {
      interval = parseInt($id("backupHourInterval").value, 10) || 6;
    } else if (schedType === "weekly") {
      time = $id("backupWeekTime").value || "18:00";
      weekday = parseInt($id("backupWeekday").value, 10) || 0;
    } else {
      time = $id("backupTime").value || "18:00";
    }
    saveConfig({
      enabled: $id("backupEnabled").checked,
      schedule: {
        type: schedType,
        interval: interval,
        time: time,
        weekday: weekday,
        keep: parseInt($id("backupKeep").value, 10) || 30,
      },
    }).then(function (r) {
      if (!r.ok) { statusLine(r.message || "Save failed", true); return; }
      statusLine(r.config.enabled ? "Auto backup enabled \u2014 schedule saved" : "Auto backup disabled \u2014 settings saved", false);
      render();
      if (r.config.enabled && !_busy) {
        _busy = true;
        autoBackup().then(function (res) {
          _busy = false;
          if (!res.ok) statusLine("Auto backup enabled but first save failed: " + (res.message || "unknown"), true);
          else if (res.dir) statusLine("Auto backup enabled \u2014 first backup saved to PC folder: " + res.file, false);
          else statusLine("Auto backup enabled \u2014 first backup saved: " + res.file, false);
          refresh();
        });
      }
    });
  });
}

function init() {
  if (!document.getElementById("backupPanel")) return; // panel not mounted
  wire();
  render();
  loadDirHandle().then(adoptDirHandle); // restore the chosen PC folder (if permission granted)
  if (_timer) clearInterval(_timer);
  _timer = setInterval(tick, POLL_MS);
  setTimeout(tick, 2500); // catch-up check right after load
}

// Cross-tab wiring: refreshing the Backup tab re-renders status/history, exactly
// like the old app's switchTab('backup', ...) -> KickRefresh.backup() hook.
document.addEventListener("tabshown", function (e) {
  if (e && e.detail === "backup") { try { render(); } catch (_) {} }
});

// Old-app global: the panel's Refresh button calls KickRefresh.backup().
window.KickRefresh = window.KickRefresh || {};
window.KickRefresh.backup = function () { try { render(); } catch (_) {} };

window.BackupSys = {
  init: init,
  render: render,
  refresh: refresh,
  getConfig: function () { return _cfg; },
  backupNow: backupNow,
  autoBackup: autoBackup,
  download: downloadBackup,
  importPayload: importPayload,
  restoreServerFile: restoreServerFile,
  handleImportFile: handleImportFile,
  applyData: applyData,
  tick: tick,
  chooseDir: chooseDir,
  saveToDir: saveToDir,
  pruneDir: pruneDir,
};

export function bootBackup() {
  init();
}

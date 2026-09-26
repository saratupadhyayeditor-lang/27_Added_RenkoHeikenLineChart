// Self-contained updater UI, injected into the app's pages by the shell.
//
// This does not touch the app's own HTML/CSS/JS. It only adds a small floating
// panel that talks to the shell over wry IPC (`window.ipc.postMessage`), so the
// app stays 100% unported.

pub fn injected_script() -> String {
    r##"(function () {
  window.__auRan = (window.__auRan || 0) + 1;
  try {
  if (window.__algoUpdater) { return; }

  var css = [
    "#__auBtn{position:fixed;right:16px;bottom:16px;z-index:2147483000;padding:8px 14px;",
    "border:1px solid #3a3f4b;border-radius:20px;background:#1f2430;color:#e8eaf0;",
    "font:600 12px system-ui,sans-serif;cursor:pointer;box-shadow:0 4px 14px rgba(0,0,0,.45)}",
    "#__auBtn:hover{background:#2a3140}",
    "#__auPanel{position:fixed;right:16px;bottom:60px;z-index:2147483000;width:320px;",
    "background:#161a22;color:#e8eaf0;border:1px solid #3a3f4b;border-radius:12px;",
    "padding:14px;box-shadow:0 10px 30px rgba(0,0,0,.55);font:12px system-ui,sans-serif}",
    "#__auPanel h4{margin:0 0 8px;font-size:13px}",
    "#__auPanel label{display:block;margin:8px 0 4px;color:#9aa3b2;font-size:11px}",
    "#__auRepo{width:100%;box-sizing:border-box;padding:7px 9px;border-radius:7px;",
    "border:1px solid #3a3f4b;background:#0e1117;color:#e8eaf0;font-size:12px}",
    "#__auRow{display:flex;gap:8px;margin-top:10px}",
    "#__auSave,#__auUpdate{flex:1;padding:8px;border-radius:7px;border:1px solid #3a3f4b;",
    "background:#232a37;color:#e8eaf0;font:600 12px system-ui;cursor:pointer}",
    "#__auUpdate{background:#2b5bd7;border-color:#2b5bd7}",
    "#__auUpdate:disabled{opacity:.55;cursor:default}",
    "#__auStatus{margin-top:10px;color:#9aa3b2;font-size:11px;min-height:16px;white-space:pre-wrap}",
    "#__auClose{position:absolute;top:8px;right:10px;background:none;border:none;color:#9aa3b2;",
    "cursor:pointer;font-size:14px}"
  ].join("");

  function boot() {
    try {
    if (window.__algoUpdater) { return; }
    var style = document.createElement("style");
    style.textContent = css;
    (document.head || document.documentElement).appendChild(style);

    var btn = document.createElement("button");
    btn.id = "__auBtn";
    btn.textContent = "Update";

    var panel = document.createElement("div");
    panel.id = "__auPanel";
    panel.style.display = "none";
    panel.innerHTML =
      '<button id="__auClose">x</button>' +
      "<h4>App Updater</h4>" +
      '<label>GitHub repo URL / owner-repo</label>' +
      '<input id="__auRepo" placeholder="https://github.com/owner/repo" spellcheck="false">' +
      '<div id="__auRow"><button id="__auSave">Save</button>' +
      '<button id="__auUpdate">Update App</button></div>' +
      '<div id="__auStatus">Ready.</div>';

    document.body.appendChild(btn);
    document.body.appendChild(panel);

    var repoInput = panel.querySelector("#__auRepo");
    var statusEl = panel.querySelector("#__auStatus");

    function post(obj) {
      try {
        if (window.ipc && window.ipc.postMessage) {
          window.ipc.postMessage(JSON.stringify(obj));
        } else {
          statusEl.textContent = "IPC available nahi (shell ke through chalao).";
        }
      } catch (e) {
        statusEl.textContent = "IPC error: " + e;
      }
    }

    window.__algoUpdater = {
      status: function (p) {
        p = p || {};
        if (typeof p.repo === "string" && p.repo) {
          repoInput.value = p.repo;
        }
        var line = p.message || "";
        if (p.current) {
          line += (line ? "\n" : "") + "current: v" + p.current;
        }
        if (p.latest) {
          line += "\nlatest: " + p.latest;
        }
        statusEl.textContent = line;
        var busy = p.state === "busy";
        panel.querySelector("#__auUpdate").disabled = busy;
        panel.querySelector("#__auSave").disabled = busy;
      }
    };

    btn.addEventListener("click", function () {
      panel.style.display = panel.style.display === "none" ? "block" : "none";
      post({ cmd: "ready" });
    });
    panel.querySelector("#__auClose").addEventListener("click", function () {
      panel.style.display = "none";
    });
    panel.querySelector("#__auSave").addEventListener("click", function () {
      post({ cmd: "save_repo", repo: repoInput.value });
    });
    panel.querySelector("#__auUpdate").addEventListener("click", function () {
      statusEl.textContent = "checking...";
      post({ cmd: "update", repo: repoInput.value });
    });
    } catch (e) { window.__auErr = String((e && e.stack) || e); }
  }

  function start() {
    if (document.body) { boot(); } else { setTimeout(start, 50); }
  }
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start);
  }
  start();
  } catch (e) { window.__auErr = String((e && e.stack) || e); }
})();"##
        .to_string()
}

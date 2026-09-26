// Superseded: the updater UI now lives in the app itself as a normal tab.
//
// It was previously injected by the shell as a floating panel. It is now a
// first-class tab rendered by `crates/server/static/updater.js`, which talks to
// this shell over wry IPC (`window.ipc.postMessage`) exactly like before. The
// shell pushes status/log/history back with
// `window.__algoUpdater.status({...})`.
//
// This file is intentionally not compiled (`mod ui;` was removed from main.rs).

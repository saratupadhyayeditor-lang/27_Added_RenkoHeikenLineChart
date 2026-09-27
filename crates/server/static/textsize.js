// Global UI text-size control.
//
// The whole dashboard is built from fixed px font sizes, so a single root
// font-size cannot rescale it. Instead every operator-facing page applies a
// CSS `zoom` to its root element, which scales text (and the surrounding
// layout) uniformly. The value is stored in localStorage so it survives a
// reload and is shared with the paper Trade / Stats iframes. The parent frame
// broadcasts live changes; an iframe opened later simply reads the stored value.
const KEY = "algo.textScale";
const MIN = 60;
const MAX = 200;
const STEP = 5;

function clampPct(v) {
  let n = Number(v);
  if (!isFinite(n) || n <= 0) n = 100;
  n = Math.round(n);
  return Math.max(MIN, Math.min(MAX, n));
}

function readStored() {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw === null || raw === "") return 100;
    return clampPct(raw);
  } catch (_) {
    return 100;
  }
}

function apply(pct) {
  const z = clampPct(pct);
  try {
    document.documentElement.style.zoom = z / 100;
  } catch (_) {}
  return z;
}

function store(pct) {
  try {
    localStorage.setItem(KEY, String(pct));
  } catch (_) {}
}

// Push the new size into the paper Trade / Stats iframes so they scale with
// the main window instead of keeping the old size until reloaded.
function broadcast(pct) {
  document.querySelectorAll("iframe").forEach((f) => {
    try {
      if (f.contentWindow) f.contentWindow.postMessage({ type: "algoTextScale", pct }, "*");
    } catch (_) {}
  });
}

/// Apply the stored text size on every page, listen for the parent's live
/// updates, and (on the main window) wire the topbar number input + mouse wheel.
export function bootTextSize() {
  apply(readStored());
  window.addEventListener("message", (e) => {
    const d = e.data || {};
    if (d && d.type === "algoTextScale") apply(d.pct);
  });
  const input = document.getElementById("textSizeInput");
  if (!input) return;
  input.value = readStored();
  const commit = (v) => {
    const z = clampPct(v);
    input.value = z;
    store(z);
    apply(z);
    broadcast(z);
  };
  input.addEventListener("input", () => commit(input.value));
  input.addEventListener("change", () => commit(input.value));
  input.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      commit(clampPct(Number(input.value) + (e.deltaY < 0 ? STEP : -STEP)));
    },
    { passive: false }
  );
}

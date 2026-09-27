// Cohesive dark theme for the non-realtime panes. Each section gets a subtle
// colour-tinted dark surface, a bright accent for its title/ribbon and
// high-contrast light text. Realtime engine rows are themed by realtime.js.
//
// Palette entry: [accent, tinted bg, edge, text].

const PALETTE = [
  ["#38bdf8", "#0f2433", "#1f4a63", "#e6eaf5"],
  ["#fbbf24", "#2a2314", "#57491f", "#e6eaf5"],
  ["#34d399", "#0f2a20", "#215340", "#e6eaf5"],
  ["#a78bfa", "#1b1830", "#3a3160", "#e6eaf5"],
  ["#f472b6", "#2a1626", "#552a48", "#e6eaf5"],
  ["#fb7185", "#2b1620", "#582936", "#e6eaf5"],
  ["#22d3ee", "#0e2530", "#1f4c5e", "#e6eaf5"],
  ["#a3e635", "#1f2a14", "#3c5228", "#e6eaf5"],
  ["#fb923c", "#2b1d12", "#573a22", "#e6eaf5"],
  ["#e879f9", "#271733", "#502c66", "#e6eaf5"],
  ["#2dd4bf", "#0e2a26", "#1f544b", "#e6eaf5"],
  ["#60a5fa", "#141d31", "#293a5a", "#e6eaf5"],
];

const UNITS = "h3, h4, .acard, .oc-all-block, .pane-box, .pane-head, " +
  ".ind-section-head, .mw-cat, .rs-chart-title, .oc-toolbar, .account-toolbar";

function paintPane(pane) {
  if (!pane || pane.id === "tab-realtime") return;
  let i = 0;
  pane.querySelectorAll(UNITS).forEach((el) => {
    const c = PALETTE[i++ % PALETTE.length];
    el.style.setProperty("--sec-accent", c[0]);
    el.style.setProperty("--sec-bg", c[1]);
    el.style.setProperty("--sec-edge", c[2]);
    el.style.setProperty("--sec-text", c[3]);
  });
}

let raf = 0;
function repaint() {
  cancelAnimationFrame(raf);
  raf = requestAnimationFrame(() => {
    document.querySelectorAll(".tab-content").forEach(paintPane);
  });
}

let booted = false;
export function bootTheme() {
  if (booted) return;
  booted = true;
  repaint();
  const obs = new MutationObserver(repaint);
  document.querySelectorAll(".tab-content").forEach((p) => {
    if (p.id !== "tab-realtime") obs.observe(p, { childList: true, subtree: true });
  });
  document.addEventListener("tabshown", repaint);
}

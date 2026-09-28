// Group the chart-tab "Indicators" menu into labelled sections and pull every
// straight-line family indicator into its own "Straight Line Indicator"
// subsection. The menu itself is built by the WASM (build_menu -> #indList) as
// a flat list, so this module regroups the existing DOM nodes in place (moving
// them keeps the click handlers the WASM attached). The id set mirrors
// STRAIGHT_LINE_IDS in realtime.js so the menu and the NIFTY-trend picker agree.

const STRAIGHT_LINE_IDS = new Set([
  "slconsensus", "ovlconsensus", "autotrend", "projline", "zzline",
  "trendmaster", "panemaster", "srema", "supline", "resline",
  "pitchfork", "fibfan", "gannfan", "supplydemand", "wavefib",
  "pastruct", "vl",
]);

const STRAIGHT_LABEL = "Straight Line Indicator";

// True once every item already sits inside one of our .ind-group wrappers. A
// fresh WASM rebuild drops those wrappers, so this also detects a rebuild.
function isGrouped(list) {
  if (!list.querySelector(".ind-group")) return false;
  return Array.from(list.querySelectorAll(".ind-item")).every(
    (el) => el.parentElement && el.parentElement.classList.contains("ind-group")
  );
}

function regroup(list) {
  const items = Array.from(list.querySelectorAll(".ind-item"));
  if (!items.length || isGrouped(list)) return;
  // Preserve the registry order of every category, then force the straight-line
  // family last so it reads as one dedicated subsection.
  const sections = new Map();
  for (const el of items) {
    const id = el.getAttribute("data-id") || "";
    const label = STRAIGHT_LINE_IDS.has(id)
      ? STRAIGHT_LABEL
      : el.getAttribute("data-cat") || "Other";
    if (!sections.has(label)) sections.set(label, []);
    sections.get(label).push(el);
  }
  const labels = Array.from(sections.keys()).filter((l) => l !== STRAIGHT_LABEL);
  if (sections.has(STRAIGHT_LABEL)) labels.push(STRAIGHT_LABEL);

  while (list.firstChild) list.removeChild(list.firstChild);
  for (const label of labels) {
    const head = document.createElement("div");
    head.className = "ind-cat";
    head.textContent = label;
    list.appendChild(head);
    const group = document.createElement("div");
    group.className = "ind-group";
    for (const el of sections.get(label)) group.appendChild(el);
    list.appendChild(group);
  }
  syncSearch(list);
}

// Hide a section heading (and its wrapper) when the WASM search filter has
// hidden every item inside it.
function syncSearch(list) {
  list.querySelectorAll(".ind-group").forEach((group) => {
    const any = group.querySelector(".ind-item:not([style*='display:none'])");
    group.style.display = any ? "" : "none";
    const head = group.previousElementSibling;
    if (head && head.classList.contains("ind-cat")) head.style.display = any ? "" : "none";
  });
}

export function bootIndGroup() {
  const list = document.getElementById("indList");
  if (!list) return;
  regroup(list);
  // The WASM can rebuild #indList (fresh innerHTML); regroup whenever it does.
  new MutationObserver(() => regroup(list)).observe(list, { childList: true });
  const search = document.getElementById("indSearch");
  if (search) {
    // Defer so the WASM's own input handler applies the filter first.
    search.addEventListener("input", () => setTimeout(() => syncSearch(list), 0));
  }
}

/* tslint:disable */
/* eslint-disable */

/**
 * Current chart kind + Renko settings as JSON, so the UI can restore its state.
 */
export function chart_kind_json(): string;

export function clear_dir_overlay(): void;

export function clear_oc_level_lines(): void;

export function clear_trade_lines(): void;

/**
 * Push-style realtime hook. The sidebar calls this on every quote update
 * (`render`) so the forming candle tracks the tape immediately instead of
 * waiting for the 1s fallback ticker. No-op when the chart's realtime toggle
 * is off or no live price is cached yet.
 */
export function live_tick(): void;

/**
 * Open the option-premium chart directly from a CE/PE security id (old app's
 * `openOptionChartBySid`), without needing the option chain to be loaded.
 * Public WASM export kept for parity with the old Auto-Experiment detail
 * button (the new UI currently has no caller, but JS may invoke it).
 */
export function open_option_chart_by_sid(sid: number, exchange_segment: string, instrument_type: string, label: string): void;

/**
 * Sidebar entry point: switch the chart to another instrument. `sec_id` is an
 * f64 so JS can pass a plain number (i64 would require a BigInt).
 */
export function select_symbol(sec_id: number, exch: string, inst_type: string, name: string): void;

/**
 * Switch the chart type (`candles` / `heikin_ashi` / `renko`) and apply the
 * Renko settings, rebuilding the display series from the raw candles. The
 * open indicators are recomputed on the new series, exactly as TradingView
 * does when the chart type changes.
 */
export function set_chart_kind(kind: string, json: string): void;

/**
 * Trend arrows / labels for the direction overlay (merged alongside the
 * indicator-owned marker arrows).
 */
export function set_dir_markers(json: string): void;

/**
 * Direction-state line pushed by the OI Trend overlay. `json` is an array of
 * `{time, value}` points. Pass an empty string / empty array to clear.
 */
export function set_dir_series(json: string, color: string, line_width: number): void;

/**
 * Replace the option-chain level lines (separate registry from trade lines so
 * the two never wipe each other; old app's `setOcLevelLines`).
 */
export function set_oc_level_lines(json: string): void;

/**
 * Enable/disable the OI Trend overlay from outside the toggle handler (the
 * old-app global `OITrend.setEnabled`).
 */
export function set_oi_trend_enabled(on: boolean): void;

/**
 * Replace the strategy/paper-trade level lines drawn on the main chart (old
 * app's `IndChart.setTradeLines`).
 */
export function set_trade_lines(json: string): void;

export function start(): void;

/**
 * Live quote ingest: the sidebar quote engine pushes its merged map here on
 * every `/ws` frame / `/api/quotes` poll so option LTPs tick and flash without
 * a full chain re-render (old app's `updateOCLive`).
 */
export function update_oc_quotes(json: string): void;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly chart_kind_json: () => [number, number];
    readonly clear_dir_overlay: () => void;
    readonly clear_oc_level_lines: () => void;
    readonly clear_trade_lines: () => void;
    readonly live_tick: () => void;
    readonly open_option_chart_by_sid: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly select_symbol: (a: number, b: number, c: number, d: number, e: number, f: number, g: number) => void;
    readonly set_chart_kind: (a: number, b: number, c: number, d: number) => void;
    readonly set_dir_markers: (a: number, b: number) => void;
    readonly set_dir_series: (a: number, b: number, c: number, d: number, e: number) => void;
    readonly set_oc_level_lines: (a: number, b: number) => void;
    readonly set_oi_trend_enabled: (a: number) => void;
    readonly set_trade_lines: (a: number, b: number) => void;
    readonly start: () => void;
    readonly update_oc_quotes: (a: number, b: number) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___js_sys_74bf28d486ca7ec2___Function_fn_wasm_bindgen_7005259d21a40caf___JsValue_____wasm_bindgen_7005259d21a40caf___sys__Undefined___js_sys_74bf28d486ca7ec2___Function_fn_wasm_bindgen_7005259d21a40caf___JsValue_____wasm_bindgen_7005259d21a40caf___sys__Undefined_______true_: (a: number, b: number, c: any, d: any) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___wasm_bindgen_7005259d21a40caf___JsValue__core_608f92abc48d28da___result__Result_____wasm_bindgen_7005259d21a40caf___JsError___true_: (a: number, b: number, c: any) => [number, number];
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___web_sys_f91a6cb96ff99210___features__gen_MouseEvent__MouseEvent______true_: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___web_sys_f91a6cb96ff99210___features__gen_MouseEvent__MouseEvent______true__18: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___web_sys_f91a6cb96ff99210___features__gen_MouseEvent__MouseEvent______true__19: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke___web_sys_f91a6cb96ff99210___features__gen_MouseEvent__MouseEvent______true__20: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen_7005259d21a40caf___convert__closures_____invoke_______true_: (a: number, b: number) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_destroy_closure: (a: number, b: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;

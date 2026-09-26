# algo-desktop

Dedicated native desktop shell for the Rust algo app.

Ye kisi tab/function/option ko port nahi karta. Existing `algo-server` already
poori app (saare tabs, functions, options) + REST API + WebSocket ek hi port par
serve karta hai. Ye shell bas usko ek native window me dikhata hai.

## How it works

1. `algo-server` ko ek free local port par child process ki tarah start karta hai.
2. `/api/health` green hone tak wait karta hai.
3. App ko apni native window me OS webview se kholta hai:
   - Windows: WebView2 (Chromium engine)
   - macOS: WKWebView
   - Linux: WebKitGTK
4. Window band karte hi server process bhi band ho jata hai.

Isse milta hai: zero UI porting, native-speed Rust backend, Chromium-level UI
speed, aur browser chrome ka jhanjhat nahi.

## Run (development)

`algo-server` pehle build karo, phir shell:

```bash
cargo build -p algo-server
cargo run --manifest-path desktop/Cargo.toml
```

Shell apne paas (`target/debug/`) me `algo-server` dhoondh leta hai.

## Environment variables

- `ALGO_SERVER_BIN` - specific `algo-server` binary ka path
- `ALGO_APP_URL` - kisi already-running server se attach karo (spawn skip)
- `ALGODHAN_DATA_DIR` - durable data folder for engine state (strategies/settings)
- `ALGO_DEVTOOLS=1` - reserved for devtools (future)

## Package (build a distributable folder)

Linux/macOS:

```bash
scripts/build-desktop.sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-desktop.ps1
```

Output: `dist/` containing `algo-desktop`, `algo-server`, `static/`, the update
asset `algo-desktop-<os>-<arch>.zip`, `SHA256SUMS`, aur ek single-file portable
launcher `algo-desktop-<os>-<arch>-portable`.

## Portable single-file launcher

`dist/algo-desktop-<os>-<arch>-portable` (Windows par `.exe`) ek hi file hai
jisme shell + `algo-server` + `static/` embedded hote hain.

- Double-click karo, installer ya admin rights ki zaroorat nahi.
- Pehli baar chalane par ye apne andar se runtime `<data_dir>/runtime/<hash>/`
  me extract karta hai (content-hash cache, dobara extract nahi hota).
- `data_dir` default `data/`, `ALGODHAN_DATA_DIR` se badal sakte ho.
- Portable folder (`algo-desktop` + `algo-server` + `static/`) primary supported
  form hai; single-file variant convenience ke liye hai. Note: self-extracting
  single file antivirus / Windows SmartScreen me kabhi flag ho sakti hai -
  unhe "More info -> Run anyway" karna pad sakta hai.

Manual build:

```bash
desktop/target/release/algo-desktop --make-launcher \
  desktop/target/release/algo-desktop \
  target/release/algo-server \
  crates/server/static \
  dist/algo-desktop-linux-x86_64-portable
```

## Manual updater (GitHub Releases)

App ke andar ek floating "Update" panel inject hota hai (app ke apne HTML/JS ko
chhede bina). Usme GitHub repo URL daalo, Save karo, aur "Update App" dabao.
Shell GitHub Releases se latest version check karke download, verify aur
apply karta hai, phir app restart ho jata hai.

Updater ko chahiye:

1. Repo me ek Release ho, tag `vX.Y.Z` format me (jaise `v0.1.1`).
2. Us release me platform asset ho, exact naam se:
   - `algo-desktop-windows-x86_64.zip`
   - `algo-desktop-linux-x86_64.zip`
   - `algo-desktop-macos-aarch64.zip` (arch ke hisaab se)
3. Saath me `SHA256SUMS` asset ho (build script khud bana deta hai). App sha256
   verify karta hai; GitHub asset `digest` ho to wo bhi use hota hai.

Asset ke andar `algo-desktop`, `algo-server` aur `static/` root par hone chahiye
- build script yahi banata hai.

Naya release kaise banao:

```bash
# version bump: desktop/Cargo.toml me version badlo, phir:
scripts/build-desktop.sh
# dist/algo-desktop-<os>-<arch>.zip + dist/SHA256SUMS ko GitHub Release me upload karo
```

Repo private ho to: manual update ke waqt repo ko thodi der public karo,
button dabao, phir wapas private. Automatic update nahi chahiye. Behtar option:
sirf binaries/manifest wala chhota public "distribution repo" rakho.

Security: release me checksum/SHA256SUMS rakho, warna app verify skip karega.
Dhyan rahe ki bina verification ke fake update install ho sakta hai.

## Platform notes

- Windows 10/11 par WebView2 runtime already hota hai; warna Microsoft ka
  WebView2 Evergreen Runtime install karna hoga. Best speed-to-size.
- macOS par WKWebView built-in hai.
- Linux par OS webview (WebKitGTK) thoda slow ho sakta hai. Max consistent speed
  ke liye embedded Chromium (CEF) shell use karo.

## Debug / test modes

```bash
# updater ke filesystem logic ka local test (network-free)
algo-desktop --selftest

# kisi repo ka latest release check (API + tag + asset selection)
algo-desktop --check owner/repo

# download -> verify -> extract -> copy pipeline, kisi URL ke against
algo-desktop --test-update http://host:port algo-desktop-linux-x86_64.zip /tmp/target

# helper mode (app ise khud call karta hai update apply ke liye)
algo-desktop --apply <staging> <target> <launch> <pid>

# single-file portable launcher banao
algo-desktop --make-launcher <shell_exe> <server_exe> <static_dir> <out_exe>
```

Server resolution order: `ALGO_SERVER_BIN` env -> embedded payload (agar launcher
hai) -> sibling `algo-server` -> dev `target/` build.

`ALGO_UI_SELFTEST=1` ke saath chalao to shell injected updater panel aur IPC
round-trip verify karke exit kar jata hai.

Installed app ke naye options/settings ke liye signed config pack (templates /
strategies / selection already data-driven hain) bhi use kar sakte ho - uske
liye binary update ki zaroorat nahi.


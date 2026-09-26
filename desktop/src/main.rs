// algo-desktop: dedicated native desktop shell for the Rust algo app, with a
// manual GitHub-Releases updater.
//
// This is NOT a rewrite or a port. The existing `algo-server` already serves the
// entire application (all tabs, functions and options) plus its REST API and
// WebSocket on a single port. This shell:
//   1. starts that server binary as a child process on a free local port,
//   2. waits until it is healthy, then
//   3. shows the app in its own native window using the OS webview
//      (WebView2 / Chromium on Windows, WKWebView on macOS, WebKitGTK on Linux).
//   4. injects a small "Update App" panel (GitHub repo URL + button) that talks
//      to the shell over IPC; the shell downloads the release and swaps itself.
//
// Result: zero UI porting, native-speed Rust backend, Chromium-level UI speed,
// no browser chrome, and a manual updater driven from GitHub Releases.

use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

mod payload;
mod ui;
mod updater;

const ENV_APP_URL: &str = "ALGO_APP_URL";
const ENV_SERVER_BIN: &str = "ALGO_SERVER_BIN";
const ENV_DATA_DIR: &str = "ALGODHAN_DATA_DIR";

const DEFAULT_TITLE: &str = "Dhan Algo Trading System";

enum UserEvent {
    Status(serde_json::Value),
    ProbeUi,
    ProbeStatus,
    Quit,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Build a single-file launcher: shell exe + embedded server/static payload.
    if args.get(1).map(String::as_str) == Some("--make-launcher") {
        let shell = args.get(2).map(PathBuf::from);
        let server = args.get(3).map(PathBuf::from);
        let static_dir = args.get(4).map(PathBuf::from);
        let out = args.get(5).map(PathBuf::from);
        match (shell, server, static_dir, out) {
            (Some(s), Some(srv), Some(st), Some(o)) => match payload::make_launcher(&s, &srv, &st, &o) {
                Ok(()) => {
                    println!("launcher -> {}", o.display());
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("make-launcher failed: {e}");
                    std::process::exit(1);
                }
            },
            _ => {
                eprintln!("usage: --make-launcher <shell_exe> <server_exe> <static_dir> <out_exe>");
                std::process::exit(2);
            }
        }
    }

    // Helper mode: swap files after the main app exits, then relaunch.
    if args.get(1).map(String::as_str) == Some("--apply") {
        let staging = args.get(2).map(PathBuf::from);
        let target = args.get(3).map(PathBuf::from);
        let launch = args.get(4).map(PathBuf::from);
        let pid = args.get(5).map(String::as_str).unwrap_or("0");
        match (staging, target, launch) {
            (Some(s), Some(t), Some(l)) => updater::run_apply_helper(&s, &t, &l, pid),
            _ => {
                eprintln!("usage: --apply <staging> <target> <launch> <pid>");
                std::process::exit(2);
            }
        }
    }

    // Self-test of the updater's risky pieces (hashing + file swap).
    if args.get(1).map(String::as_str) == Some("--selftest") {
        selftest();
    }

    // Debug mode: exercise download -> verify -> extract -> copy against a URL.
    if args.get(1).map(String::as_str) == Some("--test-update") {
        let base = args.get(2).cloned().unwrap_or_default();
        let asset = args
            .get(3)
            .cloned()
            .unwrap_or_else(updater::platform_asset_name);
        let target = args.get(4).cloned().unwrap_or_default();
        let release = updater::Release {
            tag: "v0.0.0-test".into(),
            version: semver::Version::new(0, 0, 0),
            asset_name: asset.clone(),
            asset_url: format!("{base}/{asset}"),
            digest: None,
            sums_url: Some(format!("{base}/SHA256SUMS")),
        };
        let data_dir = std::env::temp_dir().join("algodhan-test-update");
        match updater::download_and_stage(&data_dir, &release, &|m| println!("{m}")) {
            Ok(staging) => {
                if target.is_empty() {
                    println!("staged at {}", staging.display());
                    std::process::exit(0);
                }
                match updater::copy_tree(&staging, std::path::Path::new(&target)) {
                    Ok(()) => {
                        println!("applied -> {target}");
                        std::process::exit(0);
                    }
                    Err(e) => {
                        eprintln!("copy failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("test-update failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // Debug/CLI mode: print the latest release info for a repo.
    if args.get(1).map(String::as_str) == Some("--check") {
        let repo = args.get(2).cloned().unwrap_or_default();
        match updater::parse_repo(&repo) {
            Some(r) => match updater::fetch_latest(&r) {
                Ok(rel) => {
                    println!(
                        "repo={r}\ntag={}\nversion={}\nasset={}\ndigest={:?}",
                        rel.tag, rel.version, rel.asset_name, rel.digest
                    );
                    std::process::exit(0);
                }
                Err(e) => {
                    eprintln!("check failed: {e}");
                    std::process::exit(1);
                }
            },
            None => {
                eprintln!("invalid repo: {repo:?}");
                std::process::exit(2);
            }
        }
    }

    if let Err(err) = run() {
        eprintln!("[algo-desktop] startup failed: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = resolve_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);

    let attach_url = std::env::var(ENV_APP_URL).ok().filter(|s| !s.trim().is_empty());

    let (url, mut server): (String, Option<Child>) = match attach_url {
        Some(u) => {
            println!("[algo-desktop] attaching to {u}");
            (u, None)
        }
        None => {
            let bin = resolve_server_bin().ok_or(
                "could not find the `algo-server` binary. Put it next to algo-desktop, \
                 or set ALGO_SERVER_BIN, or set ALGO_APP_URL to attach.",
            )?;
            let port = free_port()?;
            println!("[algo-desktop] starting {} on 127.0.0.1:{port}", bin.display());
            let child = spawn_server(&bin, port)?;
            if !wait_healthy(port, Duration::from_secs(25)) {
                kill(&mut Some(child));
                return Err(
                    format!("algo-server did not become healthy on 127.0.0.1:{port} within 25s")
                        .into(),
                );
            }
            println!("[algo-desktop] server healthy, opening window");
            (format!("http://127.0.0.1:{port}/"), Some(child))
        }
    };

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy: EventLoopProxy<UserEvent> = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title(DEFAULT_TITLE)
        .with_inner_size(tao::dpi::LogicalSize::new(1440.0, 900.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(1024.0, 680.0))
        .build(&event_loop)?;

    let ipc_data_dir = data_dir.clone();
    let ipc_proxy = proxy.clone();
    let ui_probe = std::env::var("ALGO_UI_SELFTEST").is_ok();
    let probe_proxy = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_url(url)
        .with_initialization_script(ui::injected_script())
        .with_ipc_handler(move |req: wry::http::Request<String>| {
            handle_ipc(req.body(), &ipc_data_dir, &ipc_proxy);
        })
        .with_on_page_load_handler(move |event, _url| {
            if ui_probe && matches!(event, wry::PageLoadEvent::Finished) {
                let _ = probe_proxy.send_event(UserEvent::ProbeUi);
            }
        });

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let webview = builder.build(&window)?;

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window
            .default_vbox()
            .ok_or("this tao backend does not expose a GTK box for the webview")?;
        builder.build_gtk(vbox)?
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Status(payload)) => {
                let js = format!(
                    "window.__algoUpdater && window.__algoUpdater.status({})",
                    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
                );
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(UserEvent::Quit) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::ProbeUi) => {
                let _ = webview.evaluate_script_with_callback(
                    "JSON.stringify({ran: window.__auRan, err: window.__auErr, \
                     hasUpdater: typeof window.__algoUpdater, \
                     hasIpc: !!(window.ipc && window.ipc.postMessage), \
                     btn: !!document.getElementById('__auBtn'), \
                     panel: !!document.getElementById('__auPanel'), \
                     body: document.body ? document.body.children.length : -1})",
                    |res| println!("UI_PROBE={res}"),
                );
                let _ = webview.evaluate_script(
                    "try{window.ipc.postMessage(JSON.stringify({cmd:'ready'}));}catch(e){}",
                );
                let p = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(900));
                    let _ = p.send_event(UserEvent::ProbeStatus);
                });
            }
            Event::UserEvent(UserEvent::ProbeStatus) => {
                let p = proxy.clone();
                let _ = webview.evaluate_script_with_callback(
                    "(document.getElementById('__auStatus')||{}).textContent || ''",
                    move |res| {
                        println!("UI_IPC_STATUS={res}");
                        let _ = p.send_event(UserEvent::Quit);
                    },
                );
            }
            Event::LoopDestroyed => {
                kill(&mut server);
            }
            _ => {}
        }
    });
}

/// Handle messages posted by the injected updater panel.
fn handle_ipc(body: &str, data_dir: &std::path::Path, proxy: &EventLoopProxy<UserEvent>) {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return,
    };
    let cmd = value.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let current = updater::current_version().to_string();

    match cmd {
        "ready" => {
            let cfg = updater::load_config(data_dir);
            let _ = proxy.send_event(UserEvent::Status(json!({
                "state": "info",
                "message": "Repo URL save karke 'Update App' dabao.",
                "repo": cfg.repo,
                "current": current,
            })));
        }
        "save_repo" => {
            let raw = value.get("repo").and_then(|v| v.as_str()).unwrap_or("");
            match updater::parse_repo(raw) {
                Some(repo) => {
                    let cfg = updater::UpdateConfig { repo: repo.clone() };
                    let msg = match updater::save_config(data_dir, &cfg) {
                        Ok(()) => format!("Saved: {repo}"),
                        Err(e) => format!("save failed: {e}"),
                    };
                    let _ = proxy.send_event(UserEvent::Status(json!({
                        "state": "info",
                        "message": msg,
                        "repo": repo,
                        "current": current,
                    })));
                }
                None => {
                    let _ = proxy.send_event(UserEvent::Status(json!({
                        "state": "error",
                        "message": "Repo URL galat hai. Format: owner/repo",
                        "current": current,
                    })));
                }
            }
        }
        "update" => {
            let typed = value.get("repo").and_then(|v| v.as_str()).unwrap_or("");
            let repo = updater::parse_repo(typed).or_else(|| {
                let cfg = updater::load_config(data_dir);
                updater::parse_repo(&cfg.repo)
            });
            let Some(repo) = repo else {
                let _ = proxy.send_event(UserEvent::Status(json!({
                    "state": "error",
                    "message": "Pehle GitHub repo URL save karo.",
                    "current": current,
                })));
                return;
            };
            let _ = updater::save_config(data_dir, &updater::UpdateConfig { repo: repo.clone() });

            let proxy = proxy.clone();
            let data_dir = data_dir.to_path_buf();
            std::thread::spawn(move || run_update(proxy, repo, data_dir));
        }
        _ => {}
    }
}

fn run_update(proxy: EventLoopProxy<UserEvent>, repo: String, data_dir: PathBuf) {
    let send = |state: &str, message: String, latest: Option<String>| {
        let _ = proxy.send_event(UserEvent::Status(json!({
            "state": state,
            "message": message,
            "repo": repo,
            "current": updater::current_version().to_string(),
            "latest": latest,
        })));
    };

    send("busy", "GitHub se latest release check ho rahi hai...".into(), None);

    let release = match updater::fetch_latest(&repo) {
        Ok(r) => r,
        Err(e) => {
            send("error", e, None);
            return;
        }
    };

    let current = updater::current_version();
    if release.version <= current {
        send(
            "info",
            format!("Already up to date (v{current})."),
            Some(release.tag.clone()),
        );
        return;
    }

    send(
        "busy",
        format!("Naya version mila: {} - download shuru...", release.tag),
        Some(release.tag.clone()),
    );

    let progress = |msg: &str| {
        let _ = proxy.send_event(UserEvent::Status(json!({
            "state": "busy",
            "message": msg,
            "repo": repo,
            "current": updater::current_version().to_string(),
            "latest": release.tag,
        })));
    };

    let staging = match updater::download_and_stage(&data_dir, &release, &progress) {
        Ok(s) => s,
        Err(e) => {
            send("error", format!("Update failed: {e}"), Some(release.tag));
            return;
        }
    };

    let install_dir = match std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        Some(d) => d,
        None => {
            send("error", "install dir resolve nahi hua".into(), Some(release.tag));
            return;
        }
    };

    send(
        "busy",
        "Apply ho raha hai - app restart hoga...".into(),
        Some(release.tag.clone()),
    );

    match updater::apply_and_restart(&staging, &install_dir) {
        Ok(()) => {
            let _ = proxy.send_event(UserEvent::Quit);
        }
        Err(e) => {
            send("error", format!("Apply failed: {e}"), Some(release.tag));
        }
    }
}

fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn resolve_server_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(ENV_SERVER_BIN) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }
    // Single-file launcher: extract the embedded runtime if present.
    if payload::is_launcher() {
        if let Some(server) = payload::extract_embedded_server(&resolve_data_dir()) {
            println!("[algo-desktop] using embedded runtime at {}", server.display());
            return Some(server);
        }
    }
    let name = if cfg!(windows) { "algo-server.exe" } else { "algo-server" };
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let candidates = [
        dir.join(name),
        dir.join("bin").join(name),
        dir.join("binaries").join(name),
        PathBuf::from(name),
    ];
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    // Development fallback: look for `target/{debug,release}/algo-server` in the
    // ancestor directories (covers `desktop` being its own cargo workspace).
    let mut cursor = Some(dir.as_path());
    while let Some(cur) = cursor {
        for profile in ["debug", "release"] {
            let candidate = cur.join("target").join(profile).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        cursor = cur.parent();
    }
    None
}

fn spawn_server(bin: &std::path::Path, port: u16) -> std::io::Result<Child> {
    let data_dir = resolve_data_dir();
    let _ = std::fs::create_dir_all(&data_dir);

    let mut cmd = Command::new(bin);
    cmd.env("PORT", port.to_string())
        .env(ENV_DATA_DIR, &data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    if let Some(dir) = bin.parent() {
        cmd.current_dir(dir);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    cmd.spawn()
}

fn resolve_data_dir() -> PathBuf {
    if let Ok(p) = std::env::var(ENV_DATA_DIR) {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    #[cfg(windows)]
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("DhanAlgo").join("data");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".dhanalgo").join("data");
    }
    PathBuf::from("data")
}

fn wait_healthy(port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if health_ok(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

fn health_ok(port: u16) -> bool {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(800)));
    let req = format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
        }
        _ => false,
    }
}

fn kill(server: &mut Option<Child>) {
    if let Some(child) = server.as_mut() {
        let _ = child.kill();
        let _ = child.wait();
    }
    *server = None;
}

/// Local, network-free verification of the updater's filesystem logic.
fn selftest() {
    use std::io::Write;

    let mut failures: Vec<String> = Vec::new();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::temp_dir().join(format!("algodhan-selftest-{}-{stamp}", std::process::id()));

    let target = base.join("target");
    let staging = base.join("staging");
    std::fs::create_dir_all(target.join("static")).unwrap();
    std::fs::create_dir_all(staging.join("static")).unwrap();

    std::fs::write(target.join("a.txt"), "old").unwrap();
    std::fs::write(target.join("static").join("keep.txt"), "old").unwrap();

    std::fs::write(staging.join("a.txt"), "new").unwrap();
    std::fs::write(staging.join("static").join("keep.txt"), "new").unwrap();
    std::fs::write(staging.join("static").join("new.txt"), "brand new").unwrap();

    // copy_tree replaces existing files and adds new ones.
    if let Err(e) = updater::copy_tree(&staging, &target) {
        failures.push(format!("copy_tree failed: {e}"));
    }
    if std::fs::read_to_string(target.join("a.txt")).unwrap_or_default() != "new" {
        failures.push("a.txt not replaced".into());
    }
    if std::fs::read_to_string(target.join("static").join("keep.txt")).unwrap_or_default() != "new" {
        failures.push("static/keep.txt not replaced".into());
    }
    if !target.join("static").join("new.txt").is_file() {
        failures.push("static/new.txt not added".into());
    }

    // sha256 of "abc".
    let hash_file = base.join("hash.txt");
    let mut f = std::fs::File::create(&hash_file).unwrap();
    f.write_all(b"abc").unwrap();
    drop(f);
    let got = updater::sha256_file(&hash_file).unwrap_or_default();
    let want = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    if got != want {
        failures.push(format!("sha256 mismatch: {got} != {want}"));
    }

    // repo URL parsing.
    for (input, expect) in [
        ("https://github.com/owner/repo", "owner/repo"),
        ("https://github.com/owner/repo.git", "owner/repo"),
        ("https://github.com/owner/repo/", "owner/repo"),
        ("owner/repo", "owner/repo"),
    ] {
        if updater::parse_repo(input).as_deref() != Some(expect) {
            failures.push(format!("parse_repo({input:?}) != {expect:?}"));
        }
    }
    if updater::parse_repo("nope").is_some() {
        failures.push("parse_repo should reject single token".into());
    }

    // version ordering.
    let a = semver::Version::parse("1.2.3").unwrap();
    let b = semver::Version::parse("1.2.4").unwrap();
    if !(a < b) {
        failures.push("semver ordering wrong".into());
    }

    if failures.is_empty() {
        println!("selftest: PASS (copy_tree, sha256, parse_repo, semver)");
        std::process::exit(0);
    }
    for f in &failures {
        eprintln!("selftest FAIL: {f}");
    }
    std::process::exit(1);
}

// Manual GitHub-Releases based updater for the desktop shell.
//
// Flow (triggered by the "Update App" button injected into the UI):
//   parse repo -> GET /repos/{repo}/releases/latest -> compare versions
//   -> pick the platform asset -> download -> verify sha256 -> unzip to staging
//   -> copy this exe to a temp helper -> run `--apply` -> helper waits for this
//      process to exit, swaps the files in the install dir, relaunches the app.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const USER_AGENT: &str = "algo-desktop-updater";
const API: &str = "https://api.github.com";

/// Distribution repo the updater uses until the operator saves another one.
/// GitHub Releases on this repo must publish `algo-desktop-<os>-<arch>.zip`
/// assets (+ `SHA256SUMS`) - see `.github/workflows/release.yml`.
pub const DEFAULT_REPO: &str =
    "saratupadhyayeditor-lang/20_backup_Fixed_NiftyTrend_StrategyTradeExecutionOnOppositeSide";

#[derive(Default, Serialize, Deserialize, Clone)]
pub struct UpdateConfig {
    #[serde(default)]
    pub repo: String,
}

impl UpdateConfig {
    /// The configured repo, or the baked-in distribution repo when none is set.
    pub fn effective_repo(&self) -> String {
        if self.repo.trim().is_empty() {
            DEFAULT_REPO.to_string()
        } else {
            self.repo.clone()
        }
    }
}

pub struct Release {
    pub tag: String,
    pub version: semver::Version,
    pub asset_name: String,
    pub asset_url: String,
    pub digest: Option<String>,
    pub sums_url: Option<String>,
}

pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| semver::Version::new(0, 0, 0))
}

pub fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update_config.json")
}

pub fn load_config(data_dir: &Path) -> UpdateConfig {
    std::fs::read_to_string(config_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .map(|cfg: UpdateConfig| UpdateConfig {
            repo: cfg.effective_repo(),
        })
        .unwrap_or_else(|| UpdateConfig {
            repo: DEFAULT_REPO.to_string(),
        })
}

pub fn save_config(data_dir: &Path, cfg: &UpdateConfig) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(cfg).unwrap_or_default();
    std::fs::write(config_path(data_dir), bytes)
}

/// One persisted update attempt, shown in the updater tab's history list.
#[derive(Default, Serialize, Deserialize, Clone)]
pub struct HistoryEntry {
    pub time_ms: u64,
    pub from: String,
    pub to: String,
    /// ok | error | info
    pub status: String,
    pub message: String,
}

const HISTORY_LIMIT: usize = 60;

pub fn history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("update_history.json")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn load_history(data_dir: &Path) -> Vec<HistoryEntry> {
    std::fs::read_to_string(history_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_history(data_dir: &Path, list: &[HistoryEntry]) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let bytes = serde_json::to_vec_pretty(list).unwrap_or_default();
    std::fs::write(history_path(data_dir), bytes)
}

/// Prepend a new entry (newest first) and trim the list.
pub fn append_history(data_dir: &Path, entry: HistoryEntry) {
    let mut list = load_history(data_dir);
    list.insert(0, entry);
    list.truncate(HISTORY_LIMIT);
    let _ = save_history(data_dir, &list);
}

/// Accepts a full GitHub URL, `owner/repo`, or a URL with `.git` / trailing slash.
pub fn parse_repo(input: &str) -> Option<String> {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix("https://") {
        s = rest.to_string();
    } else if let Some(rest) = s.strip_prefix("http://") {
        s = rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("github.com/") {
        s = rest.to_string();
    }
    s = s.trim_end_matches('/').to_string();
    if let Some(rest) = s.strip_suffix(".git") {
        s = rest.to_string();
    }
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        Some(format!("{}/{}", parts[0], parts[1]))
    } else {
        None
    }
}

/// Platform asset name convention: algo-desktop-<os>-<arch>.zip
pub fn platform_asset_name() -> String {
    let os = match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        "linux" => "linux",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => other,
    };
    format!("algo-desktop-{os}-{arch}.zip")
}

fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(30))
        .call();
    match resp {
        Ok(r) => r
            .into_string()
            .map_err(|e| format!("response read failed: {e}"))
            .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("invalid JSON: {e}"))),
        Err(ureq::Error::Status(code, _)) => {
            if code == 404 {
                Err("repo/release not found (404) - repo private hai ya URL galat".to_string())
            } else if code == 403 {
                Err("GitHub ne request block ki (403) - rate limit ya access issue".to_string())
            } else {
                Err(format!("GitHub API error (HTTP {code})"))
            }
        }
        Err(e) => Err(format!("network error: {e}")),
    }
}

pub fn fetch_latest(repo: &str) -> Result<Release, String> {
    let url = format!("{API}/repos/{repo}/releases/latest");
    let json = get_json(&url)?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("release me tag_name nahi mila")?
        .to_string();
    let version = semver::Version::parse(tag.trim_start_matches('v'))
        .map_err(|e| format!("tag '{tag}' semver nahi hai: {e}"))?;

    let want = platform_asset_name();
    let assets = json.get("assets").and_then(|v| v.as_array()).cloned().unwrap_or_default();

    let mut asset_name = String::new();
    let mut asset_url = String::new();
    let mut digest = None;
    let mut sums_url = None;

    for a in &assets {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let dl = a.get("browser_download_url").and_then(|v| v.as_str()).unwrap_or("");
        if name == want {
            asset_name = name.to_string();
            asset_url = dl.to_string();
            digest = a.get("digest").and_then(|v| v.as_str()).map(|s| s.to_string());
        }
        if name == "SHA256SUMS" || name == "SHA256SUMS.txt" {
            sums_url = Some(dl.to_string());
        }
    }

    if asset_url.is_empty() {
        return Err(format!(
            "is release me '{want}' asset nahi hai (platform ke liye build upload karo)"
        ));
    }

    Ok(Release {
        tag,
        version,
        asset_name,
        asset_url,
        digest,
        sums_url,
    })
}

pub(crate) fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let resp = ureq::get(url)
        .set("User-Agent", USER_AGENT)
        .timeout(Duration::from_secs(600))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Resolve the expected sha256 for the asset: prefer the API `digest`, else a
/// `SHA256SUMS` asset published alongside the release.
fn expected_sha256(release: &Release, dest: &Path) -> Result<Option<String>, String> {
    if let Some(d) = &release.digest {
        let hex = d.strip_prefix("sha256:").unwrap_or(d);
        return Ok(Some(hex.to_lowercase()));
    }
    if let Some(sums_url) = &release.sums_url {
        let body = ureq::get(sums_url)
            .set("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| format!("SHA256SUMS download failed: {e}"))?
            .into_string()
            .map_err(|e| e.to_string())?;
        for line in body.lines() {
            let mut it = line.split_whitespace();
            if let (Some(hash), Some(name)) = (it.next(), it.next()) {
                let name = name.trim_start_matches('*');
                if name == release.asset_name {
                    return Ok(Some(hash.to_lowercase()));
                }
            }
        }
    }
    let _ = dest;
    Ok(None)
}

/// Download + verify + extract. Returns the staging directory.
pub fn download_and_stage(
    data_dir: &Path,
    release: &Release,
    progress: &dyn Fn(&str),
) -> Result<PathBuf, String> {
    let updates = data_dir.join("updates");
    std::fs::create_dir_all(&updates).map_err(|e| e.to_string())?;
    let archive = updates.join(&release.asset_name);

    progress("download shuru...");
    download(&release.asset_url, &archive)?;
    progress("download poora, verify ho raha hai...");

    let actual = sha256_file(&archive)?;
    if let Some(expected) = expected_sha256(release, &archive)? {
        if actual.to_lowercase() != expected {
            return Err(format!("sha256 mismatch! mila {actual}, expected {expected}"));
        }
    } else {
        progress("warning: koi checksum publish nahi hua, verification skip");
    }

    let staging = updates.join(format!("staging-{}", now_stamp()));
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    extract_zip(&archive, &staging)?;
    progress("extract ho gaya");
    Ok(staging)
}

pub(crate) fn extract_zip(archive: &Path, dest: &Path) -> Result<(), String> {
    let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("zip open failed: {e}"))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = entry.enclosed_name() else {
            return Err("zip me unsafe path mila".to_string());
        };
        let out = dest.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut f = std::fs::File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut f).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    Ok(())
}

fn now_stamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Copy the staging tree over the install directory, retrying while the old app
/// releases its file locks (Windows especially).
pub(crate) fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_tree(&entry.path(), &to)?;
        } else {
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// Spawn the detached helper that will swap the files after this process exits.
pub fn apply_and_restart(staging: &Path, install_dir: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let ext = if cfg!(windows) { "exe" } else { "" };
    let helper = std::env::temp_dir().join(format!("algodhan-updater.{ext}"));
    std::fs::copy(&exe, &helper).map_err(|e| format!("helper copy failed: {e}"))?;

    let pid = std::process::id().to_string();
    let mut cmd = std::process::Command::new(&helper);
    cmd.arg("--apply")
        .arg(staging)
        .arg(install_dir)
        .arg(&exe)
        .arg(&pid)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map_err(|e| format!("helper start failed: {e}"))?;
    Ok(())
}

/// Runs inside the helper process (`--apply`). Never returns normally.
pub fn run_apply_helper(staging: &Path, target: &Path, launch: &Path, _pid: &str) -> ! {
    // Give the main app a moment to exit and release the port/files.
    std::thread::sleep(Duration::from_millis(1500));
    let mut last = String::new();
    for _ in 0..120 {
        match copy_tree(staging, target) {
            Ok(()) => {
                let _ = std::process::Command::new(launch).spawn();
                std::process::exit(0);
            }
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    eprintln!("[updater] apply failed after retries: {last}");
    std::process::exit(1);
}

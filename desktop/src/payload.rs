// Single-file launcher support.
//
// A launcher is the normal shell executable with a zip payload (server binary +
// static UI) appended, plus a 16-byte footer: 8-byte magic + little-endian u64
// payload length. On start, if no sibling `algo-server` exists, the shell reads
// its own tail, extracts the payload into the per-user data dir (cached by
// content hash) and runs the server from there.
//
// Double-click the single file: it self-extracts once and opens the app window.
// No installer, no admin rights.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"ALGOPAY1";
const FOOTER_LEN: u64 = 16;

fn server_name() -> &'static str {
    if cfg!(windows) {
        "algo-server.exe"
    } else {
        "algo-server"
    }
}

/// Read the appended payload, if any. Returns the raw zip bytes.
fn read_payload(exe: &Path) -> Option<Vec<u8>> {
    let mut file = std::fs::File::open(exe).ok()?;
    let size = file.metadata().ok()?.len();
    if size <= FOOTER_LEN {
        return None;
    }
    file.seek(SeekFrom::End(-(FOOTER_LEN as i64))).ok()?;
    let mut footer = [0u8; 16];
    file.read_exact(&mut footer).ok()?;
    if &footer[0..8] != MAGIC {
        return None;
    }
    let len = u64::from_le_bytes(footer[8..16].try_into().ok()?);
    if len == 0 || len + FOOTER_LEN > size {
        return None;
    }
    let start = size - FOOTER_LEN - len;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut payload = vec![0u8; len as usize];
    file.read_exact(&mut payload).ok()?;
    Some(payload)
}

/// True if this executable carries an embedded payload.
pub fn is_launcher() -> bool {
    std::env::current_exe()
        .ok()
        .map(|e| read_payload(&e).is_some())
        .unwrap_or(false)
}

/// Extract the embedded runtime (if present) and return the server binary path.
/// Cached under `<data_dir>/runtime/<content-hash>/`.
pub fn extract_embedded_server(data_dir: &Path) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let payload = read_payload(&exe)?;

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    let digest = hex::encode(hasher.finalize());
    let cache = data_dir.join("runtime").join(&digest[..16]);

    let server = cache.join(server_name());
    if server.is_file() {
        return Some(server);
    }

    std::fs::create_dir_all(&cache).ok()?;
    let tmp_zip = cache.join("payload.zip");
    std::fs::write(&tmp_zip, &payload).ok()?;
    let extracted = crate::updater::extract_zip(&tmp_zip, &cache).is_ok();
    let _ = std::fs::remove_file(&tmp_zip);
    if !extracted {
        return None;
    }

    if server.is_file() {
        Some(server)
    } else {
        None
    }
}

/// Build a single-file launcher: copy `shell_exe`, append a zip of
/// `server_exe` (as-is name) + `static_dir` (as `static/...`), then the footer.
pub fn make_launcher(
    shell_exe: &Path,
    server_exe: &Path,
    static_dir: &Path,
    out: &Path,
) -> Result<(), String> {
    let zip_path = std::env::temp_dir().join(format!("algodhan-payload-{}.zip", std::process::id()));
    build_payload_zip(server_exe, static_dir, &zip_path)?;
    let payload = std::fs::read(&zip_path).map_err(|e| e.to_string())?;

    let mut data = std::fs::read(shell_exe).map_err(|e| format!("read shell: {e}"))?;
    data.extend_from_slice(&payload);
    let len = payload.len() as u64;
    data.extend_from_slice(MAGIC);
    data.extend_from_slice(&len.to_le_bytes());

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(out, &data).map_err(|e| format!("write launcher: {e}"))?;
    let _ = std::fs::remove_file(&zip_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn build_payload_zip(server_exe: &Path, static_dir: &Path, out: &Path) -> Result<(), String> {
    let file = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipWriter::new(file);
    let server_opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    let file_opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);

    let name = server_exe
        .file_name()
        .ok_or("server path has no file name")?
        .to_string_lossy()
        .to_string();
    zip.start_file(name, server_opts).map_err(|e| e.to_string())?;
    let mut srv = std::fs::File::open(server_exe).map_err(|e| e.to_string())?;
    std::io::copy(&mut srv, &mut zip).map_err(|e| e.to_string())?;

    add_dir_to_zip(&mut zip, static_dir, "static", file_opts)?;
    zip.finish().map_err(|e| e.to_string())?;
    Ok(())
}

fn add_dir_to_zip(
    zip: &mut zip::ZipWriter<std::fs::File>,
    dir: &Path,
    prefix: &str,
    opts: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let zip_name = format!("{prefix}/{name}");
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            add_dir_to_zip(zip, &path, &zip_name, opts)?;
        } else {
            zip.start_file(zip_name, opts).map_err(|e| e.to_string())?;
            let mut f = std::fs::File::open(&path).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, zip).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

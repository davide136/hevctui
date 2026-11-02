// Aggiornamenti da GitHub Releases: controllo della versione, download del setup, avvio silenzioso.
// Il setup (Inno Setup) attende la chiusura dell'app tramite il mutex `MUTEX_NAME`, poi la rilancia.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::core::{logline, Shared};

pub const REPO: &str = "davide136/hevctui";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const MUTEX_NAME: &str = "hevctui-running";

#[derive(Clone)]
pub struct Release {
    pub version: String,
    pub url: String,
    pub size: u64,
    pub sha256: Option<String>,
    pub page: String,
}

#[derive(Clone, Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Downloading { version: String, done: u64, total: u64 },
    /// Setup avviato: la GUI deve chiudersi per lasciargli sostituire l'exe.
    Installing,
    Error(String),
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GhAsset>,
}
#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

fn parse_version(v: &str) -> [u64; 3] {
    let mut out = [0u64; 3];
    for (i, part) in v.trim().trim_start_matches(['v', 'V']).split('.').take(3).enumerate() {
        let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
        out[i] = digits.parse().unwrap_or(0);
    }
    out
}
pub fn is_newer(remote: &str, current: &str) -> bool { parse_version(remote) > parse_version(current) }

fn agent() -> Result<ureq::Agent, String> {
    let tls = native_tls::TlsConnector::new().map_err(|e| format!("TLS: {e}"))?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(Arc::new(tls))
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(60))
        .user_agent(&format!("hevctui/{CURRENT}"))
        .build())
}

/// Ultima release pubblicata; `None` se non è più recente della versione in uso.
pub fn check() -> Result<Option<Release>, String> {
    let rel = fetch_latest()?;
    Ok(if is_newer(&rel.version, CURRENT) { Some(rel) } else { None })
}

fn fetch_latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = match agent()?.get(&url).set("Accept", "application/vnd.github+json").call() {
        Ok(r) => r,
        Err(ureq::Error::Status(404, _)) => return Err("nessuna release pubblicata".into()),
        Err(ureq::Error::Status(code, _)) => return Err(format!("GitHub ha risposto {code}")),
        Err(e) => return Err(format!("connessione: {e}")),
    };
    let rel: GhRelease = serde_json::from_reader(resp.into_reader()).map_err(|e| format!("risposta non valida: {e}"))?;
    let asset = rel.assets.iter()
        .find(|a| a.name.starts_with("hevctui-setup") && a.name.ends_with(".exe"))
        .ok_or_else(|| format!("la release {} non contiene il setup", rel.tag_name))?;
    Ok(Release {
        version: rel.tag_name.trim_start_matches(['v', 'V']).to_string(),
        url: asset.browser_download_url.clone(),
        size: asset.size,
        sha256: asset.digest.as_deref().and_then(|d| d.strip_prefix("sha256:")).map(|s| s.to_lowercase()),
        page: rel.html_url,
    })
}

fn download(rel: &Release, dest: &Path, mut progress: impl FnMut(u64)) -> Result<(), String> {
    let resp = agent()?.get(&rel.url).call().map_err(|e| format!("download: {e}"))?;
    let mut reader = resp.into_reader();
    let mut out = File::create(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let (mut hasher, mut buf, mut done) = (Sha256::new(), vec![0u8; 256 * 1024], 0u64);
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download: {e}"))?;
        if n == 0 { break; }
        out.write_all(&buf[..n]).map_err(|e| format!("scrittura: {e}"))?;
        hasher.update(&buf[..n]);
        done += n as u64;
        progress(done);
    }
    drop(out);
    if done != rel.size { return Err(format!("download incompleto ({done}/{} byte)", rel.size)); }
    if let Some(expected) = &rel.sha256 {
        let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if got != *expected { return Err("checksum SHA-256 non corrisponde".into()); }
    }
    Ok(())
}

pub fn spawn_check(st: Shared) {
    st.lock().unwrap().update = UpdateState::Checking;
    thread::spawn(move || {
        let r = check();
        let mut a = st.lock().unwrap();
        a.update = match r {
            Ok(Some(rel)) => UpdateState::Available(rel),
            Ok(None) => UpdateState::UpToDate,
            Err(e) => UpdateState::Error(e),
        };
    });
}

/// Scarica il setup e lo avvia in modalità silenziosa; la GUI si chiude quando vede `Installing`.
pub fn spawn_install(st: Shared, rel: Release) {
    st.lock().unwrap().update = UpdateState::Downloading { version: rel.version.clone(), done: 0, total: rel.size };
    thread::spawn(move || {
        let dest: PathBuf = std::env::temp_dir().join(format!("hevctui-setup-{}.exe", rel.version));
        let r = download(&rel, &dest, |done| {
            if let UpdateState::Downloading { done: d, .. } = &mut st.lock().unwrap().update { *d = done; }
        }).and_then(|_| {
            Command::new(&dest).args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"]).spawn()
                .map(|_| ()).map_err(|e| format!("avvio setup: {e}"))
        });
        match r {
            Ok(()) => {
                logline(Some(&st), &format!("UPDATE avvio setup {}", rel.version));
                let mut a = st.lock().unwrap();
                a.running = false;
                a.update = UpdateState::Installing;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                logline(Some(&st), &format!("ERR-UPDATE {e}"));
                st.lock().unwrap().update = UpdateState::Error(e);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn versions() {
        assert!(is_newer("v3.1.0", "3.0.0"));
        assert!(is_newer("3.0.10", "3.0.9"));
        assert!(!is_newer("v3.0.0", "3.0.0"));
        assert!(!is_newer("2.9.9", "3.0.0"));
        assert!(is_newer("v4", "3.9.9"));
    }

    /// Rete: `cargo test -- --ignored` dopo aver pubblicato una release.
    #[test]
    #[ignore]
    fn live_latest_release_downloads_and_verifies() {
        let rel = fetch_latest().expect("fetch");
        assert!(rel.sha256.is_some(), "GitHub non ha fornito il digest dell'asset");
        let dest = std::env::temp_dir().join("hevctui-setup-livetest.exe");
        download(&rel, &dest, |_| {}).expect("download");
        let _ = std::fs::remove_file(&dest);
    }
}

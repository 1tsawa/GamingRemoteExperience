//! `status.json` dans le dossier de données : état du host en cours d'exécution (lu par
//! `invite --addr auto` et par l'interface graphique). Écrit atomiquement, supprimé à l'arrêt.
//!
//! ```json
//! { "pid": 1234, "bind": "0.0.0.0:7777", "started_ms": 1731400000000,
//!   "upnp": { "ok": true, "state": "ok", "method": "upnp", "external_addr": "198.51.100.7:7777",
//!             "lease_secs": 3600, "error": null, "warnings": [] },
//!   "ipv6": ["2a01:…"], "firewall": { "state": "missing", "detail": "…" } }
//! ```
//! `upnp.state` : `disabled` | `pending` | `ok` | `error` ; `firewall.state` : `checking` | `ok` |
//! `missing` | `blocked` | `wrong_profile` | `unknown`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub const FILE: &str = "status.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UpnpStatus {
    pub ok: bool,
    pub state: String,
    pub method: Option<String>,
    pub external_addr: Option<String>,
    pub lease_secs: Option<u64>,
    pub error: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FirewallStatus {
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HostStatus {
    pub pid: u32,
    pub bind: String,
    pub started_ms: u64,
    pub upnp: UpnpStatus,
    #[serde(default)]
    pub ipv6: Vec<String>,
    #[serde(default)]
    pub firewall: FirewallStatus,
}

impl HostStatus {
    pub fn bind_addr(&self) -> Option<SocketAddr> {
        self.bind.parse().ok()
    }
}

/// Fichier d'état partagé entre les threads du host.
#[derive(Clone)]
pub struct StatusFile {
    path: PathBuf,
    inner: Arc<Mutex<HostStatus>>,
}

impl StatusFile {
    pub fn create(dir: &Path, st: HostStatus) -> Self {
        let f = Self { path: dir.join(FILE), inner: Arc::new(Mutex::new(st)) };
        f.flush();
        f
    }

    pub fn update(&self, change: impl FnOnce(&mut HostStatus)) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut g);
        write(&self.path, &g);
    }

    fn flush(&self) {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        write(&self.path, &g);
    }

    /// Supprime le fichier s'il est bien le nôtre (même pid) : un second host qui échoue au
    /// démarrage n'efface pas l'état du premier.
    pub fn remove(&self) {
        let _g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if read(self.path.parent().unwrap_or(Path::new("."))).is_some_and(|s| s.pid != std::process::id()) {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write(path: &Path, st: &HostStatus) {
    match serde_json::to_vec_pretty(st) {
        Ok(data) => {
            if let Err(e) = grx_core::util::write_atomic(path, &data) {
                eprintln!("! écriture de {} impossible : {e}", path.display());
            }
        }
        Err(e) => eprintln!("! status.json : {e}"),
    }
}

pub fn read(dir: &Path) -> Option<HostStatus> {
    let data = std::fs::read(dir.join(FILE)).ok()?;
    serde_json::from_slice(&data).ok()
}

/// État d'un host réellement vivant (fichier présent ET processus `pid` en cours d'exécution).
pub fn read_live(dir: &Path) -> Option<HostStatus> {
    read(dir).filter(|s| pid_alive(s.pid))
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    pid != 0 && Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip_and_owner_only_removal() {
        let dir = std::env::temp_dir().join(format!("grx-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = HostStatus { pid: std::process::id(), bind: "0.0.0.0:7777".into(), started_ms: 1, ..Default::default() };
        let f = StatusFile::create(&dir, st);
        f.update(|s| {
            s.upnp = UpnpStatus { ok: true, state: "ok".into(), method: Some("upnp".into()), external_addr: Some("198.51.100.7:7777".into()), ..Default::default() }
        });
        let back = read_live(&dir).expect("notre propre pid est vivant");
        assert_eq!(back.upnp.external_addr.as_deref(), Some("198.51.100.7:7777"));
        assert_eq!(back.bind_addr(), Some("0.0.0.0:7777".parse().unwrap()));
        f.remove();
        assert!(read(&dir).is_none());

        // Le fichier d'un AUTRE processus n'est pas supprimé.
        let other = HostStatus { pid: u32::MAX - 1, ..Default::default() };
        write(&dir.join(FILE), &other);
        f.remove();
        assert!(read(&dir).is_some());
        assert!(read_live(&dir).is_none(), "pid inexistant : host mort");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dead_pid_is_not_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }
}

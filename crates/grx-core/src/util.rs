use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{Error, Result};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Dossier de données par défaut : `%APPDATA%\GamingRemoteExperience\<role>`.
pub fn default_data_dir(role: &str) -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("GamingRemoteExperience")
        .join(role)
}

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Écriture atomique (fichier temporaire unique + renommage) pour ne jamais laisser un fichier
/// tronqué. Le nom temporaire contient le pid et un compteur : deux processus (ou threads) qui
/// écrivent en même temps ne se marchent pas dessus. Le temporaire est supprimé en cas d'échec,
/// et un éventuel ancien `<nom>.tmp` résiduel (ancien format) est nettoyé.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_file_name(format!("{name}.{}.{n}.tmp", std::process::id()));
    let res = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, path));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    let _ = std::fs::remove_file(path.with_extension("tmp"));
    Ok(())
}

/// Verrou de fichier consultatif (`LockFileEx` sous Windows) sur `<path>.lock`, libéré au `Drop`.
/// Sérialise les cycles lecture-modification-écriture entre processus et entre threads.
pub struct FileLock(#[allow(dead_code)] File);

impl FileLock {
    pub fn acquire(path: &Path, exclusive: bool) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let lock_path = path.with_file_name(format!("{name}.lock"));
        let f = std::fs::OpenOptions::new().create(true).read(true).write(true).truncate(false).open(&lock_path)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = if exclusive { f.try_lock() } else { f.try_lock_shared() };
            match r {
                Ok(()) => return Ok(FileLock(f)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Err(Error::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "verrou de la liste de confiance occupé",
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }
}

/// Erreurs de `recv`/`recv_from` UDP qui ne doivent JAMAIS arrêter une boucle réseau : un pair
/// distant (ou un simple paquet trop gros ou un ICMP « port unreachable » d'un envoi précédent)
/// ne doit pas pouvoir faire tomber le processus. Sous Windows : `WSAEMSGSIZE` (10040) quand le
/// datagramme dépasse le tampon, `WSAECONNRESET` (10054), etc.
pub fn is_transient_recv_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    if matches!(
        e.kind(),
        WouldBlock | TimedOut | Interrupted | ConnectionReset | ConnectionAborted | ConnectionRefused
    ) {
        return true;
    }
    // WSAEINTR, WSAEMSGSIZE, WSAENETDOWN.., WSAENETUNREACH, WSAENETRESET, WSAECONNABORTED,
    // WSAECONNRESET, WSAEHOSTUNREACH, WSAECONNREFUSED/WSAETIMEDOUT ; EMSGSIZE sous Unix.
    matches!(
        e.raw_os_error(),
        Some(10004 | 10040 | 10051 | 10052 | 10053 | 10054 | 10060 | 10061 | 10065 | 90)
    )
}

pub fn b32_encode(bytes: &[u8]) -> String {
    data_encoding::BASE32_NOPAD.encode(bytes).to_lowercase()
}

pub fn b32_decode(s: &str) -> Option<Vec<u8>> {
    data_encoding::BASE32_NOPAD
        .decode(s.to_uppercase().as_bytes())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgsize_and_reset_are_transient() {
        assert!(is_transient_recv_error(&std::io::Error::from_raw_os_error(10040)));
        assert!(is_transient_recv_error(&std::io::Error::from_raw_os_error(10054)));
        assert!(is_transient_recv_error(&std::io::Error::from(std::io::ErrorKind::ConnectionReset)));
        assert!(!is_transient_recv_error(&std::io::Error::from_raw_os_error(10038))); // WSAENOTSOCK
        assert!(!is_transient_recv_error(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
    }

    #[test]
    fn write_atomic_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("grx-util-{}", std::process::id()));
        let p = dir.join("x.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.tmp"), b"vieux").unwrap(); // résidu ancien format
        write_atomic(&p, b"un").unwrap();
        write_atomic(&p, b"deux").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"deux");
        let names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["x.json".to_string()], "aucun .tmp résiduel : {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

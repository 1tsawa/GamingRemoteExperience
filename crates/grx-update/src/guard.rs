//! Garde-fous autour de l'installation : ne jamais couper une partie en cours.
//!
//! - [`busy`] : processus `grx-host` / `grx-client` lancés depuis le dossier d'installation
//!   (un host qui tourne peut avoir un joueur connecté, un client est une session en cours).
//!   Seuls les exécutables de CE dossier comptent : une copie de développement ailleurs n'est
//!   pas concernée par l'installateur.
//! - [`stop_in_dir`] : arrêt propre (Ctrl+C, comme la GUI) puis forcé après délai ; utilisé par
//!   l'installateur (`grx-gui --stop-grx-processes`) avant mise à jour / désinstallation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::install::is_within;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrxProcess {
    pub pid: u32,
    /// `grx-host`, `grx-client` ou `grx-gui`
    pub name: String,
    pub exe: PathBuf,
}

const WATCHED: [&str; 3] = ["grx-host", "grx-client", "grx-gui"];

fn stem_of(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

/// Processus grx-* dont l'exécutable est dans `dir` (le processus courant est exclu).
pub fn grx_processes_in(dir: &Path) -> Vec<GrxProcess> {
    let Ok(mut sys) = System::new() else {
        return Vec::new();
    };
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing().with_exe(UpdateKind::Always));
    let me = std::process::id();
    let mut out: Vec<GrxProcess> = sys
        .processes()
        .iter()
        .filter_map(|(pid, p)| {
            let stem = stem_of(&p.name().to_string_lossy());
            let exe = p.exe()?.to_path_buf();
            let parent = exe.parent()?;
            (pid.as_u32() != me && WATCHED.contains(&stem.as_str()) && is_within(parent, dir) && is_within(dir, parent))
                .then(|| GrxProcess { pid: pid.as_u32(), name: stem, exe })
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

/// Raisons d'attendre avant d'installer (vide = on peut installer).
pub fn busy(dir: &Path) -> Vec<String> {
    grx_processes_in(dir)
        .into_iter()
        .filter_map(|p| match p.name.as_str() {
            "grx-host" => Some(format!("le host est en marche (pid {}) : un joueur peut être connecté", p.pid)),
            "grx-client" => Some(format!("une session de jeu est ouverte (grx-client, pid {})", p.pid)),
            _ => None,
        })
        .collect()
}

/// Arrête les grx-host / grx-client de `dir` proprement (Ctrl+C : le host referme la redirection
/// de la box et relâche les entrées), puis de force après `grace`. Les autres grx-gui de `dir`
/// reçoivent une demande de fermeture (WM_CLOSE). Renvoie le nombre de processus encore vivants.
pub fn stop_in_dir(dir: &Path, grace: Duration) -> usize {
    let procs = grx_processes_in(dir);
    for p in procs.iter().filter(|p| p.name != "grx-gui") {
        // Host élevé (mode Contrôle total) : grx-gui non élevé ne peut pas lui envoyer Ctrl+C ;
        // on déclenche d'abord son événement nommé d'arrêt propre (sans effet sur un host normal),
        // puis on tente Ctrl+C comme avant. L'installateur en mode machine est élevé : les deux marchent.
        platform::signal_stop_event(p.pid);
        platform::send_ctrl_c(p.pid);
    }
    let t0 = Instant::now();
    while t0.elapsed() < grace && grx_processes_in(dir).iter().any(|p| p.name != "grx-gui") {
        std::thread::sleep(Duration::from_millis(200));
    }
    for p in grx_processes_in(dir).iter().filter(|p| p.name != "grx-gui") {
        platform::terminate(p.pid);
    }
    for p in grx_processes_in(dir).iter().filter(|p| p.name == "grx-gui") {
        platform::close_windows(p.pid);
    }
    let t1 = Instant::now();
    while t1.elapsed() < grace && !grx_processes_in(dir).is_empty() {
        std::thread::sleep(Duration::from_millis(200));
    }
    grx_processes_in(dir).len()
}

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM};
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
    };
    use windows_sys::Win32::System::Threading::{
        OpenEventW, OpenProcess, SetEvent, TerminateProcess, EVENT_MODIFY_STATE, PROCESS_TERMINATE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE};

    /// Déclenche l'événement d'arrêt propre d'un host élevé (voir `grx_core::elevation`). Sans
    /// effet (silencieux) si l'événement n'existe pas (host normal ou déjà arrêté).
    pub fn signal_stop_event(pid: u32) {
        let name: Vec<u16> = format!("Local\\GamingRemoteExperience-Host-Stop-{pid}\0").encode_utf16().collect();
        // SAFETY : handle vérifié puis fermé ; nom terminé par NUL.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
            if !h.is_null() {
                SetEvent(h);
                CloseHandle(h);
            }
        }
    }

    /// Même méthode que grx-gui : s'attacher à la console du processus, émettre Ctrl+C en
    /// l'ignorant soi-même, se détacher.
    pub fn send_ctrl_c(pid: u32) -> bool {
        // SAFETY : appels Win32 sans pointeur ; seul ce fil modifie l'état de console du processus.
        unsafe {
            FreeConsole();
            if AttachConsole(pid) == 0 {
                return false;
            }
            SetConsoleCtrlHandler(None, 1);
            let ok = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) != 0;
            std::thread::sleep(Duration::from_millis(150));
            FreeConsole();
            SetConsoleCtrlHandler(None, 0);
            ok
        }
    }

    pub fn terminate(pid: u32) {
        // SAFETY : handle vérifié puis fermé.
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }

    unsafe extern "system" fn post_close(hwnd: HWND, lparam: LPARAM) -> i32 {
        let mut owner = 0u32;
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == lparam as u32 {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        1
    }

    pub fn close_windows(pid: u32) {
        // SAFETY : rappel sans état partagé, pid passé par valeur.
        unsafe {
            EnumWindows(Some(post_close), pid as LPARAM);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn signal_stop_event(_pid: u32) {}
    pub fn send_ctrl_c(_pid: u32) -> bool {
        false
    }
    pub fn terminate(_pid: u32) {}
    pub fn close_windows(_pid: u32) {}
}

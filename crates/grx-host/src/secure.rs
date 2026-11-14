//! Observation (LECTURE SEULE) du bureau d'entrée et de l'intégrité de la fenêtre au premier plan,
//! pour la politique pure de `grx_core::secure`.
//!
//! Un thread `grx-secure` unique (démarré par `run`) interroge Windows toutes les 200 ms et publie
//! la situation dans un atomique global ; la boucle réseau la lit à chaque tour (aucun appel
//! système sur le chemin chaud). Aucun privilège n'est demandé ni utilisé : `OpenInputDesktop`
//! en lecture, `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` + `TokenIntegrityLevel`.
//!
//! Sans thread démarré (tests du serveur), la situation reste « normale ».
//! Mode de test caché `run --simulate-secure <normal|elevated|secure|cycle>` : la situation est
//! simulée (aucune lecture de Windows), pour vérifier le bandeau de l'overlay sans UAC.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use grx_core::secure::HostSituation;

/// Situation courante encodée (bits : 1 bureau sécurisé, 2 fenêtre élevée, 4 host élevé,
/// 8 fenêtre au-dessus du host).
static CURRENT: AtomicU8 = AtomicU8::new(0);

fn encode(s: &HostSituation) -> u8 {
    (s.secure_desktop as u8) | (s.elevated_foreground as u8) << 1 | (s.host_elevated as u8) << 2 | (s.foreground_above_host as u8) << 3
}

fn decode(b: u8) -> HostSituation {
    HostSituation {
        secure_desktop: b & 1 != 0,
        elevated_foreground: b & 2 != 0,
        host_elevated: b & 4 != 0,
        foreground_above_host: b & 8 != 0,
    }
}

/// Dernière situation observée.
pub fn current() -> HostSituation {
    decode(CURRENT.load(Ordering::Relaxed))
}

/// Simulation (mode de test caché).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Simulate {
    Normal,
    /// Fenêtre administrateur au premier plan, host non élevé.
    Elevated,
    /// Bureau sécurisé (UAC).
    Secure,
    /// normal → elevated → secure → normal… toutes les 6 s.
    Cycle,
}

fn simulated(sim: Simulate, t: Duration, host_elevated: bool) -> HostSituation {
    let state = match sim {
        Simulate::Cycle => [Simulate::Normal, Simulate::Elevated, Simulate::Secure][(t.as_secs() / 6 % 3) as usize],
        s => s,
    };
    let elevated = state == Simulate::Elevated;
    HostSituation {
        secure_desktop: state == Simulate::Secure,
        elevated_foreground: elevated,
        host_elevated,
        foreground_above_host: elevated && !host_elevated,
    }
}

/// Démarre le thread d'observation (une seule fois par processus).
pub fn start(simulate: Option<Simulate>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let own = own_integrity();
        let host_elevated = own.is_some_and(|r| r >= HIGH_RID);
        let _ = std::thread::Builder::new().name("grx-secure".into()).spawn(move || {
            let t0 = Instant::now();
            let mut last = u8::MAX;
            loop {
                let s = match simulate {
                    Some(sim) => simulated(sim, t0.elapsed(), host_elevated),
                    None => observe(own),
                };
                let b = encode(&s);
                CURRENT.store(b, Ordering::Relaxed);
                if b != last {
                    last = b;
                    println!("  [sécurité] {}", describe(&s));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
    });
}

fn describe(s: &HostSituation) -> String {
    if s.secure_desktop {
        "bureau sécurisé Windows actif (UAC / verrouillage) : ni image ni entrée".into()
    } else if s.foreground_above_host {
        "fenêtre d'intégrité supérieure au host au premier plan : clavier/souris non injectés".into()
    } else if s.elevated_foreground {
        "fenêtre administrateur au premier plan (host élevé : rôle « Contrôle total » seulement)".into()
    } else {
        "bureau normal".into()
    }
}

/// RID d'intégrité « élevée » (`SECURITY_MANDATORY_HIGH_RID`).
pub const HIGH_RID: u32 = 0x3000;

/// Situation réelle. `own` : intégrité du host (inconnue = considérée moyenne).
fn observe(own: Option<u32>) -> HostSituation {
    let own = own.unwrap_or(0x2000);
    let secure_desktop = !input_desktop_is_default();
    let fg = foreground_integrity();
    // Fenêtre au premier plan illisible : prudence (traitée comme élevée et hors d'atteinte).
    let (elevated_foreground, foreground_above_host) = match fg {
        Fg::None => (false, false),
        Fg::Rid(r) => (r >= HIGH_RID, r > own),
        Fg::Unknown => (true, true),
    };
    HostSituation { secure_desktop, elevated_foreground, host_elevated: own >= HIGH_RID, foreground_above_host }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fg {
    /// Aucune fenêtre au premier plan (transition) : rien de bloqué.
    None,
    Rid(u32),
    Unknown,
}

#[cfg(windows)]
mod win {
    use super::Fg;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

    /// Le bureau qui reçoit les entrées est-il `Default` ? Faux si c'est un autre bureau
    /// (`Winlogon` : UAC, Ctrl+Alt+Suppr) ou s'il est inaccessible (cas de l'UAC pour un processus
    /// non SYSTEM : accès refusé).
    pub fn input_desktop_is_default() -> bool {
        // SAFETY : handle fermé ci-dessous ; tampon local de taille annoncée.
        unsafe {
            let h = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
            if h.is_null() {
                return false;
            }
            let mut buf = [0u16; 64];
            let mut needed = 0u32;
            let ok = GetUserObjectInformationW(h, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed) != 0;
            CloseDesktop(h);
            if !ok {
                return false;
            }
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..n]).eq_ignore_ascii_case("Default")
        }
    }

    /// RID d'intégrité du jeton d'un processus ouvert.
    ///
    /// SAFETY (appelant) : `process` est un handle de processus valide avec au moins
    /// PROCESS_QUERY_LIMITED_INFORMATION.
    unsafe fn integrity_of_process(process: HANDLE) -> Option<u32> {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut tok) == 0 {
            return None;
        }
        // TOKEN_MANDATORY_LABEL + SID (≤ 68 octets) : 128 octets alignés suffisent.
        let mut buf = [0u64; 16];
        let mut len = 0u32;
        let ok = GetTokenInformation(tok, TokenIntegrityLevel, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &mut len) != 0;
        CloseHandle(tok);
        if !ok {
            return None;
        }
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_null() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, (count - 1) as u32))
    }

    pub fn own_integrity() -> Option<u32> {
        // SAFETY : pseudo-handle du processus courant, toujours valide.
        unsafe { integrity_of_process(GetCurrentProcess()) }
    }

    pub fn integrity_of_pid(pid: u32) -> Option<u32> {
        // SAFETY : handle fermé avant retour.
        unsafe {
            let p = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if p.is_null() {
                return None;
            }
            let r = integrity_of_process(p);
            CloseHandle(p);
            r
        }
    }

    pub fn foreground_integrity() -> Fg {
        // SAFETY : appels sans pointeur à durée de vie (pid écrit dans une variable locale).
        unsafe {
            let w = GetForegroundWindow();
            if w.is_null() {
                return Fg::None;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(w, &mut pid);
            if pid == 0 {
                return Fg::None;
            }
            integrity_of_pid(pid).map_or(Fg::Unknown, Fg::Rid)
        }
    }
}

#[cfg(windows)]
#[allow(unused_imports)] // `integrity_of_pid` n'est utilisé que par les tests.
pub use win::{foreground_integrity, input_desktop_is_default, integrity_of_pid, own_integrity};

#[cfg(not(windows))]
pub fn input_desktop_is_default() -> bool {
    true
}
#[cfg(not(windows))]
pub fn own_integrity() -> Option<u32> {
    None
}
#[cfg(not(windows))]
pub fn integrity_of_pid(_pid: u32) -> Option<u32> {
    None
}
#[cfg(not(windows))]
pub fn foreground_integrity() -> Fg {
    Fg::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn situation_encoding_roundtrips() {
        for b in 0..16u8 {
            assert_eq!(encode(&decode(b)), b);
        }
    }

    #[test]
    fn simulation_cycles_through_the_three_states() {
        let at = |s| simulated(Simulate::Cycle, Duration::from_secs(s), false);
        assert_eq!(at(0), HostSituation::default());
        assert!(at(7).elevated_foreground && at(7).foreground_above_host && !at(7).secure_desktop);
        assert!(at(13).secure_desktop && !at(13).elevated_foreground);
        assert_eq!(at(19), HostSituation::default());
        // Host élevé simulé : la fenêtre administrateur est atteignable.
        let s = simulated(Simulate::Elevated, Duration::ZERO, true);
        assert!(s.elevated_foreground && !s.foreground_above_host && s.host_elevated);
    }

    /// Test réel, lecture seule : intégrité de processus déjà présents.
    #[cfg(windows)]
    #[test]
    fn real_integrity_levels_are_read_only_observed() {
        let own = own_integrity().expect("intégrité du processus de test");
        assert!((0x1000..=0x4000).contains(&own), "{own:#x}");
        // Processus « System » (pid 4) ou smss : protégés, ouverture refusée → None, sans panique.
        let _ = integrity_of_pid(4);
        // csrss/wininit/services tournent en SYSTEM (0x4000) : au moins un est lisible en
        // PROCESS_QUERY_LIMITED_INFORMATION depuis un processus élevé ; non élevé, l'ouverture
        // peut être refusée. On vérifie seulement que la lecture ne ment pas.
        if let Some(r) = find_pid("winlogon.exe").and_then(integrity_of_pid) {
            assert_eq!(r, 0x4000, "winlogon tourne en intégrité système");
        }
        // Le bureau courant d'une session de test interactive est normalement « Default ».
        let _ = input_desktop_is_default();
        let _ = foreground_integrity();
    }

    #[cfg(windows)]
    fn find_pid(name: &str) -> Option<u32> {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("IMAGENAME eq {name}"), "/FO", "CSV", "/NH"]).output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        s.lines().find_map(|l| l.split(',').nth(1).and_then(|p| p.trim_matches('"').parse().ok()))
    }
}

//! Lecture de `status.json` (écrit par `grx-host run`, voir `crates/grx-host/src/status.rs`) et
//! traduction en texte pour l'interface. Analyse pure et testée ; seule `read_live` touche au disque.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

pub const FILE: &str = "status.json";

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct UpnpStatus {
    pub ok: bool,
    pub state: String,
    pub method: Option<String>,
    pub external_addr: Option<String>,
    pub lease_secs: Option<u64>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct FirewallStatus {
    pub state: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct RunStatus {
    pub pid: u32,
    pub bind: String,
    pub started_ms: u64,
    pub upnp: UpnpStatus,
    pub ipv6: Vec<String>,
    pub firewall: FirewallStatus,
}

impl RunStatus {
    pub fn bind_addr(&self) -> Option<SocketAddr> {
        self.bind.parse().ok()
    }

    /// Le host écoute aussi en IPv6 (`--bind [::]:port`).
    pub fn dual_stack(&self) -> bool {
        matches!(self.bind_addr(), Some(SocketAddr::V6(a)) if a.ip().is_unspecified())
    }

    pub fn port(&self) -> Option<u16> {
        self.bind_addr().map(|b| b.port())
    }

    /// Adresse `[ipv6]:port` à proposer en alternative, seulement si le host accepte l'IPv6.
    pub fn ipv6_alt(&self) -> Option<String> {
        let port = self.port()?;
        if !self.dual_stack() {
            return None;
        }
        let ip: std::net::Ipv6Addr = self.ipv6.first()?.parse().ok()?;
        Some(SocketAddr::new(ip.into(), port).to_string())
    }
}

/// Contenu du fichier -> état ; JSON invalide = `None` (traité comme « host arrêté »).
pub fn parse(bytes: &[u8]) -> Option<RunStatus> {
    serde_json::from_slice::<RunStatus>(bytes).ok().filter(|s| s.pid != 0)
}

/// État d'un host vivant : fichier présent, lisible ET `pid` en cours d'exécution.
/// Retourne aussi l'âge du fichier (dernière écriture = dernier renouvellement du bail).
pub fn read_live(dir: &Path, alive: impl Fn(u32) -> bool) -> Option<(RunStatus, Duration)> {
    let path = dir.join(FILE);
    let bytes = std::fs::read(&path).ok()?;
    let st = parse(&bytes)?;
    if !alive(st.pid) {
        return None;
    }
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    Some((st, age))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tone {
    Ok,
    Pending,
    Warn,
    Error,
}

/// Une ligne d'état (titre + détails + conseil éventuel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub tone: Tone,
    pub title: String,
    pub detail: Option<String>,
    pub advice: Option<String>,
}

fn method_label(m: Option<&str>) -> String {
    match m {
        Some("upnp") => "UPnP".into(),
        Some("pcp") => "PCP".into(),
        Some("natpmp") | Some("nat-pmp") => "NAT-PMP".into(),
        Some(o) => o.to_uppercase(),
        None => "?".into(),
    }
}

pub fn format_lease(lease_secs: Option<u64>, age: Duration) -> String {
    match lease_secs {
        Some(0) => "bail permanent (supprimé à l'arrêt du host)".into(),
        Some(l) => {
            let left = l.saturating_sub(age.as_secs());
            format!("bail restant ≈ {} min sur {} min, renouvelé automatiquement", left.div_ceil(60), l / 60)
        }
        None => "bail inconnu".into(),
    }
}

/// État de la redirection de port.
pub fn redirect_line(st: &RunStatus, age: Duration) -> Line {
    let port = st.port().unwrap_or(crate::logic::DEFAULT_PORT);
    let manual = format!(
        "Redirigez à la main le port UDP {port} de la box vers ce PC (ou activez l'UPnP dans l'interface de la box), utilisez l'IPv6 ou un VPN (Tailscale…)."
    );
    let u = &st.upnp;
    match u.state.as_str() {
        "ok" if u.ok => Line {
            tone: Tone::Ok,
            title: format!(
                "Port ouvert automatiquement ({}) : {}",
                method_label(u.method.as_deref()),
                u.external_addr.as_deref().unwrap_or("?")
            ),
            detail: Some(
                std::iter::once(format_lease(u.lease_secs, age))
                    .chain(u.warnings.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" — "),
            ),
            advice: None,
        },
        "pending" => Line {
            tone: Tone::Pending,
            title: "Ouverture du port sur la box en cours…".into(),
            detail: None,
            advice: None,
        },
        "disabled" => Line {
            tone: Tone::Warn,
            title: "Ouverture automatique du port désactivée".into(),
            detail: u.error.clone(),
            advice: Some(manual),
        },
        _ => {
            let err = u.error.clone().unwrap_or_else(|| "cause inconnue".into());
            let low = err.to_lowercase();
            let advice = if low.contains("cgnat") || low.contains("100.64") {
                "Votre opérateur partage l'IPv4 (CGNAT) : aucune redirection n'est possible. Demandez une IPv4 publique « full-stack » à votre FAI, ou utilisez l'IPv6 ou un VPN.".to_string()
            } else if low.contains("double nat") || low.contains("privée") {
                "La box est derrière un autre routeur (double NAT) : passez-la en mode bridge ou redirigez aussi le port sur le routeur amont.".to_string()
            } else {
                manual
            };
            Line { tone: Tone::Error, title: "Échec de l'ouverture automatique du port".into(), detail: Some(err), advice: Some(advice) }
        }
    }
}

/// État du pare-feu Windows (lecture seule côté host).
pub fn firewall_line(st: &RunStatus) -> Line {
    let f = &st.firewall;
    let (tone, title) = match f.state.as_str() {
        "ok" => (Tone::Ok, "Pare-feu Windows : grx-host est autorisé"),
        "checking" | "" => (Tone::Pending, "Pare-feu Windows : vérification…"),
        "missing" => (Tone::Warn, "Pare-feu Windows : aucune règle n'autorise grx-host"),
        "blocked" => (Tone::Error, "Pare-feu Windows : une règle BLOQUE grx-host"),
        "wrong_profile" => (Tone::Warn, "Pare-feu Windows : règle présente mais pas pour ce profil réseau"),
        _ => (Tone::Warn, "Pare-feu Windows : état inconnu"),
    };
    let advice = matches!(tone, Tone::Warn | Tone::Error).then(|| {
        "Les clients extérieurs seront bloqués : « Voir la commande » donne la commande à lancer vous-même en administrateur.".to_string()
    });
    Line { tone, title: title.into(), detail: f.detail.clone(), advice }
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    if pid == 0 {
        return false;
    }
    // SAFETY : handle vérifié non nul puis fermé ; `code` est une variable locale.
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

    const SAMPLE: &str = r#"{ "pid": 1234, "bind": "0.0.0.0:7777", "started_ms": 1731400000000,
      "upnp": { "ok": true, "state": "ok", "method": "upnp", "external_addr": "198.51.100.7:7777",
                "lease_secs": 3600, "error": null, "warnings": [] },
      "ipv6": ["2001:db8::5"], "firewall": { "state": "missing", "detail": "aucune règle" } }"#;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("grx-gui-status-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_the_host_format() {
        let s = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(s.pid, 1234);
        assert_eq!(s.port(), Some(7777));
        assert!(s.upnp.ok);
        assert_eq!(s.upnp.external_addr.as_deref(), Some("198.51.100.7:7777"));
        assert!(!s.dual_stack());
        assert_eq!(s.ipv6_alt(), None, "IPv4 seul : pas de lien IPv6");
        let l = redirect_line(&s, Duration::from_secs(600));
        assert_eq!(l.tone, Tone::Ok);
        assert!(l.title.contains("UPnP") && l.title.contains("198.51.100.7:7777"));
        assert!(l.detail.unwrap().contains("≈ 50 min sur 60 min"));
        assert_eq!(firewall_line(&s).tone, Tone::Warn);
    }

    #[test]
    fn ipv6_alt_only_when_dual_stack() {
        let s = parse(SAMPLE.replace("0.0.0.0:7777", "[::]:7777").as_bytes()).unwrap();
        assert!(s.dual_stack());
        assert_eq!(s.ipv6_alt().as_deref(), Some("[2001:db8::5]:7777"));
        let s = parse(SAMPLE.replace("0.0.0.0:7777", "[::]:7777").replace("\"2001:db8::5\"", "").as_bytes()).unwrap();
        assert_eq!(s.ipv6_alt(), None);
    }

    #[test]
    fn minimal_and_older_files_are_accepted_with_defaults() {
        let s = parse(br#"{"pid":7,"bind":"0.0.0.0:9000","upnp":{"ok":false,"state":"pending"}}"#).unwrap();
        assert_eq!(redirect_line(&s, Duration::ZERO).tone, Tone::Pending);
        assert_eq!(firewall_line(&s).tone, Tone::Pending);
        assert!(s.ipv6.is_empty());
    }

    #[test]
    fn corrupt_or_empty_or_pidless_file_means_stopped() {
        assert!(parse(b"").is_none());
        assert!(parse(b"{pas du json").is_none());
        assert!(parse(br#"{"bind":"0.0.0.0:7777"}"#).is_none(), "pid absent = 0");
    }

    #[test]
    fn read_live_requires_file_and_living_pid() {
        let d = tmp("live");
        assert!(read_live(&d, |_| true).is_none(), "fichier absent");
        std::fs::write(d.join(FILE), SAMPLE).unwrap();
        assert!(read_live(&d, |_| false).is_none(), "pid mort");
        let (s, age) = read_live(&d, |p| p == 1234).unwrap();
        assert_eq!(s.pid, 1234);
        assert!(age < Duration::from_secs(60));
        std::fs::write(d.join(FILE), b"{corrompu").unwrap();
        assert!(read_live(&d, |_| true).is_none(), "corrompu");
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn failures_get_specific_advice() {
        let mk = |state: &str, err: Option<&str>| RunStatus {
            pid: 1,
            bind: "0.0.0.0:7777".into(),
            upnp: UpnpStatus { state: state.into(), error: err.map(Into::into), ..Default::default() },
            ..Default::default()
        };
        let l = redirect_line(&mk("error", Some("l'UPnP ne suffira pas : IP 100.64.1.2 dans 100.64.0.0/10 (CGNAT de l'opérateur)")), Duration::ZERO);
        assert_eq!(l.tone, Tone::Error);
        assert!(l.advice.unwrap().contains("CGNAT"));
        let l = redirect_line(&mk("error", Some("aucune passerelle UPnP trouvée")), Duration::ZERO);
        assert!(l.advice.unwrap().contains("Redirigez à la main le port UDP 7777"));
        let l = redirect_line(&mk("disabled", None), Duration::ZERO);
        assert_eq!(l.tone, Tone::Warn);
        assert!(l.advice.unwrap().contains("à la main"));
        let l = redirect_line(&mk("error", Some("IP privée 192.168.1.1 (double NAT)")), Duration::ZERO);
        assert!(l.advice.unwrap().contains("bridge"));
    }

    #[test]
    fn lease_formatting() {
        assert!(format_lease(Some(0), Duration::ZERO).contains("permanent"));
        assert!(format_lease(Some(3600), Duration::from_secs(4000)).contains("≈ 0 min"));
        assert_eq!(format_lease(None, Duration::ZERO), "bail inconnu");
    }

    #[test]
    fn firewall_states() {
        let mk = |s: &str| RunStatus { firewall: FirewallStatus { state: s.into(), detail: None }, ..Default::default() };
        assert_eq!(firewall_line(&mk("ok")).tone, Tone::Ok);
        assert_eq!(firewall_line(&mk("blocked")).tone, Tone::Error);
        assert!(firewall_line(&mk("blocked")).advice.is_some());
        assert_eq!(firewall_line(&mk("wrong_profile")).tone, Tone::Warn);
        assert_eq!(firewall_line(&mk("bizarre")).tone, Tone::Warn);
        assert!(firewall_line(&mk("ok")).advice.is_none());
    }
}

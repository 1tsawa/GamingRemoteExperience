//! Redirection automatique du port du host (UPnP / PCP / NAT-PMP) dans un thread à part : la
//! boucle réseau n'attend jamais la box, et un échec n'empêche jamais le host de tourner.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use grx_portmap::{plan, Error, Mapping, PortMapper};

use crate::status::{StatusFile, UpnpStatus};

/// Après un échec définitif (pas d'IP publique, refus) : nouvel essai bien plus tard.
const RETRY_AFTER_HARD_FAILURE: Duration = Duration::from_secs(30 * 60);
/// Après un échec ordinaire (pas d'IGD...) : nouvel essai, délai croissant jusqu'à ça.
const RETRY_MAX: Duration = Duration::from_secs(10 * 60);

/// Adresse locale IPv4 à rediriger pour une adresse d'écoute donnée. `Err` = pas de redirection
/// possible/utile (message pour l'utilisateur).
pub fn local_for_bind(bind: SocketAddr) -> Result<SocketAddrV4, String> {
    let port = bind.port();
    match bind.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => Err(format!("écoute sur {ip} (boucle locale) : pas de redirection")),
        IpAddr::V4(ip) => Ok(SocketAddrV4::new(ip, port)),
        IpAddr::V6(ip) if ip.is_unspecified() => Ok(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)), // double pile
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(v4) if !v4.is_loopback() => Ok(SocketAddrV4::new(v4, port)),
            _ => Err(format!("écoute IPv6 seule ({ip}) : pas de redirection IPv4 (en IPv6, ouvrez seulement le pare-feu)")),
        },
    }
}

pub struct PortmapTask {
    stop: mpsc::Sender<()>,
    done: mpsc::Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

fn ok_status(m: &Mapping) -> UpnpStatus {
    UpnpStatus {
        ok: true,
        state: "ok".into(),
        method: Some(m.method.as_str().into()),
        external_addr: Some(m.external.to_string()),
        lease_secs: Some(m.lease.as_secs()),
        error: None,
        warnings: m.warnings.clone(),
    }
}

fn err_status(e: &str) -> UpnpStatus {
    UpnpStatus { ok: false, state: "error".into(), error: Some(e.to_string()), ..Default::default() }
}

fn announce(m: &Mapping) {
    let lease = if m.lease.is_zero() { "permanent (supprimé à l'arrêt)".to_string() } else { format!("{} s, renouvelé à mi-vie", m.lease.as_secs()) };
    println!("[upnp] Redirection active via {} : {} -> {} (bail {lease})", m.method, m.external, m.local);
    println!("[upnp] Adresse à donner aux clients : {}   (grx-host invite --addr auto)", m.external);
    for w in &m.warnings {
        println!("[upnp] Attention : {w}");
    }
}

impl PortmapTask {
    pub fn start(local: SocketAddrV4, external_hint: u16, lease: Duration, status: StatusFile) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let handle = thread::Builder::new()
            .name("grx-portmap".into())
            .spawn(move || {
                run(local, external_hint, lease, &status, &stop_rx);
                let _ = done_tx.send(());
            })
            .expect("thread grx-portmap");
        Self { stop: stop_tx, done: done_rx, handle: Some(handle) }
    }

    /// Demande la suppression de la redirection et attend au plus `timeout` (la box peut ne
    /// pas répondre : la redirection expirera alors d'elle-même à la fin du bail).
    pub fn stop(mut self, timeout: Duration) {
        let _ = self.stop.send(());
        match self.done.recv_timeout(timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                eprintln!("[upnp] La box ne répond pas : la redirection expirera d'elle-même à la fin du bail.");
            }
        }
    }
}

fn run(local: SocketAddrV4, hint: u16, lease: Duration, status: &StatusFile, stop: &mpsc::Receiver<()>) {
    let pm = PortMapper::default();
    let mut mapping: Option<Mapping> = None;
    let mut failures: u32 = 0;
    let mut last_error = String::new();
    // Port externe à redemander après une perte (pour garder les invitations valides).
    let mut want_port = hint;
    println!("[upnp] Recherche de la box (UPnP, puis PCP/NAT-PMP) pour ouvrir le port UDP {hint}...");
    loop {
        let wait = match mapping.as_mut() {
            None => match pm.open(local, want_port, lease, "grx-host") {
                Ok(m) => {
                    failures = 0;
                    last_error.clear();
                    if m.external.port() != hint {
                        println!("[upnp] Le port externe {hint} est déjà pris par une autre machine : port {} utilisé à la place.", m.external.port());
                    }
                    announce(&m);
                    status.update(|s| s.upnp = ok_status(&m));
                    want_port = m.external.port();
                    let d = m.renew_in();
                    mapping = Some(m);
                    d
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg != last_error {
                        eprintln!("[upnp] Échec de la redirection automatique : {msg}");
                        eprintln!("[upnp] Le host continue : joignable en local (ou redirigez le port UDP {} à la main sur la box).", local.port());
                        last_error = msg.clone();
                    }
                    status.update(|s| s.upnp = err_status(&msg));
                    failures += 1;
                    match e {
                        Error::NoPublicIp { .. } | Error::NotAuthorized(_) => RETRY_AFTER_HARD_FAILURE,
                        _ => plan::retry_delay(failures.saturating_sub(1) + 2, None).min(RETRY_MAX),
                    }
                }
            },
            Some(m) => {
                let before = m.external;
                match m.renew() {
                    Ok(()) => {
                        failures = 0;
                        if m.external != before {
                            println!("[upnp] L'adresse publique a changé : {before} -> {} (les anciennes invitations ne marchent plus ; un nom DuckDNS évite ça)", m.external);
                        }
                        status.update(|s| s.upnp = ok_status(m));
                        m.renew_in()
                    }
                    Err(e) => {
                        failures += 1;
                        eprintln!("[upnp] Renouvellement impossible ({e}), nouvel essai bientôt.");
                        let lost = matches!(e, Error::PortTaken(_)) || m.remaining().is_some_and(|r| r.is_zero());
                        if lost {
                            // Redirection perdue : on en recrée une (même port si possible).
                            mapping = None;
                            status.update(|s| s.upnp = err_status(&format!("redirection perdue : {e}")));
                            plan::MIN_RENEW
                        } else {
                            plan::retry_delay(failures - 1, m.remaining())
                        }
                    }
                }
            }
        };
        match stop.recv_timeout(wait) {
            Err(RecvTimeoutError::Timeout) => continue,
            _ => break,
        }
    }
    if let Some(mut m) = mapping {
        match m.close() {
            Ok(()) => println!("[upnp] Redirection {} supprimée.", m.external),
            Err(e) => eprintln!("[upnp] Suppression de la redirection impossible ({e}) : elle expirera à la fin du bail."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_address_for_each_bind() {
        let l = |s: &str| local_for_bind(s.parse().unwrap());
        assert_eq!(l("0.0.0.0:7777").unwrap(), "0.0.0.0:7777".parse().unwrap());
        assert_eq!(l("192.168.1.20:7000").unwrap(), "192.168.1.20:7000".parse().unwrap());
        assert_eq!(l("[::]:7777").unwrap(), "0.0.0.0:7777".parse().unwrap());
        assert!(l("127.0.0.1:7777").is_err());
        assert!(l("[2a01:e0a::1]:7777").is_err());
        assert!(l("[::1]:7777").is_err());
    }
}

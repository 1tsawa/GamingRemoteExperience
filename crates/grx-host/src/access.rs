//! `invite --addr auto` : adresse publique du host (redirection active d'un host lancé, sinon IP
//! externe de la box), et alternative IPv6 tirée des interfaces locales (aucune requête externe).

use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use anyhow::{bail, Result};
use grx_portmap::{global_ipv6, plan, AddrClass, PortMapper};

use crate::status::{self, HostStatus};

pub struct AutoAddr {
    pub primary: String,
    /// Lien IPv6 alternatif (même jeton : un seul des deux liens servira).
    pub ipv6_alt: Option<String>,
    pub notes: Vec<String>,
}

fn is_dual_stack(bind: Option<SocketAddr>) -> bool {
    matches!(bind, Some(SocketAddr::V6(a)) if a.ip().is_unspecified())
}

pub fn resolve(dir: &Path, port: u16) -> Result<AutoAddr> {
    resolve_with(status::read_live(dir), port, || PortMapper::default().external_ip().map_err(|e| e.to_string()))
}

fn resolve_with(
    live: Option<HostStatus>,
    port: u16,
    query: impl FnOnce() -> std::result::Result<grx_portmap::ExternalIp, String>,
) -> Result<AutoAddr> {
    let mut notes = Vec::new();
    let bind = live.as_ref().and_then(|s| s.bind_addr());
    let host_port = bind.map(|b| b.port()).unwrap_or(port);

    // IPv6 : adresses globales stables d'abord.
    let v6 = global_ipv6();
    let ipv6_alt = v6.first().map(|g| {
        if g.temporary {
            notes.push("IPv6 : seule une adresse temporaire existe, elle changera (extensions de confidentialité)".into());
        }
        SocketAddr::new(IpAddr::V6(g.addr), host_port).to_string()
    });
    if ipv6_alt.is_some() {
        match &live {
            Some(_) if !is_dual_stack(bind) => notes.push(format!(
                "IPv6 : le host tourne en IPv4 seul ; relancez-le avec --bind [::]:{host_port} pour accepter l'IPv6"
            )),
            None => notes.push(format!("IPv6 : lancez le host avec --bind [::]:{host_port} (IPv4 + IPv6)")),
            _ => {}
        }
        notes.push(format!(
            "IPv6 : pas de redirection nécessaire, autorisez seulement le port UDP {host_port} dans le pare-feu Windows \
             (grx-host firewall-help) ; certaines box filtrent aussi l'IPv6 entrant (option « pare-feu IPv6 » de la box)"
        ));
    }

    // IPv4 : redirection d'un host vivant, sinon IP externe de la box.
    if let Some(s) = &live {
        if s.upnp.ok {
            if let Some(a) = &s.upnp.external_addr {
                notes.insert(0, format!("adresse de la redirection {} active du host (pid {})", s.upnp.method.as_deref().unwrap_or("?"), s.pid));
                return Ok(AutoAddr { primary: a.clone(), ipv6_alt, notes });
            }
        }
        match s.upnp.state.as_str() {
            "disabled" => notes.push(format!(
                "le host tourne avec --no-upnp : le port UDP {host_port} doit être redirigé à la main sur la box"
            )),
            "pending" => notes.push("le host cherche encore la box : relancez `invite --addr auto` dans quelques secondes".into()),
            _ => notes.push(format!(
                "le host n'a pas pu ouvrir la redirection ({}) : redirigez le port UDP {host_port} à la main",
                s.upnp.error.as_deref().unwrap_or("?")
            )),
        }
    }
    let v4_err = match query() {
        Ok(e) if e.class == AddrClass::Public => {
            if live.is_none() {
                notes.push(format!(
                    "aucun host lancé : la redirection n'existe pas encore. Lancez `grx-host run` (redirection UPnP automatique du port {host_port}) ; \
                     si ce port est pris par une autre machine, le host en choisira un autre : régénérez alors l'invitation"
                ));
            }
            notes.insert(0, format!("IP externe lue sur la box via {}", e.method));
            return Ok(AutoAddr { primary: SocketAddr::new(IpAddr::V4(e.ip), host_port).to_string(), ipv6_alt, notes });
        }
        Ok(e) => plan::explain_non_public(e.ip, e.class),
        Err(e) => format!("IP externe introuvable : {e}"),
    };
    if let Some(v6) = ipv6_alt {
        notes.insert(0, format!("IPv4 impossible ({v4_err}) : lien IPv6 uniquement"));
        return Ok(AutoAddr { primary: v6, ipv6_alt: None, notes });
    }
    bail!(
        "impossible de déterminer l'adresse publique : {v4_err}.\nDonnez-la à la main : --addr MON_IP_OU_NOM_DUCKDNS:{host_port}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::UpnpStatus;
    use grx_portmap::{ExternalIp, Method};

    fn ext(ip: &str) -> std::result::Result<ExternalIp, String> {
        let ip = ip.parse().unwrap();
        Ok(ExternalIp { ip, class: grx_portmap::classify_ipv4(ip), method: Method::Upnp, gateway: "192.168.1.1".parse().unwrap(), local: "192.168.1.2".parse().unwrap() })
    }

    #[test]
    fn live_host_mapping_wins() {
        let live = HostStatus {
            pid: 1,
            bind: "0.0.0.0:7777".into(),
            upnp: UpnpStatus { ok: true, state: "ok".into(), method: Some("upnp".into()), external_addr: Some("198.51.100.7:7790".into()), ..Default::default() },
            ..Default::default()
        };
        let a = resolve_with(Some(live), 7777, || panic!("pas de requête à la box")).unwrap();
        assert_eq!(a.primary, "198.51.100.7:7790");
    }

    #[test]
    fn no_host_queries_the_box_and_uses_port() {
        let a = resolve_with(None, 7000, || ext("82.64.1.2")).unwrap();
        assert_eq!(a.primary, "82.64.1.2:7000");
        assert!(a.notes.iter().any(|n| n.contains("aucun host lancé")));
    }

    #[test]
    fn cgnat_is_explained() {
        match resolve_with(None, 7777, || ext("100.70.1.2")) {
            Ok(a) => {
                // Machine avec IPv6 globale : repli IPv6, l'explication CGNAT reste visible.
                assert!(a.primary.starts_with('['));
                assert!(a.notes[0].contains("pas d'IP publique"));
            }
            Err(e) => assert!(e.to_string().contains("pas d'IP publique")),
        }
    }
}

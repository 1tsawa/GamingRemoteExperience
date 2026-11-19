//! grx-portmap : redirection automatique d'un port UDP sur la box (UPnP IGD, puis PCP/NAT-PMP
//! en repli), pour qu'un host soit joignable depuis Internet sans serveur de NAT traversal.
//!
//! ```no_run
//! # use std::time::Duration;
//! let mut m = grx_portmap::PortMapper::default()
//!     .open("0.0.0.0:443".parse().unwrap(), 443, Duration::from_secs(3600), "grx-host")?;
//! println!("joignable sur {} via {}", m.external, m.method);
//! m.renew()?;   // à mi-vie : grx_portmap::plan::renew_delay(m.lease)
//! m.close()?;   // (ou Drop)
//! # Ok::<(), grx_portmap::Error>(())
//! ```

pub mod netif;
pub mod plan;
pub mod pmp;
pub mod upnp;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use rand::RngCore;

pub use netif::{global_ipv6, DefaultRoute, Ipv6Global};
pub use plan::{classify_ipv4, AddrClass};
pub use upnp::{UpnpEntry, UpnpGateway};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Udp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Upnp,
    NatPmp,
    Pcp,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Upnp => "upnp",
            Method::NatPmp => "nat-pmp",
            Method::Pcp => "pcp",
        }
    }
}

impl std::fmt::Display for Method {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Method::Upnp => "UPnP IGD",
            Method::NatPmp => "NAT-PMP",
            Method::Pcp => "PCP",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("aucune passerelle IPv4 par défaut (réseau déconnecté ?)")]
    NoGateway,
    #[error("aucune redirection automatique possible : {0}")]
    NoIgd(String),
    #[error("{}", plan::explain_non_public(*external, *class))]
    NoPublicIp { external: Ipv4Addr, class: AddrClass, method: Method },
    #[error("la passerelle refuse la redirection ({0})")]
    NotAuthorized(String),
    #[error("aucun port externe libre (tous les ports essayés sont pris par une autre machine)")]
    NoPortAvailable,
    #[error("le port externe {0} a été repris par une autre machine")]
    PortTaken(u16),
    #[error("{0}")]
    Protocol(String),
    #[error("redirection déjà fermée")]
    Closed,
}

pub type Result<T> = std::result::Result<T, Error>;

/// IP externe de la box, et comment elle a été obtenue.
#[derive(Debug, Clone)]
pub struct ExternalIp {
    pub ip: Ipv4Addr,
    pub class: AddrClass,
    pub method: Method,
    pub gateway: IpAddr,
    pub local: Ipv4Addr,
}

#[derive(Debug)]
enum Backend {
    Upnp(UpnpGateway),
    NatPmp { gateway: Ipv4Addr },
    Pcp { gateway: Ipv4Addr, nonce: [u8; 12] },
}

/// Une redirection active. Supprimée par [`Mapping::close`] ou au `Drop`.
#[derive(Debug)]
pub struct Mapping {
    /// Adresse publique à donner aux clients.
    pub external: SocketAddr,
    /// Adresse locale (IP de l'interface vers la passerelle + port du host).
    pub local: SocketAddrV4,
    pub protocol: Protocol,
    /// Bail accordé (0 = permanent : la redirection est supprimée à la fermeture).
    pub lease: Duration,
    pub method: Method,
    pub gateway: IpAddr,
    /// Remarques non bloquantes (bail permanent imposé, IGD qui n'est pas la passerelle par défaut...).
    pub warnings: Vec<String>,
    /// Instant du dernier (re)nouvellement réussi.
    pub renewed_at: Instant,
    description: String,
    requested_lease: Duration,
    backend: Backend,
    closed: bool,
}

/// Configuration de la recherche de passerelle et du choix de port.
#[derive(Debug, Clone)]
pub struct PortMapper {
    /// Délai de découverte (SSDP) et de chaque échange PCP/NAT-PMP.
    pub discovery_timeout: Duration,
    pub try_upnp: bool,
    pub try_pmp: bool,
    /// Ports essayés après le port souhaité (hint+1..=hint+n), puis ports aléatoires.
    pub sequential_ports: u16,
    pub random_ports: usize,
    /// Crée quand même la redirection si l'IP externe n'est pas publique (double NAT : l'utilisateur
    /// redirige à la main sur le routeur amont). Défaut : non, erreur explicite.
    pub allow_non_public: bool,
}

impl Default for PortMapper {
    fn default() -> Self {
        Self {
            discovery_timeout: Duration::from_secs(3),
            try_upnp: true,
            try_pmp: true,
            sequential_ports: 4,
            random_ports: 6,
            allow_non_public: false,
        }
    }
}

fn lease_secs(d: Duration) -> u32 {
    d.as_secs().min(u32::MAX as u64) as u32
}

struct Route {
    gateway: Option<Ipv4Addr>,
    local: Option<Ipv4Addr>,
}

impl PortMapper {
    fn route(&self, local: Ipv4Addr) -> Route {
        let def = netif::default_route_v4();
        let local = if local.is_unspecified() { def.map(|d| d.local) } else { Some(local) };
        Route { gateway: def.map(|d| d.gateway), local }
    }

    /// Découvre la passerelle UPnP IGD (≤ `discovery_timeout`).
    pub fn discover_upnp(&self) -> Result<UpnpGateway> {
        let r = self.route(Ipv4Addr::UNSPECIFIED);
        UpnpGateway::discover(r.local, self.discovery_timeout).map_err(Error::NoIgd)
    }

    /// Interroge la passerelle pour connaître l'IP externe (UPnP puis NAT-PMP). Ne crée rien.
    pub fn external_ip(&self) -> Result<ExternalIp> {
        let r = self.route(Ipv4Addr::UNSPECIFIED);
        let mut errs = Vec::new();
        if self.try_upnp {
            match UpnpGateway::discover(r.local, self.discovery_timeout).and_then(|g| Ok((g.external_ip()?, g))) {
                Ok((IpAddr::V4(ip), g)) => {
                    return Ok(ExternalIp { ip, class: classify_ipv4(ip), method: Method::Upnp, gateway: g.gateway_ip(), local: g.local_ip })
                }
                Ok((IpAddr::V6(ip), _)) => errs.push(format!("UPnP : IP externe IPv6 inattendue {ip}")),
                Err(e) => errs.push(format!("UPnP : {e}")),
            }
        }
        if self.try_pmp {
            match (r.gateway, r.local) {
                (Some(gw), Some(local)) => {
                    match pmp::exchange(local, gw, &pmp::natpmp_external_request(), self.discovery_timeout, pmp::natpmp_parse_external) {
                        Ok(ip) => {
                            return Ok(ExternalIp { ip, class: classify_ipv4(ip), method: Method::NatPmp, gateway: IpAddr::V4(gw), local })
                        }
                        Err(e) => errs.push(format!("NAT-PMP : {e}")),
                    }
                }
                _ => errs.push("NAT-PMP : pas de passerelle par défaut".into()),
            }
        }
        Err(Error::NoIgd(errs.join(" ; ")))
    }

    /// Ouvre une redirection UDP `external_port_hint` → `local`. Si l'IP de `local` est 0.0.0.0,
    /// l'IP de l'interface qui mène à la passerelle est choisie. `lease` 0 = permanent.
    pub fn open(&self, local: SocketAddrV4, external_port_hint: u16, lease: Duration, description: &str) -> Result<Mapping> {
        if local.port() == 0 {
            return Err(Error::Protocol("port local 0 invalide".into()));
        }
        let r = self.route(*local.ip());
        let hint = if external_port_hint == 0 { local.port() } else { external_port_hint };
        let mut errs: Vec<String> = Vec::new();

        if self.try_upnp {
            match UpnpGateway::discover(r.local, self.discovery_timeout) {
                Ok(g) => match self.open_upnp(g, local, hint, lease, description, r.gateway) {
                    Ok(m) => return Ok(m),
                    // CGNAT/double NAT : PCP/NAT-PMP ne feront pas mieux.
                    Err(e @ Error::NoPublicIp { .. }) => return Err(e),
                    Err(e) => errs.push(format!("UPnP : {e}")),
                },
                Err(e) => errs.push(format!("UPnP : {e}")),
            }
        }
        if self.try_pmp {
            match (r.gateway, r.local) {
                (Some(gw), Some(lip)) => {
                    let local = SocketAddrV4::new(lip, local.port());
                    match self.open_pmp(gw, local, hint, lease, description) {
                        Ok(m) => return Ok(m),
                        Err(e @ Error::NoPublicIp { .. }) => return Err(e),
                        Err(e) => errs.push(e.to_string()),
                    }
                }
                _ => {
                    if errs.is_empty() {
                        return Err(Error::NoGateway);
                    }
                    errs.push("PCP/NAT-PMP : pas de passerelle par défaut".into());
                }
            }
        }
        Err(Error::NoIgd(errs.join(" ; ")))
    }

    fn check_public(&self, ip: Ipv4Addr, method: Method, warnings: &mut Vec<String>) -> Result<()> {
        let class = classify_ipv4(ip);
        if class == AddrClass::Public {
            return Ok(());
        }
        if self.allow_non_public && class != AddrClass::Unspecified {
            warnings.push(plan::explain_non_public(ip, class));
            return Ok(());
        }
        Err(Error::NoPublicIp { external: ip, class, method })
    }

    fn open_upnp(
        &self,
        g: UpnpGateway,
        local: SocketAddrV4,
        hint: u16,
        lease: Duration,
        description: &str,
        default_gw: Option<Ipv4Addr>,
    ) -> Result<Mapping> {
        let local = SocketAddrV4::new(if local.ip().is_unspecified() { g.local_ip } else { *local.ip() }, local.port());
        let mut warnings = Vec::new();
        if let (IpAddr::V4(igd), Some(def)) = (g.gateway_ip(), default_gw) {
            if igd != def {
                warnings.push(format!(
                    "la passerelle UPnP trouvée ({igd}) n'est pas la passerelle par défaut ({def}) : double NAT possible"
                ));
            }
        }
        let ext_ip = match g.external_ip().map_err(Error::Protocol)? {
            IpAddr::V4(ip) => ip,
            IpAddr::V6(ip) => return Err(Error::Protocol(format!("IP externe IPv6 inattendue {ip}"))),
        };
        self.check_public(ext_ip, Method::Upnp, &mut warnings)?;

        let ports = plan::candidate_ports(hint, self.sequential_ports, self.random_ports, &mut rand::thread_rng());
        let mut lease_s = lease_secs(lease);
        let mut desc = description.to_string();
        let mut tried_same = false;
        let mut i = 0;
        while i < ports.len() {
            let port = ports[i];
            // Entrée existante : à nous (reste d'un arrêt brutal) → on la remplace ; à une autre
            // machine → port suivant sans rien écraser.
            if let Ok(Some(e)) = g.specific(port) {
                let ours = e.internal_client == local.ip().to_string() && e.internal_port == local.port();
                if !ours {
                    i += 1;
                    continue;
                }
                let _ = g.remove(port);
            }
            match g.add(port, local, lease_s, &desc) {
                Ok(()) => {
                    let mut granted = Duration::from_secs(lease_s as u64);
                    // Bail réellement appliqué par la box (certaines l'ignorent ou le plafonnent).
                    if let Ok(Some(e)) = g.specific(port) {
                        if lease_s != 0 && e.lease_secs == 0 {
                            warnings.push("la box a rendu la redirection permanente (bail ignoré)".into());
                            granted = Duration::ZERO;
                        } else if e.lease_secs != 0 && e.lease_secs + 5 < lease_s {
                            granted = Duration::from_secs(e.lease_secs as u64);
                        }
                    }
                    if lease_s == 0 && !lease.is_zero() {
                        warnings.push("la box n'accepte que les baux permanents : redirection permanente, supprimée à l'arrêt".into());
                    }
                    return Ok(Mapping {
                        external: SocketAddr::new(IpAddr::V4(ext_ip), port),
                        local,
                        protocol: Protocol::Udp,
                        lease: granted,
                        method: Method::Upnp,
                        gateway: g.gateway_ip(),
                        warnings,
                        renewed_at: Instant::now(),
                        description: desc,
                        requested_lease: lease,
                        backend: Backend::Upnp(g),
                        closed: false,
                    });
                }
                Err(igd_next::AddPortError::OnlyPermanentLeasesSupported) if lease_s != 0 => {
                    lease_s = 0; // même port, bail permanent
                }
                Err(igd_next::AddPortError::DescriptionTooLong) if desc.len() > 8 => {
                    desc = desc.chars().take(8).collect();
                }
                Err(igd_next::AddPortError::PortInUse) => i += 1,
                Err(igd_next::AddPortError::SamePortValuesRequired) if !tried_same => {
                    tried_same = true;
                    // La box exige externe == interne : seul ce port est possible.
                    if port == local.port() {
                        return Err(Error::NoPortAvailable);
                    }
                    let Some(pos) = ports.iter().position(|&p| p == local.port()) else {
                        return self.open_upnp_single(g, local, ext_ip, lease_s, lease, &desc, warnings);
                    };
                    i = pos;
                }
                Err(igd_next::AddPortError::ActionNotAuthorized) => {
                    return Err(Error::NotAuthorized("UPnP en lecture seule ou redirections interdites dans la box".into()))
                }
                Err(e) => return Err(Error::Protocol(format!("AddPortMapping : {e}"))),
            }
        }
        Err(Error::NoPortAvailable)
    }

    #[allow(clippy::too_many_arguments)]
    fn open_upnp_single(
        &self,
        g: UpnpGateway,
        local: SocketAddrV4,
        ext_ip: Ipv4Addr,
        lease_s: u32,
        lease: Duration,
        desc: &str,
        warnings: Vec<String>,
    ) -> Result<Mapping> {
        g.add(local.port(), local, lease_s, desc).map_err(|e| match e {
            igd_next::AddPortError::PortInUse => Error::NoPortAvailable,
            e => Error::Protocol(format!("AddPortMapping : {e}")),
        })?;
        Ok(Mapping {
            external: SocketAddr::new(IpAddr::V4(ext_ip), local.port()),
            local,
            protocol: Protocol::Udp,
            lease: Duration::from_secs(lease_s as u64),
            method: Method::Upnp,
            gateway: g.gateway_ip(),
            warnings,
            renewed_at: Instant::now(),
            description: desc.to_string(),
            requested_lease: lease,
            backend: Backend::Upnp(g),
            closed: false,
        })
    }

    fn open_pmp(&self, gw: Ipv4Addr, local: SocketAddrV4, hint: u16, lease: Duration, description: &str) -> Result<Mapping> {
        // PCP et NAT-PMP n'ont pas de bail permanent : 0 = suppression. On demande 2 h max.
        let lease_s = if lease.is_zero() { 7200 } else { lease_secs(lease) };
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let t = self.discovery_timeout;
        let mut warnings = Vec::new();
        let pcp_err = match pmp::exchange(
            *local.ip(),
            gw,
            &pmp::pcp_map_request(*local.ip(), nonce, local.port(), hint, Ipv4Addr::UNSPECIFIED, lease_s),
            t,
            |b| pmp::pcp_parse_map(b, &nonce),
        ) {
            Ok((granted, port, ip)) => {
                let mut m = Mapping {
                    external: SocketAddr::new(IpAddr::V4(ip), port),
                    local,
                    protocol: Protocol::Udp,
                    lease: Duration::from_secs(granted as u64),
                    method: Method::Pcp,
                    gateway: IpAddr::V4(gw),
                    warnings: Vec::new(),
                    renewed_at: Instant::now(),
                    description: description.into(),
                    requested_lease: lease,
                    backend: Backend::Pcp { gateway: gw, nonce },
                    closed: false,
                };
                if let Err(e) = self.check_public(ip, Method::Pcp, &mut warnings) {
                    let _ = m.close();
                    return Err(e);
                }
                m.warnings = warnings;
                return Ok(m);
            }
            Err(e) => e,
        };
        // NAT-PMP : IP externe, puis redirection (la box choisit le port si le nôtre est pris).
        let ip = pmp::exchange(*local.ip(), gw, &pmp::natpmp_external_request(), t, pmp::natpmp_parse_external)
            .map_err(|e| Error::Protocol(format!("PCP : {pcp_err} ; NAT-PMP : {e}")))?;
        self.check_public(ip, Method::NatPmp, &mut warnings)?;
        let (_, port, granted) = pmp::exchange(*local.ip(), gw, &pmp::natpmp_map_request(local.port(), hint, lease_s), t, pmp::natpmp_parse_map)
            .map_err(|e| match e {
                pmp::PmpError::Result(2, m) => Error::NotAuthorized(format!("NAT-PMP : {m}")),
                e => Error::Protocol(format!("NAT-PMP : {e}")),
            })?;
        Ok(Mapping {
            external: SocketAddr::new(IpAddr::V4(ip), port),
            local,
            protocol: Protocol::Udp,
            lease: Duration::from_secs(granted as u64),
            method: Method::NatPmp,
            gateway: IpAddr::V4(gw),
            warnings,
            renewed_at: Instant::now(),
            description: description.into(),
            requested_lease: lease,
            backend: Backend::NatPmp { gateway: gw },
            closed: false,
        })
    }
}

impl Mapping {
    /// Délai conseillé avant le prochain [`Mapping::renew`] (mi-vie ; re-vérification périodique
    /// pour un bail permanent).
    pub fn renew_in(&self) -> Duration {
        plan::renew_delay(self.lease)
    }

    /// Temps restant avant expiration (`None` si permanent).
    pub fn remaining(&self) -> Option<Duration> {
        (!self.lease.is_zero()).then(|| self.lease.saturating_sub(self.renewed_at.elapsed()))
    }

    /// Renouvelle le bail (même port externe si possible). L'IP externe est relue : si elle a
    /// changé, `external` est mis à jour (les invitations déjà émises deviennent caduques).
    pub fn renew(&mut self) -> Result<()> {
        if self.closed {
            return Err(Error::Closed);
        }
        let port = self.external.port();
        match &self.backend {
            Backend::Upnp(g) => {
                if let Ok(Some(e)) = g.specific(port) {
                    if e.internal_client != self.local.ip().to_string() || e.internal_port != self.local.port() {
                        return Err(Error::PortTaken(port));
                    }
                }
                let lease_s = lease_secs(self.lease);
                g.add(port, self.local, lease_s, &self.description).map_err(|e| match e {
                    igd_next::AddPortError::PortInUse => Error::PortTaken(port),
                    igd_next::AddPortError::ActionNotAuthorized => Error::NotAuthorized("UPnP".into()),
                    e => Error::Protocol(format!("AddPortMapping : {e}")),
                })?;
                if let Ok(IpAddr::V4(ip)) = g.external_ip() {
                    self.external = SocketAddr::new(IpAddr::V4(ip), port);
                }
            }
            Backend::Pcp { gateway, nonce } => {
                let lease_s = if self.requested_lease.is_zero() { 7200 } else { lease_secs(self.requested_lease) };
                let ip = match self.external.ip() {
                    IpAddr::V4(ip) => ip,
                    _ => Ipv4Addr::UNSPECIFIED,
                };
                let req = pmp::pcp_map_request(*self.local.ip(), *nonce, self.local.port(), port, ip, lease_s);
                let (granted, p, ip) = pmp::exchange(*self.local.ip(), *gateway, &req, Duration::from_secs(3), |b| pmp::pcp_parse_map(b, nonce))
                    .map_err(|e| Error::Protocol(format!("PCP : {e}")))?;
                self.external = SocketAddr::new(IpAddr::V4(ip), p);
                self.lease = Duration::from_secs(granted as u64);
            }
            Backend::NatPmp { gateway } => {
                let lease_s = if self.requested_lease.is_zero() { 7200 } else { lease_secs(self.requested_lease) };
                let t = Duration::from_secs(3);
                let (_, p, granted) =
                    pmp::exchange(*self.local.ip(), *gateway, &pmp::natpmp_map_request(self.local.port(), port, lease_s), t, pmp::natpmp_parse_map)
                        .map_err(|e| Error::Protocol(format!("NAT-PMP : {e}")))?;
                let ip = pmp::exchange(*self.local.ip(), *gateway, &pmp::natpmp_external_request(), t, pmp::natpmp_parse_external)
                    .ok()
                    .map(IpAddr::V4)
                    .unwrap_or(self.external.ip());
                self.external = SocketAddr::new(ip, p);
                self.lease = Duration::from_secs(granted as u64);
            }
        }
        self.renewed_at = Instant::now();
        Ok(())
    }

    /// Supprime la redirection sur la box. Idempotent.
    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let port = self.external.port();
        match &self.backend {
            Backend::Upnp(g) => {
                // Ne supprime que si l'entrée est toujours la nôtre.
                if let Ok(Some(e)) = g.specific(port) {
                    if e.internal_client != self.local.ip().to_string() || e.internal_port != self.local.port() {
                        return Ok(());
                    }
                }
                g.remove(port).map(|_| ()).map_err(Error::Protocol)
            }
            Backend::Pcp { gateway, nonce } => {
                let req = pmp::pcp_map_request(*self.local.ip(), *nonce, self.local.port(), 0, Ipv4Addr::UNSPECIFIED, 0);
                pmp::exchange(*self.local.ip(), *gateway, &req, Duration::from_secs(2), |b| pmp::pcp_parse_map(b, nonce))
                    .map(|_| ())
                    .map_err(|e| Error::Protocol(format!("PCP : {e}")))
            }
            Backend::NatPmp { gateway } => {
                pmp::exchange(*self.local.ip(), *gateway, &pmp::natpmp_map_request(self.local.port(), 0, 0), Duration::from_secs(2), pmp::natpmp_parse_map)
                    .map(|_| ())
                    .map_err(|e| Error::Protocol(format!("NAT-PMP : {e}")))
            }
        }
    }

    /// Accès à la passerelle UPnP (diagnostic : liste/entrée spécifique).
    pub fn upnp_gateway(&self) -> Option<&UpnpGateway> {
        match &self.backend {
            Backend::Upnp(g) => Some(g),
            _ => None,
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

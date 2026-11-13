//! UPnP IGD : découverte et opérations via `igd-next` (synchrone), plus une petite requête SOAP
//! maison pour `GetSpecificPortMappingEntry` (absente d'igd-next), avec délais bornés.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

use igd_next::{PortMappingProtocol, SearchOptions};

/// Entrée de la table de redirections de la passerelle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpnpEntry {
    pub external_port: u16,
    pub protocol: String,
    pub internal_client: String,
    pub internal_port: u16,
    pub enabled: bool,
    pub description: String,
    /// Durée restante annoncée par la box (0 = permanent).
    pub lease_secs: u32,
}

/// Passerelle UPnP IGD découverte.
#[derive(Clone, Debug)]
pub struct UpnpGateway {
    pub(crate) gw: igd_next::Gateway,
    /// IP locale (interface) utilisée pour la découverte et comme client interne des redirections.
    pub local_ip: Ipv4Addr,
}

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);

impl UpnpGateway {
    /// Découverte SSDP depuis `local_ip` (ou toutes interfaces si `None`), `timeout` maximum.
    pub fn discover(local_ip: Option<Ipv4Addr>, timeout: Duration) -> Result<Self, String> {
        let bind_ip = local_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let opts = SearchOptions {
            bind_addr: SocketAddr::new(IpAddr::V4(bind_ip), 0),
            timeout: Some(timeout),
            single_search_timeout: Some(timeout),
            ..Default::default()
        };
        let gw = igd_next::search_gateway(opts).map_err(|e| match e {
            igd_next::SearchError::NoResponseWithinTimeout => {
                format!("aucune passerelle UPnP IGD n'a répondu en {} s (UPnP désactivé sur la box ?)", timeout.as_secs_f32())
            }
            igd_next::SearchError::IoError(ref io) if matches!(io.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                format!("aucune passerelle UPnP IGD n'a répondu en {} s (UPnP désactivé sur la box ?)", timeout.as_secs_f32())
            }
            e => format!("découverte UPnP : {e}"),
        })?;
        let local_ip = match (local_ip, gw.addr.ip()) {
            (Some(l), _) => l,
            (None, IpAddr::V4(g)) => crate::netif::local_ip_towards(g).ok_or("aucune IP locale vers la passerelle")?,
            (None, IpAddr::V6(_)) => return Err("passerelle UPnP IPv6 non prise en charge".into()),
        };
        Ok(Self { gw, local_ip })
    }

    /// Adresse (IP:port HTTP) de la passerelle.
    pub fn addr(&self) -> SocketAddr {
        self.gw.addr
    }

    pub fn gateway_ip(&self) -> IpAddr {
        self.gw.addr.ip()
    }

    pub fn external_ip(&self) -> Result<IpAddr, String> {
        self.gw.get_external_ip().map_err(|e| format!("UPnP GetExternalIPAddress : {e}"))
    }

    pub(crate) fn add(&self, ext: u16, local: SocketAddrV4, lease_s: u32, desc: &str) -> Result<(), igd_next::AddPortError> {
        self.gw.add_port(PortMappingProtocol::UDP, ext, SocketAddr::V4(local), lease_s, desc)
    }

    /// Supprime la redirection UDP `external_port`. `Ok(false)` si elle n'existait pas.
    pub fn remove(&self, external_port: u16) -> Result<bool, String> {
        match self.gw.remove_port(PortMappingProtocol::UDP, external_port) {
            Ok(()) => Ok(true),
            Err(igd_next::RemovePortError::NoSuchPortMapping) => Ok(false),
            Err(e) => Err(format!("UPnP DeletePortMapping : {e}")),
        }
    }

    /// Liste les redirections (tous protocoles), jusqu'à `max` entrées.
    pub fn list(&self, max: u32) -> Vec<UpnpEntry> {
        let mut v = Vec::new();
        for i in 0..max {
            match self.gw.get_generic_port_mapping_entry(i) {
                Ok(e) => v.push(UpnpEntry {
                    external_port: e.external_port,
                    protocol: format!("{:?}", e.protocol),
                    internal_client: e.internal_client,
                    internal_port: e.internal_port,
                    enabled: e.enabled,
                    description: e.port_mapping_description,
                    lease_secs: e.lease_duration,
                }),
                Err(_) => break,
            }
        }
        v
    }

    /// `GetSpecificPortMappingEntry` pour UDP `external_port` : `Ok(None)` si absente.
    pub fn specific(&self, external_port: u16) -> Result<Option<UpnpEntry>, String> {
        let mut last = String::new();
        for service in ["urn:schemas-upnp-org:service:WANIPConnection:1", "urn:schemas-upnp-org:service:WANPPPConnection:1"] {
            let body = format!(
                "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
                 s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
                 <u:GetSpecificPortMappingEntry xmlns:u=\"{service}\"><NewRemoteHost></NewRemoteHost>\
                 <NewExternalPort>{external_port}</NewExternalPort><NewProtocol>UDP</NewProtocol>\
                 </u:GetSpecificPortMappingEntry></s:Body></s:Envelope>"
            );
            let resp = match soap(self.gw.addr, &self.gw.control_url, &format!("{service}#GetSpecificPortMappingEntry"), &body) {
                Ok(r) => r,
                Err(e) => return Err(format!("UPnP GetSpecificPortMappingEntry : {e}")),
            };
            if let Some(code) = tag(&resp, "errorCode") {
                match code.trim() {
                    "714" => return Ok(None), // NoSuchEntryInArray
                    "401" => {
                        last = "action invalide (401)".into();
                        continue; // mauvais type de service : essayer PPP
                    }
                    c => return Err(format!("UPnP GetSpecificPortMappingEntry : erreur {c}")),
                }
            }
            let client = tag(&resp, "NewInternalClient").ok_or("réponse sans NewInternalClient")?;
            return Ok(Some(UpnpEntry {
                external_port,
                protocol: "UDP".into(),
                internal_client: client.trim().to_string(),
                internal_port: tag(&resp, "NewInternalPort").and_then(|p| p.trim().parse().ok()).unwrap_or(0),
                enabled: tag(&resp, "NewEnabled").map(|e| e.trim() == "1").unwrap_or(false),
                description: tag(&resp, "NewPortMappingDescription").unwrap_or_default(),
                lease_secs: tag(&resp, "NewLeaseDuration").and_then(|p| p.trim().parse().ok()).unwrap_or(0),
            }));
        }
        Err(format!("UPnP GetSpecificPortMappingEntry : {last}"))
    }
}

/// Requête SOAP HTTP/1.0 (pas de chunked), délais de 3 s. Renvoie le corps.
fn soap(addr: SocketAddr, path: &str, action: &str, body: &str) -> std::io::Result<String> {
    let mut s = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)?;
    s.set_read_timeout(Some(HTTP_TIMEOUT))?;
    s.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let req = format!(
        "POST {path} HTTP/1.0\r\nHost: {addr}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\n\
         SOAPAction: \"{action}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 256 * 1024 {
                    break;
                }
            }
            Err(e) if !out.is_empty() && matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
            Err(e) => return Err(e),
        }
    }
    let text = String::from_utf8_lossy(&out).into_owned();
    Ok(text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or(text))
}

/// Contenu texte de la première balise `<name>` (préfixe d'espace de noms toléré).
pub(crate) fn tag(xml: &str, name: &str) -> Option<String> {
    let mut i = 0;
    while let Some(p) = xml[i..].find('<') {
        let start = i + p + 1;
        let end = start + xml[start..].find('>')?;
        let t = &xml[start..end];
        let local = t.split_whitespace().next().unwrap_or("");
        let local = local.rsplit(':').next().unwrap_or(local);
        if local == name && !t.starts_with('/') && !t.ends_with('/') {
            let rest = &xml[end + 1..];
            let close = rest.find("</")?;
            return Some(rest[..close].to_string());
        }
        i = end + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_extraction() {
        let x = "<s:Envelope><s:Body><u:R xmlns:u=\"x\"><NewInternalClient>192.168.1.20</NewInternalClient>\
                 <NewEnabled>1</NewEnabled><NewPortMappingDescription/><NewLeaseDuration>118</NewLeaseDuration></u:R></s:Body></s:Envelope>";
        assert_eq!(tag(x, "NewInternalClient").as_deref(), Some("192.168.1.20"));
        assert_eq!(tag(x, "NewLeaseDuration").as_deref(), Some("118"));
        assert_eq!(tag(x, "NewPortMappingDescription"), None);
        assert_eq!(tag(x, "Absent"), None);
        let f = "<s:Fault><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>714</errorCode></UPnPError></detail></s:Fault>";
        assert_eq!(tag(f, "errorCode").as_deref(), Some("714"));
    }
}

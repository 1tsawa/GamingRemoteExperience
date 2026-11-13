//! NAT-PMP (RFC 6886) et PCP (RFC 6887, opcode MAP), implémentés à la main : quelques octets
//! en UDP vers la passerelle, port 5351.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

pub const PORT: u16 = 5351;
const PROTO_UDP: u8 = 17;

#[derive(Debug)]
pub enum PmpError {
    /// Aucun serveur (pas de réponse ou ICMP « port inaccessible »).
    NoServer,
    /// Le serveur ne parle pas cette version du protocole.
    UnsupportedVersion,
    /// Code de résultat non nul.
    Result(u16, &'static str),
    Malformed,
    Io(std::io::Error),
}

impl std::fmt::Display for PmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PmpError::NoServer => f.write_str("pas de réponse de la passerelle"),
            PmpError::UnsupportedVersion => f.write_str("version non prise en charge par la passerelle"),
            PmpError::Result(c, m) => write!(f, "refus de la passerelle (code {c} : {m})"),
            PmpError::Malformed => f.write_str("réponse malformée"),
            PmpError::Io(e) => write!(f, "{e}"),
        }
    }
}

// ───────────────────────────── NAT-PMP ─────────────────────────────

pub fn natpmp_result_text(code: u16) -> &'static str {
    match code {
        1 => "version non prise en charge",
        2 => "non autorisé (NAT-PMP désactivé ?)",
        3 => "panne réseau (WAN sans IP ?)",
        4 => "plus de ressources",
        5 => "opcode non pris en charge",
        _ => "inconnu",
    }
}

pub fn natpmp_external_request() -> [u8; 2] {
    [0, 0]
}

pub fn natpmp_map_request(internal: u16, suggested_external: u16, lifetime_s: u32) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[1] = 1; // opcode 1 = UDP
    b[4..6].copy_from_slice(&internal.to_be_bytes());
    b[6..8].copy_from_slice(&suggested_external.to_be_bytes());
    b[8..12].copy_from_slice(&lifetime_s.to_be_bytes());
    b
}

fn natpmp_check(b: &[u8], op: u8, len: usize) -> Result<(), PmpError> {
    if b.len() < 4 {
        return Err(PmpError::Malformed);
    }
    if b[0] != 0 {
        return Err(PmpError::UnsupportedVersion);
    }
    if b[1] != op {
        return Err(PmpError::Malformed);
    }
    let code = u16::from_be_bytes([b[2], b[3]]);
    if code != 0 {
        return Err(if code == 1 { PmpError::UnsupportedVersion } else { PmpError::Result(code, natpmp_result_text(code)) });
    }
    if b.len() < len {
        return Err(PmpError::Malformed);
    }
    Ok(())
}

pub fn natpmp_parse_external(b: &[u8]) -> Result<Ipv4Addr, PmpError> {
    natpmp_check(b, 128, 12)?;
    Ok(Ipv4Addr::new(b[8], b[9], b[10], b[11]))
}

/// (port interne, port externe accordé, durée accordée en s)
pub fn natpmp_parse_map(b: &[u8]) -> Result<(u16, u16, u32), PmpError> {
    natpmp_check(b, 129, 16)?;
    Ok((
        u16::from_be_bytes([b[8], b[9]]),
        u16::from_be_bytes([b[10], b[11]]),
        u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
    ))
}

// ───────────────────────────── PCP ─────────────────────────────

pub fn pcp_result_text(code: u8) -> &'static str {
    match code {
        1 => "UNSUPP_VERSION",
        2 => "NOT_AUTHORIZED",
        3 => "MALFORMED_REQUEST",
        4 => "UNSUPP_OPCODE",
        5 => "UNSUPP_OPTION",
        6 => "MALFORMED_OPTION",
        7 => "NETWORK_FAILURE",
        8 => "NO_RESOURCES",
        9 => "UNSUPP_PROTOCOL",
        10 => "USER_EX_QUOTA",
        11 => "CANNOT_PROVIDE_EXTERNAL",
        12 => "ADDRESS_MISMATCH",
        13 => "EXCESSIVE_REMOTE_PEERS",
        _ => "inconnu",
    }
}

/// Requête MAP (60 octets).
pub fn pcp_map_request(
    client: Ipv4Addr,
    nonce: [u8; 12],
    internal: u16,
    suggested_external: u16,
    suggested_ip: Ipv4Addr,
    lifetime_s: u32,
) -> [u8; 60] {
    let mut b = [0u8; 60];
    b[0] = 2;
    b[1] = 1; // R=0, opcode MAP
    b[4..8].copy_from_slice(&lifetime_s.to_be_bytes());
    b[8..24].copy_from_slice(&client.to_ipv6_mapped().octets());
    b[24..36].copy_from_slice(&nonce);
    b[36] = PROTO_UDP;
    b[40..42].copy_from_slice(&internal.to_be_bytes());
    b[42..44].copy_from_slice(&suggested_external.to_be_bytes());
    b[44..60].copy_from_slice(&suggested_ip.to_ipv6_mapped().octets());
    b
}

/// Réponse MAP : (durée accordée, port externe, IP externe).
pub fn pcp_parse_map(b: &[u8], nonce: &[u8; 12]) -> Result<(u32, u16, Ipv4Addr), PmpError> {
    if b.len() < 4 {
        return Err(PmpError::Malformed);
    }
    if b[0] != 2 {
        // Un serveur NAT-PMP seul répond en version 0 avec « version non prise en charge ».
        return Err(PmpError::UnsupportedVersion);
    }
    if b[1] != 0x81 {
        return Err(PmpError::Malformed);
    }
    if b[3] != 0 {
        return Err(if b[3] == 1 { PmpError::UnsupportedVersion } else { PmpError::Result(b[3] as u16, pcp_result_text(b[3])) });
    }
    if b.len() < 60 || &b[24..36] != nonce || b[36] != PROTO_UDP {
        return Err(PmpError::Malformed);
    }
    let lifetime = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    let port = u16::from_be_bytes([b[42], b[43]]);
    let ip6 = Ipv6Addr::from(<[u8; 16]>::try_from(&b[44..60]).unwrap());
    let ip = ip6.to_ipv4_mapped().ok_or(PmpError::Malformed)?;
    Ok((lifetime, port, ip))
}

// ───────────────────────────── E/S ─────────────────────────────

/// Envoie `req` à `gateway:5351` depuis `local:0` avec retransmissions (250 ms, 500 ms, 1 s...)
/// tant que `budget` le permet ; renvoie la première réponse acceptée par `accept`.
pub fn exchange<T>(
    local: Ipv4Addr,
    gateway: Ipv4Addr,
    req: &[u8],
    budget: Duration,
    accept: impl Fn(&[u8]) -> Result<T, PmpError>,
) -> Result<T, PmpError> {
    let s = UdpSocket::bind(SocketAddrV4::new(local, 0)).map_err(PmpError::Io)?;
    s.connect(SocketAddrV4::new(gateway, PORT)).map_err(PmpError::Io)?;
    let deadline = Instant::now() + budget;
    let mut wait = Duration::from_millis(250);
    let mut buf = [0u8; 1100];
    let mut last_err = PmpError::NoServer;
    while Instant::now() < deadline {
        s.send(req).map_err(PmpError::Io)?;
        let until = (Instant::now() + wait).min(deadline);
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            s.set_read_timeout(Some(left)).map_err(PmpError::Io)?;
            match s.recv(&mut buf) {
                Ok(n) => match accept(&buf[..n]) {
                    Ok(v) => return Ok(v),
                    Err(PmpError::Malformed) => last_err = PmpError::Malformed, // autre réponse : on attend
                    Err(e) => return Err(e),
                },
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                // ICMP « port inaccessible » : personne n'écoute sur 5351.
                Err(e) if e.kind() == ErrorKind::ConnectionReset || e.raw_os_error() == Some(10054) => {
                    return Err(PmpError::NoServer)
                }
                Err(e) => return Err(PmpError::Io(e)),
            }
        }
        wait *= 2;
    }
    Err(last_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natpmp_roundtrip() {
        let r = natpmp_map_request(7777, 7778, 3600);
        assert_eq!(r, [0, 1, 0, 0, 0x1e, 0x61, 0x1e, 0x62, 0, 0, 0x0e, 0x10]);
        let resp = [0, 129, 0, 0, 0, 0, 0, 9, 0x1e, 0x61, 0x1e, 0x62, 0, 0, 0x07, 0x08];
        assert_eq!(natpmp_parse_map(&resp).unwrap(), (7777, 7778, 1800));
        let ext = [0, 128, 0, 0, 0, 0, 0, 1, 82, 64, 1, 2];
        assert_eq!(natpmp_parse_external(&ext).unwrap(), Ipv4Addr::new(82, 64, 1, 2));
        assert!(matches!(natpmp_parse_external(&[0, 128, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]), Err(PmpError::Result(2, _))));
        assert!(matches!(natpmp_parse_external(&[2, 128, 0, 0]), Err(PmpError::UnsupportedVersion)));
        assert!(matches!(natpmp_parse_map(&[0, 129, 0, 0, 0]), Err(PmpError::Malformed)));
    }

    #[test]
    fn pcp_roundtrip() {
        let nonce = [7u8; 12];
        let req = pcp_map_request(Ipv4Addr::new(192, 168, 1, 20), nonce, 7777, 7777, Ipv4Addr::UNSPECIFIED, 3600);
        assert_eq!(req[0], 2);
        assert_eq!(req[1], 1);
        assert_eq!(&req[8..24], &Ipv4Addr::new(192, 168, 1, 20).to_ipv6_mapped().octets());
        assert_eq!(&req[44..60], &Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets());
        // Réponse construite à partir de la requête.
        let mut resp = req;
        resp[1] = 0x81;
        resp[3] = 0;
        resp[4..8].copy_from_slice(&600u32.to_be_bytes());
        resp[8..24].fill(0);
        resp[42..44].copy_from_slice(&40000u16.to_be_bytes());
        resp[44..60].copy_from_slice(&Ipv4Addr::new(82, 64, 1, 2).to_ipv6_mapped().octets());
        assert_eq!(pcp_parse_map(&resp, &nonce).unwrap(), (600, 40000, Ipv4Addr::new(82, 64, 1, 2)));
        assert!(matches!(pcp_parse_map(&resp, &[0u8; 12]), Err(PmpError::Malformed)), "nonce différent");
        let mut refused = resp;
        refused[3] = 2;
        assert!(matches!(pcp_parse_map(&refused, &nonce), Err(PmpError::Result(2, "NOT_AUTHORIZED"))));
        // Serveur NAT-PMP seul : réponse v0 « unsupported version ».
        assert!(matches!(pcp_parse_map(&[0, 0x81, 0, 1], &nonce), Err(PmpError::UnsupportedVersion)));
    }

    #[test]
    fn no_server_is_detected_quickly() {
        // Personne n'écoute sur 127.0.0.1:5351 (normalement) : ICMP ou délai court.
        let t = Instant::now();
        let r = exchange(Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST, &natpmp_external_request(), Duration::from_millis(600), natpmp_parse_external);
        assert!(r.is_err());
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}

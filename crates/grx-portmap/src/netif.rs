//! Énumération des interfaces locales (aucune requête réseau) : passerelle IPv4 par défaut,
//! IP locale qui y mène, adresses IPv6 globales.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Adresse IPv6 globale d'une interface locale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ipv6Global {
    pub addr: Ipv6Addr,
    /// Adresse temporaire (extensions de confidentialité) : change régulièrement, à éviter dans
    /// une invitation durable.
    pub temporary: bool,
    pub interface: String,
}

/// Passerelle IPv4 par défaut (interface active de plus petite métrique).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefaultRoute {
    pub gateway: Ipv4Addr,
    /// IP de cette machine sur l'interface qui mène à la passerelle.
    pub local: Ipv4Addr,
}

/// IP locale choisie par la table de routage pour joindre `dest` : `connect` UDP ne fait qu'une
/// résolution de route, aucun paquet n'est émis.
pub fn local_ip_towards(dest: Ipv4Addr) -> Option<Ipv4Addr> {
    let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    s.connect((dest, 9)).ok()?;
    match s.local_addr().ok()? {
        SocketAddr::V4(a) if !a.ip().is_unspecified() => Some(*a.ip()),
        _ => None,
    }
}

/// `true` si l'adresse IPv6 est globale unicast (2000::/3).
pub fn is_global_ipv6(a: &Ipv6Addr) -> bool {
    (a.segments()[0] & 0xe000) == 0x2000
}

#[cfg(windows)]
mod imp {
    use super::*;
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows_sys::Win32::Networking::WinSock::{
        IpDadStatePreferred, IpSuffixOriginRandom, AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKET_ADDRESS,
    };

    pub(super) struct Adapter {
        pub name: String,
        pub metric4: u32,
        pub gateways4: Vec<Ipv4Addr>,
        pub v4: Vec<Ipv4Addr>,
        pub v6: Vec<(Ipv6Addr, bool /*temporaire*/)>,
    }

    unsafe fn sockaddr(sa: &SOCKET_ADDRESS) -> Option<IpAddr> {
        if sa.lpSockaddr.is_null() {
            return None;
        }
        let p = sa.lpSockaddr as *const u8;
        let family = (*(sa.lpSockaddr as *const SOCKADDR)).sa_family;
        if family == AF_INET && sa.iSockaddrLength >= 8 {
            let b = std::slice::from_raw_parts(p.add(4), 4);
            Some(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3])))
        } else if family == AF_INET6 && sa.iSockaddrLength >= 24 {
            let b: [u8; 16] = std::slice::from_raw_parts(p.add(8), 16).try_into().ok()?;
            Some(IpAddr::V6(Ipv6Addr::from(b)))
        } else {
            None
        }
    }

    unsafe fn wide(p: *const u16) -> String {
        if p.is_null() {
            return String::new();
        }
        let mut n = 0;
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }

    pub(super) fn adapters() -> Vec<Adapter> {
        let flags = GAA_FLAG_INCLUDE_GATEWAYS | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;
        let mut size: u32 = 32 * 1024;
        let mut buf: Vec<u64> = Vec::new(); // u64 : alignement suffisant pour la structure
        for _ in 0..4 {
            buf.resize((size as usize).div_ceil(8), 0);
            let r = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    flags,
                    std::ptr::null(),
                    buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH,
                    &mut size,
                )
            };
            if r == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if r != NO_ERROR {
                return Vec::new();
            }
            let mut out = Vec::new();
            let mut cur = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            unsafe {
                while !cur.is_null() {
                    let a = &*cur;
                    cur = a.Next;
                    if a.OperStatus != IfOperStatusUp || a.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
                        continue;
                    }
                    let mut ad = Adapter {
                        name: wide(a.FriendlyName),
                        metric4: a.Ipv4Metric,
                        gateways4: Vec::new(),
                        v4: Vec::new(),
                        v6: Vec::new(),
                    };
                    let mut g = a.FirstGatewayAddress;
                    while !g.is_null() {
                        if let Some(IpAddr::V4(ip)) = sockaddr(&(*g).Address) {
                            if !ip.is_unspecified() {
                                ad.gateways4.push(ip);
                            }
                        }
                        g = (*g).Next;
                    }
                    let mut u = a.FirstUnicastAddress;
                    while !u.is_null() {
                        let ua = &*u;
                        match sockaddr(&ua.Address) {
                            Some(IpAddr::V4(ip)) => ad.v4.push(ip),
                            Some(IpAddr::V6(ip)) if ua.DadState == IpDadStatePreferred => {
                                // Windows : l'adresse temporaire (RFC 4941) a un suffixe « Random »
                                // et une durée de validité courte ; l'adresse stable a un suffixe
                                // « Link » (ou Random sans durée courte si randomizeidentifiers).
                                let temporary = ua.SuffixOrigin == IpSuffixOriginRandom
                                    && ua.ValidLifetime != u32::MAX
                                    && ua.PreferredLifetime <= 7 * 24 * 3600;
                                ad.v6.push((ip, temporary));
                            }
                            _ => {}
                        }
                        u = ua.Next;
                    }
                    out.push(ad);
                }
            }
            return out;
        }
        Vec::new()
    }
}

/// Passerelle IPv4 par défaut et IP locale associée.
#[cfg(windows)]
pub fn default_route_v4() -> Option<DefaultRoute> {
    let mut best: Option<(u32, Ipv4Addr)> = None;
    for a in imp::adapters() {
        if let Some(gw) = a.gateways4.first() {
            if best.is_none_or(|(m, _)| a.metric4 < m) {
                best = Some((a.metric4, *gw));
            }
        }
    }
    let gateway = best?.1;
    let local = local_ip_towards(gateway)?;
    Some(DefaultRoute { gateway, local })
}

#[cfg(not(windows))]
pub fn default_route_v4() -> Option<DefaultRoute> {
    None
}

/// Adresses IPv6 globales (2000::/3) des interfaces actives ; les adresses stables d'abord.
#[cfg(windows)]
pub fn global_ipv6() -> Vec<Ipv6Global> {
    let mut v: Vec<Ipv6Global> = imp::adapters()
        .into_iter()
        .flat_map(|a| {
            let name = a.name.clone();
            a.v6.into_iter()
                .filter(|(ip, _)| is_global_ipv6(ip))
                .map(move |(addr, temporary)| Ipv6Global { addr, temporary, interface: name.clone() })
        })
        .collect();
    v.sort_by_key(|g| g.temporary);
    v.dedup_by_key(|g| g.addr);
    v
}

#[cfg(not(windows))]
pub fn global_ipv6() -> Vec<Ipv6Global> {
    Vec::new()
}

/// IPv4 locales des interfaces actives (diagnostic).
#[cfg(windows)]
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    imp::adapters().into_iter().flat_map(|a| a.v4).collect()
}

#[cfg(not(windows))]
pub fn local_ipv4s() -> Vec<Ipv4Addr> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_v6_classification() {
        assert!(is_global_ipv6(&"2a01:e0a::1".parse().unwrap()));
        assert!(is_global_ipv6(&"2001:861::1".parse().unwrap()));
        assert!(!is_global_ipv6(&"fe80::1".parse().unwrap()));
        assert!(!is_global_ipv6(&"fd00::1".parse().unwrap()));
        assert!(!is_global_ipv6(&"::1".parse().unwrap()));
    }

    #[test]
    fn enumeration_does_not_panic() {
        let _ = default_route_v4();
        let _ = global_ipv6();
        assert!(local_ip_towards(Ipv4Addr::LOCALHOST).is_some());
    }
}

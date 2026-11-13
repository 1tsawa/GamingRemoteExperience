//! Logique pure (testée) : classification d'adresses, choix des ports, planification des baux.

use std::net::Ipv4Addr;
use std::time::Duration;

use rand::Rng;

/// Nature d'une adresse IPv4 vue comme « IP externe » de la box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrClass {
    /// Routable sur Internet : la redirection de port suffit.
    Public,
    /// 10/8, 172.16/12, 192.168/16 : la box est elle-même derrière un autre routeur (double NAT).
    Private,
    /// 100.64/10 : NAT de l'opérateur (CGNAT), aucune redirection possible côté abonné.
    Cgnat,
    /// 0.0.0.0 : WAN non connecté ou IP inconnue.
    Unspecified,
    /// Boucle locale, lien local, multicast, réservé...
    Other,
}

pub fn classify_ipv4(ip: Ipv4Addr) -> AddrClass {
    let o = ip.octets();
    if ip.is_unspecified() {
        AddrClass::Unspecified
    } else if o[0] == 10 || (o[0] == 172 && (16..=31).contains(&o[1])) || (o[0] == 192 && o[1] == 168) {
        AddrClass::Private
    } else if o[0] == 100 && (64..=127).contains(&o[1]) {
        AddrClass::Cgnat
    } else if ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (18..=19).contains(&o[1]))
    {
        AddrClass::Other
    } else {
        AddrClass::Public
    }
}

/// Explication pour l'utilisateur quand l'IP externe n'est pas publique.
pub fn explain_non_public(ip: Ipv4Addr, class: AddrClass) -> String {
    match class {
        AddrClass::Public => format!("{ip} est publique"),
        AddrClass::Cgnat => format!(
            "l'UPnP ne suffira pas, votre box n'a pas d'IP publique : son IP « externe » {ip} est dans 100.64.0.0/10 \
             (CGNAT de l'opérateur). Demandez une IPv4 publique/full-stack à votre FAI, utilisez l'IPv6, ou un VPN (Tailscale...)."
        ),
        AddrClass::Private => format!(
            "l'UPnP ne suffira pas, votre box n'a pas d'IP publique : son IP « externe » {ip} est privée (double NAT : \
             la box est derrière un autre routeur). Passez la box/le routeur amont en mode bridge, ou redirigez aussi \
             le port sur le routeur amont, ou utilisez l'IPv6 ou un VPN."
        ),
        AddrClass::Unspecified => {
            "la passerelle ne connaît pas son IP externe (0.0.0.0) : connexion Internet de la box coupée ?".to_string()
        }
        AddrClass::Other => format!("l'IP externe annoncée par la passerelle ({ip}) n'est pas routable sur Internet"),
    }
}

/// Plage des ports externes tirés au hasard quand le port souhaité et ses voisins sont pris.
pub const RANDOM_PORT_RANGE: std::ops::RangeInclusive<u16> = 20000..=60999;

/// Ports externes à essayer dans l'ordre : `hint`, `hint+1..=hint+sequential`, puis `random`
/// ports aléatoires distincts. Jamais 0, jamais de doublon, pas de débordement au-delà de 65535.
pub fn candidate_ports<R: Rng>(hint: u16, sequential: u16, random: usize, rng: &mut R) -> Vec<u16> {
    let mut v: Vec<u16> = Vec::with_capacity(1 + sequential as usize + random);
    if hint != 0 {
        v.push(hint);
        for i in 1..=sequential {
            match hint.checked_add(i) {
                Some(p) => v.push(p),
                None => break,
            }
        }
    }
    let mut guard = 0;
    let target = v.len() + random;
    while v.len() < target && guard < 10_000 {
        guard += 1;
        let p = rng.gen_range(RANDOM_PORT_RANGE);
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

/// Ré-affirmation périodique d'un bail permanent (la box a pu redémarrer et l'oublier).
pub const PERMANENT_RECHECK: Duration = Duration::from_secs(20 * 60);
/// Délai minimal entre deux renouvellements.
pub const MIN_RENEW: Duration = Duration::from_secs(5);

/// Quand renouveler un bail accordé de durée `lease` (0 = permanent) : à mi-vie.
pub fn renew_delay(lease: Duration) -> Duration {
    if lease.is_zero() {
        PERMANENT_RECHECK
    } else {
        (lease / 2).max(MIN_RENEW)
    }
}

/// Délai avant la nouvelle tentative n° `attempt` (0, 1, ...) après un échec de renouvellement :
/// 15 s, 30 s, 60 s... plafonné à 5 min, et à la moitié du temps restant avant expiration
/// (`remaining`, `None` si inconnu/permanent) pour avoir plusieurs essais avant la fin du bail.
pub fn retry_delay(attempt: u32, remaining: Option<Duration>) -> Duration {
    let base = Duration::from_secs(15).saturating_mul(1u32 << attempt.min(10));
    let mut d = base.min(Duration::from_secs(300));
    if let Some(r) = remaining {
        d = d.min(r / 2);
    }
    d.max(MIN_RENEW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{rngs::StdRng, SeedableRng};

    #[test]
    fn classifies_ipv4() {
        let c = |s: &str| classify_ipv4(s.parse().unwrap());
        assert_eq!(c("203.0.113.5"), AddrClass::Other); // documentation
        assert_eq!(c("82.64.1.2"), AddrClass::Public);
        assert_eq!(c("8.8.8.8"), AddrClass::Public);
        assert_eq!(c("10.0.0.1"), AddrClass::Private);
        assert_eq!(c("172.16.0.1"), AddrClass::Private);
        assert_eq!(c("172.31.255.255"), AddrClass::Private);
        assert_eq!(c("172.32.0.1"), AddrClass::Public);
        assert_eq!(c("172.15.0.1"), AddrClass::Public);
        assert_eq!(c("192.168.1.254"), AddrClass::Private);
        assert_eq!(c("100.64.0.1"), AddrClass::Cgnat);
        assert_eq!(c("100.127.255.254"), AddrClass::Cgnat);
        assert_eq!(c("100.128.0.1"), AddrClass::Public);
        assert_eq!(c("100.63.0.1"), AddrClass::Public);
        assert_eq!(c("0.0.0.0"), AddrClass::Unspecified);
        assert_eq!(c("127.0.0.1"), AddrClass::Other);
        assert_eq!(c("169.254.1.1"), AddrClass::Other);
        assert_eq!(c("224.0.0.1"), AddrClass::Other);
        assert!(explain_non_public("100.64.1.1".parse().unwrap(), AddrClass::Cgnat).contains("pas d'IP publique"));
        assert!(explain_non_public("192.168.0.2".parse().unwrap(), AddrClass::Private).contains("double NAT"));
    }

    #[test]
    fn candidate_ports_order_and_uniqueness() {
        let mut rng = StdRng::seed_from_u64(1);
        let v = candidate_ports(7777, 4, 6, &mut rng);
        assert_eq!(&v[..5], &[7777, 7778, 7779, 7780, 7781]);
        assert_eq!(v.len(), 11);
        for p in &v[5..] {
            assert!(RANDOM_PORT_RANGE.contains(p));
        }
        let mut s = v.clone();
        s.sort();
        s.dedup();
        assert_eq!(s.len(), v.len(), "pas de doublon");
    }

    #[test]
    fn candidate_ports_edges() {
        let mut rng = StdRng::seed_from_u64(2);
        let v = candidate_ports(65534, 4, 0, &mut rng);
        assert_eq!(v, vec![65534, 65535], "pas de débordement");
        let v = candidate_ports(0, 4, 3, &mut rng);
        assert_eq!(v.len(), 3);
        assert!(!v.contains(&0));
        // Un port souhaité dans la plage aléatoire n'est pas retiré une seconde fois.
        let v = candidate_ports(30000, 0, 200, &mut rng);
        assert_eq!(v.iter().filter(|&&p| p == 30000).count(), 1);
    }

    #[test]
    fn renewal_schedule() {
        assert_eq!(renew_delay(Duration::from_secs(3600)), Duration::from_secs(1800));
        assert_eq!(renew_delay(Duration::from_secs(120)), Duration::from_secs(60));
        assert_eq!(renew_delay(Duration::from_secs(4)), MIN_RENEW);
        assert_eq!(renew_delay(Duration::ZERO), PERMANENT_RECHECK);
    }

    #[test]
    fn retry_schedule() {
        assert_eq!(retry_delay(0, None), Duration::from_secs(15));
        assert_eq!(retry_delay(1, None), Duration::from_secs(30));
        assert_eq!(retry_delay(5, None), Duration::from_secs(300));
        assert_eq!(retry_delay(40, None), Duration::from_secs(300));
        // 10 min restantes : jamais plus de 5 min, et la moitié du restant au plus.
        assert_eq!(retry_delay(9, Some(Duration::from_secs(400))), Duration::from_secs(200));
        assert_eq!(retry_delay(0, Some(Duration::from_secs(2))), MIN_RENEW);
    }
}

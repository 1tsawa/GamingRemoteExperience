//! Demandes d'accès sans invitation et test de joignabilité (logique sans UI).
//!
//! - côté host : lignes de la liste des demandes (`trust.json`) et sonde de joignabilité
//!   (`KEY_REQ` portant la sonde du host : seul ce PC obtient une réponse quand les demandes sont
//!   fermées, voir `grx_core::pairing`) ;
//! - côté client : analyse des lignes `GRX-REQ …` écrites par `grx-client request`.

use std::net::{ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use grx_core::pairing::{parse_key_response, probe_request, ProbeKey};
use grx_core::util::now_ms;
use grx_core::trust::AccessRequest;
use grx_core::PublicKey;

use crate::logic::sanitize_display;

// ───────────────────────────── host : liste des demandes ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestRow {
    pub name: String,
    pub fingerprint: String,
    pub addr: String,
    pub first_ms: u64,
    pub denied: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RequestsState {
    /// 0 = fermées ; `grx_core::trust::REQUESTS_ALWAYS_OPEN` = sans limite.
    pub open_until_ms: u64,
    pub rows: Vec<RequestRow>,
}

impl RequestsState {
    pub fn is_open(&self, now_ms: u64) -> bool {
        now_ms < self.open_until_ms
    }

    pub fn pending(&self) -> impl Iterator<Item = &RequestRow> {
        self.rows.iter().filter(|r| !r.denied)
    }
}

pub fn request_rows(reqs: &[AccessRequest]) -> Vec<RequestRow> {
    reqs.iter()
        .map(|r| RequestRow {
            // Nom et adresse viennent d'un inconnu : nettoyés avant affichage.
            name: sanitize_display(&r.name, 60),
            fingerprint: r.fingerprint(),
            addr: sanitize_display(&r.addr, 60),
            first_ms: r.first_ms,
            denied: r.denied,
        })
        .collect()
}

// ───────────────────────────── host : joignabilité ─────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachResult {
    /// Le host a répondu (temps d'aller-retour).
    Ok(Duration),
    /// Un autre host (clé différente) répond à cette adresse.
    WrongHost,
    NoReply,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachLine {
    pub label: String,
    pub addr: String,
    pub result: ReachResult,
    /// Explication à afficher sous le résultat.
    pub hint: String,
}

/// Envoie une sonde fraîche du host à `addr` et attend sa clé publique (3 essais de 700 ms).
pub fn probe_addr(addr: &str, probe: &ProbeKey, expect: &PublicKey) -> ReachResult {
    let remote = match addr.to_socket_addrs().map(|mut a| a.next()) {
        Ok(Some(r)) => r,
        Ok(None) => return ReachResult::Error("adresse introuvable".into()),
        Err(e) => return ReachResult::Error(format!("adresse invalide : {e}")),
    };
    let sock = match UdpSocket::bind(if remote.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).and_then(|s| s.connect(remote).map(|_| s)) {
        Ok(s) => s,
        Err(e) => return ReachResult::Error(format!("socket : {e}")),
    };
    let mut buf = [0u8; 2048];
    for _ in 0..3 {
        // Sonde neuve à chaque essai : le host refuse une sonde déjà vue (anti-rejeu).
        let req = probe_request(probe, now_ms());
        let t0 = Instant::now();
        if let Err(e) = sock.send(&req) {
            return ReachResult::Error(format!("envoi : {e}"));
        }
        let deadline = t0 + Duration::from_millis(700);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let _ = sock.set_read_timeout(Some(left));
            match sock.recv(&mut buf) {
                Ok(n) => match parse_key_response(&buf[..n]) {
                    Some(k) if k == *expect => return ReachResult::Ok(t0.elapsed()),
                    Some(_) => return ReachResult::WrongHost,
                    None => continue,
                },
                // Délai écoulé, ou ICMP « port fermé » (Windows le remonte en erreur) : essai suivant.
                Err(_) => {
                    std::thread::sleep(left.min(Duration::from_millis(100)));
                    break;
                }
            }
        }
    }
    ReachResult::NoReply
}

/// Cibles du test : `(libellé, adresse, explication en cas d'échec)`.
pub fn reach_targets(local_ip: Option<&str>, port: u16, public: Option<&str>) -> Vec<(String, String, String)> {
    let mut v = vec![(
        "Host à l'écoute (ce PC)".to_string(),
        format!("127.0.0.1:{port}"),
        "Le host ne répond pas sur ce PC : est-il démarré sur ce port ?".to_string(),
    )];
    if let Some(ip) = local_ip {
        v.push((
            "Réseau local".to_string(),
            format!("{ip}:{port}"),
            "Pas de réponse sur l'IP locale : host lié à une autre interface (bind) ?".to_string(),
        ));
    }
    if let Some(p) = public.map(str::trim).filter(|p| !p.is_empty()) {
        v.push((
            "Adresse publique".to_string(),
            p.to_string(),
            "Sans réponse depuis l'intérieur : la redirection de la box est peut-être absente, OU la box ne gère pas le « NAT \
             loopback » (fréquent). Seul un essai depuis l'extérieur (partage 4G, ami) tranche : « Demander l'accès » côté \
             client avec les demandes ouvertes."
                .to_string(),
        ));
    }
    v
}

// ───────────────────────────── client : progression ─────────────────────────────

/// Ligne `GRX-REQ …` écrite par `grx-client request`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReqLine {
    ClientFp(String),
    HostFp(String),
    Pending(String),
    NoReply,
    Accepted(String),
    Denied,
}

pub fn parse_req_line(line: &str) -> Option<ReqLine> {
    let rest = line.trim_end().strip_prefix("GRX-REQ ")?;
    let (kw, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    let arg = sanitize_display(arg.trim(), 80);
    Some(match kw {
        "client" => ReqLine::ClientFp(arg),
        "host" => ReqLine::HostFp(arg),
        "pending" => ReqLine::Pending(arg),
        "noreply" => ReqLine::NoReply,
        "accepted" => ReqLine::Accepted(arg),
        "denied" => ReqLine::Denied,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqPhase {
    /// Recherche de la clé du host.
    Contacting,
    /// Clé reçue, demande envoyée.
    Sent,
    /// Le host a enregistré la demande.
    Pending,
    /// Le host ne répond plus aux relances.
    NoReply,
    Accepted,
    Denied,
}

/// Demande d'accès en cours côté client (affichée dans l'onglet Client).
#[derive(Clone, Debug)]
pub struct AccessReqUi {
    pub id: u64,
    pub addr: String,
    pub name: String,
    pub host_fp: Option<String>,
    pub host_name: Option<String>,
    pub phase: ReqPhase,
    /// Nom sous lequel le host a été mémorisé (acceptée).
    pub saved_as: Option<String>,
    /// Annulée par l'utilisateur (le processus est en cours d'arrêt).
    pub cancelled: bool,
}

impl AccessReqUi {
    pub fn new(id: u64, addr: String, name: String) -> Self {
        Self { id, addr, name, host_fp: None, host_name: None, phase: ReqPhase::Contacting, saved_as: None, cancelled: false }
    }

    pub fn apply(&mut self, l: ReqLine) {
        match l {
            ReqLine::ClientFp(_) => {}
            ReqLine::HostFp(fp) => {
                self.host_fp = Some(fp);
                self.phase = ReqPhase::Sent;
            }
            ReqLine::Pending(h) => {
                self.host_name = Some(h);
                self.phase = ReqPhase::Pending;
            }
            ReqLine::NoReply => self.phase = ReqPhase::NoReply,
            ReqLine::Accepted(n) => {
                self.saved_as = Some(n);
                self.phase = ReqPhase::Accepted;
            }
            ReqLine::Denied => self.phase = ReqPhase::Denied,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_core::Identity;

    #[test]
    fn req_lines_are_parsed_and_sanitized() {
        assert_eq!(parse_req_line("GRX-REQ host ABCD-EFGH\r"), Some(ReqLine::HostFp("ABCD-EFGH".into())));
        assert_eq!(parse_req_line("GRX-REQ pending Salon PC"), Some(ReqLine::Pending("Salon PC".into())));
        assert_eq!(parse_req_line("GRX-REQ accepted mon host"), Some(ReqLine::Accepted("mon host".into())));
        assert_eq!(parse_req_line("GRX-REQ denied"), Some(ReqLine::Denied));
        assert_eq!(parse_req_line("GRX-REQ noreply"), Some(ReqLine::NoReply));
        assert_eq!(parse_req_line("GRX-REQ bizarre x"), None);
        assert_eq!(parse_req_line("Empreinte du host : X"), None);
        // Nom de host hostile (caractères de contrôle) : nettoyé.
        match parse_req_line("GRX-REQ pending a\u{1b}[31mb") {
            Some(ReqLine::Pending(n)) => assert!(!n.contains('\u{1b}')),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn access_req_ui_phases() {
        let mut u = AccessReqUi::new(1, "1.2.3.4:443".into(), "salon".into());
        assert_eq!(u.phase, ReqPhase::Contacting);
        u.apply(ReqLine::HostFp("FP".into()));
        assert_eq!(u.phase, ReqPhase::Sent);
        u.apply(ReqLine::Pending("PC".into()));
        assert_eq!((u.phase, u.host_name.as_deref()), (ReqPhase::Pending, Some("PC")));
        u.apply(ReqLine::Accepted("salon".into()));
        assert_eq!((u.phase, u.saved_as.as_deref()), (ReqPhase::Accepted, Some("salon")));
    }

    #[test]
    fn reach_targets_list() {
        let t = reach_targets(Some("192.168.1.20"), 443, Some(" 203.0.113.5:443 "));
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].1, "127.0.0.1:443");
        assert_eq!(t[1].1, "192.168.1.20:443");
        assert_eq!(t[2].1, "203.0.113.5:443");
        assert_eq!(reach_targets(None, 7777, Some("")).len(), 1);
    }

    /// Faux host : répond à la sonde avec sa clé, ou avec une autre clé.
    #[test]
    fn probe_addr_ok_wrong_host_and_silence() {
        let id = Identity::generate();
        let probe = id.probe_key();
        let key = id.public();
        let host = UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = host.local_addr().unwrap().to_string();
        let other = Identity::generate().public();
        let th = std::thread::spawn(move || {
            let mut buf = [0u8; 128];
            for answer in [key, other] {
                let (n, from) = host.recv_from(&mut buf).unwrap();
                assert!(grx_core::pairing::parse_key_request(&buf[..n]).is_some());
                host.send_to(&grx_core::pairing::key_response(&answer), from).unwrap();
            }
        });
        assert!(matches!(probe_addr(&addr, &probe, &key), ReachResult::Ok(_)));
        assert_eq!(probe_addr(&addr, &probe, &key), ReachResult::WrongHost);
        th.join().unwrap();
        // Plus personne n'écoute : aucune réponse.
        assert_eq!(probe_addr(&addr, &probe, &key), ReachResult::NoReply);
    }
}

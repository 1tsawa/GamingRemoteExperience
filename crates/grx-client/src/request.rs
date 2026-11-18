//! Demande d'accès sans invitation : le client ne connaît que `IP:port` du host.
//!
//! 1. KEY_REQ → KEY_RESP : clé publique du host (NON authentifiée : confiance au premier usage,
//!    les deux côtés affichent les empreintes pour comparaison) ;
//! 2. INIT Noise IK avec `pair: true` toutes les ~2 s : le host répond (chiffré, authentifié par
//!    la clé obtenue) `pending`, `denied` ou `accepted` jusqu'à la décision de son propriétaire.
//!
//! Aucune dépendance Windows : simple UDP (réutilisable par le futur client Android).

use std::io::ErrorKind;
use std::net::{ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use grx_core::pairing::{key_request, parse_key_response, PKT_KEY_RESP};
use grx_core::util::{is_transient_recv_error, now_ms};
use grx_core::{fingerprint, ClientHandshake, Identity, PairStatus, PublicKey, PKT_RESP};

/// Délai entre deux relances de la demande.
const POLL_EVERY: Duration = Duration::from_secs(2);
const KEY_ATTEMPTS: u32 = 4;
const KEY_TIMEOUT: Duration = Duration::from_millis(1500);

pub struct RequestOpts {
    /// Nom annoncé au host (affiché dans sa liste de demandes).
    pub name: String,
    /// Durée maximale d'attente de la décision.
    pub wait: Duration,
    /// Empreinte attendue du host (optionnelle) : refus immédiat si elle diffère.
    pub expect_fingerprint: Option<String>,
}

pub enum RequestEvent<'a> {
    /// Clé du host reçue (à comparer avec l'empreinte affichée sur le host).
    HostKey { fingerprint: &'a str },
    /// Demande enregistrée, en attente de validation.
    Pending { host_name: &'a str },
    /// Pas de réponse à la dernière relance (le host a pu refermer les demandes).
    NoReply,
}

pub enum RequestOutcome {
    Accepted { host_key: PublicKey, host_name: String },
    Denied { host_name: String },
}

fn norm_fp(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase()
}

/// Attend un datagramme dont le premier octet est `kind` jusqu'à `deadline`.
fn recv_kind(sock: &UdpSocket, buf: &mut [u8], kind: u8, deadline: Instant) -> Result<Option<usize>> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        sock.set_read_timeout(Some(left))?;
        match sock.recv(buf) {
            Ok(n) if n > 0 && buf[0] == kind => return Ok(Some(n)),
            Ok(_) => continue, // paquet parasite
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
            Err(e) if is_transient_recv_error(&e) => {
                // ICMP « port fermé » : on attend la suite sans boucler à vide.
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn request_access(
    id: &Identity,
    addr: &str,
    opts: &RequestOpts,
    mut on: impl FnMut(RequestEvent),
) -> Result<RequestOutcome> {
    let remote = addr
        .to_socket_addrs()
        .with_context(|| format!("adresse « {addr} » invalide (attendu IP:port ou nom:port)"))?
        .next()
        .ok_or_else(|| anyhow!("adresse introuvable"))?;
    let sock = UdpSocket::bind(if remote.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    sock.connect(remote)?;
    let mut buf = vec![0u8; 65536];

    // 1. Clé du host.
    let mut host_key = None;
    for _ in 0..KEY_ATTEMPTS {
        sock.send(&key_request(None))?;
        if let Some(n) = recv_kind(&sock, &mut buf, PKT_KEY_RESP, Instant::now() + KEY_TIMEOUT)? {
            if let Some(k) = parse_key_response(&buf[..n]) {
                host_key = Some(k);
                break;
            }
        }
    }
    let Some(host_key) = host_key else {
        bail!(
            "le host {addr} ne répond pas. Causes possibles : demandes d'accès fermées sur le host \
             (onglet Host → « Demandes d'accès » → Ouvrir), mauvaise adresse ou mauvais port, pare-feu ou \
             redirection de la box, host éteint ou de version trop ancienne"
        );
    };
    let fp = fingerprint(&host_key);
    if let Some(want) = &opts.expect_fingerprint {
        if norm_fp(want) != norm_fp(&fp) {
            bail!("l'empreinte du host ({fp}) ne correspond pas à celle attendue ({want}) : possible faux host, abandon");
        }
    }
    on(RequestEvent::HostKey { fingerprint: &fp });

    // 2. Relances jusqu'à la décision.
    let deadline = Instant::now() + opts.wait;
    let mut announced = false;
    let mut misses = 0u32;
    while Instant::now() < deadline {
        let t0 = Instant::now();
        let (hs, init) = ClientHandshake::start_pairing(id, host_key, &opts.name, now_ms())?;
        sock.send(&init)?;
        match recv_kind(&sock, &mut buf, PKT_RESP, t0 + POLL_EVERY)? {
            Some(n) => {
                // Une réponse en retard à la relance précédente ne se déchiffre pas avec cette
                // poignée de main : on l'ignore (la suivante répondra).
                let Ok((st, host_name)) = hs.finish_pairing(&buf[..n]) else {
                    misses += 1;
                    continue;
                };
                misses = 0;
                match st {
                    PairStatus::Accepted => return Ok(RequestOutcome::Accepted { host_key, host_name }),
                    PairStatus::Denied => return Ok(RequestOutcome::Denied { host_name }),
                    PairStatus::Pending => {
                        if !announced {
                            announced = true;
                            on(RequestEvent::Pending { host_name: &host_name });
                        }
                    }
                }
            }
            None => {
                misses += 1;
                if misses == 3 {
                    on(RequestEvent::NoReply);
                }
            }
        }
        // Une relance toutes les ~2 s.
        let spent = t0.elapsed();
        if spent < POLL_EVERY {
            std::thread::sleep(POLL_EVERY - spent);
        }
    }
    bail!("aucune décision du host après {} s : redemandez quand son propriétaire est devant l'écran", opts.wait.as_secs())
}

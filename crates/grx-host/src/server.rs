//! Boucle socket UDP unique du host : poignée de main, session active unique, Endpoint de
//! transport, chiffrement `Session::seal/open`, timeout d'inactivité.
//!
//! Durcissement (voir « Revue de sécurité » dans docs/ARCHITECTURE.md) :
//! * un datagramme quelconque (même > MTU) ou une erreur `recv` transitoire n'arrête JAMAIS la boucle ;
//! * les INIT sont bornés en taille et limités en débit (global + par IP) AVANT tout travail coûteux ;
//! * une nouvelle session reste « en attente » (rien d'émis hormis la réponse de poignée de main,
//!   aucun thread média) jusqu'au premier paquet DATA authentifié : preuve de vivacité du pair ;
//! * une autre clé ne peut pas écraser une session active récemment vivante ;
//! * la liste de confiance est revérifiée chaque seconde (révocation, changement de rôle) ;
//! * la migration d'adresse exige un aller-retour de validation de chemin (challenge/réponse).

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grx_core::session::{peek_sid, MAX_INIT_LEN, MIN_INIT_LEN};
use grx_core::util::{is_transient_recv_error, now_ms};
use grx_core::secure::{access as secure_access, notice as secure_notice, HostSituation, SecureNotice};
use grx_core::{fingerprint, HostAuth, Role, Session, PKT_DATA, PKT_INIT};
use grx_transport::bitrate::BitrateConfig;
use grx_transport::wire::{parse_path_response, path_challenge};
use grx_transport::{Config, Endpoint, Event, Micros, Stats};
use rand::RngCore;

use crate::pipeline::{Pipeline, PipelineOpts};

/// Message `Control` d'adieu envoyé par le client quand il se ferme.
const BYE: &[u8] = b"GRX-BYE";

/// Débit maximal d'INIT traités : global (par seconde, rafale) et par adresse IP source.
const INIT_GLOBAL_RATE: f64 = 20.0;
const INIT_GLOBAL_BURST: f64 = 40.0;
const INIT_IP_RATE: f64 = 5.0;
const INIT_IP_BURST: f64 = 10.0;
const MAX_TRACKED_IPS: usize = 1024;
/// Une autre clé ne remplace pas une session ayant reçu un paquet authentifié depuis moins de ça.
const TAKEOVER_GUARD: Duration = Duration::from_secs(3);
/// Durée pendant laquelle une session issue d'un INIT attend son premier paquet DATA.
const PENDING_TTL: Duration = Duration::from_secs(5);
/// Période de revérification de la liste de confiance pour la session active.
const TRUST_CHECK_EVERY: Duration = Duration::from_secs(1);
/// Échecs de lecture consécutifs de `trust.json` tolérés avant de couper la session active.
const TRUST_MAX_FAILURES: u32 = 5;
/// Un challenge de chemin par seconde au plus, valable 5 s.
const PROBE_EVERY: Duration = Duration::from_secs(1);
const PROBE_TTL: Duration = Duration::from_secs(5);
/// Erreurs `recv` non transitoires consécutives avant d'abandonner (socket réellement cassée).
const MAX_FATAL_RECV_ERRORS: u32 = 50;

pub struct RunOpts {
    pub bind: SocketAddr,
    pub pipeline: PipelineOpts,
    /// Inactivité (aucun paquet authentifié) au-delà de laquelle la session est fermée.
    pub idle_timeout: Duration,
}

// ───────────────────────────── limitation de débit ─────────────────────────────

struct TokenBucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(rate: f64, burst: f64, now: Instant) -> Self {
        Self { rate, burst, tokens: burst, last: now }
    }

    fn take(&mut self, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = self.last.max(now);
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Seaux à jetons des INIT : par IP source d'abord (une IP ne peut pas vider le quota global),
/// puis global. Un INIT légitime est rare (1 par connexion).
struct InitLimiter {
    global: TokenBucket,
    per_ip: HashMap<IpAddr, TokenBucket>,
}

impl InitLimiter {
    fn new(now: Instant) -> Self {
        Self { global: TokenBucket::new(INIT_GLOBAL_RATE, INIT_GLOBAL_BURST, now), per_ip: HashMap::new() }
    }

    fn allow(&mut self, ip: IpAddr, now: Instant) -> bool {
        if self.per_ip.len() >= MAX_TRACKED_IPS {
            // Un seau inactif depuis > 3 s est de toute façon redevenu plein.
            self.per_ip.retain(|_, b| now.saturating_duration_since(b.last) < Duration::from_secs(3));
            if self.per_ip.len() >= MAX_TRACKED_IPS {
                self.per_ip.clear(); // inondation d'adresses usurpées : le seau global protège
            }
        }
        let b = self.per_ip.entry(ip).or_insert_with(|| TokenBucket::new(INIT_IP_RATE, INIT_IP_BURST, now));
        b.take(now) && self.global.take(now)
    }
}

/// Limiteur de journal : au plus une ligne par seconde, avec le nombre de messages supprimés.
struct LogLimiter {
    last: Option<Instant>,
    suppressed: u64,
}

impl LogLimiter {
    fn new() -> Self {
        Self { last: None, suppressed: 0 }
    }

    /// `Some(n)` si on peut journaliser maintenant (`n` messages ont été supprimés depuis la
    /// dernière ligne), `None` si le message doit être supprimé.
    fn note(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < Duration::from_secs(1) => {
                self.suppressed += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.suppressed))
            }
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
struct Counters {
    /// INIT arrivés jusqu'au déchiffrement (donc coûteux).
    init_processed: u64,
    init_rate_limited: u64,
    init_bad_size: u64,
}

// ───────────────────────────── sessions ─────────────────────────────

/// Poignée de main réussie, mais le pair n'a pas encore prouvé qu'il est vivant : on n'a émis que
/// la réponse de poignée de main, pas de pipeline média, pas d'Endpoint.
struct Pending {
    sess: Session,
    peer: SocketAddr,
    view_only: bool,
    role: Role,
    created: Instant,
}

/// Challenge de validation de chemin envoyé vers une adresse candidate (migration).
struct Probe {
    addr: SocketAddr,
    token: [u8; 8],
    sent: Instant,
}

struct Active {
    sess: Session,
    ep: Endpoint,
    peer: SocketAddr,
    pipeline: Pipeline,
    view_only: bool,
    /// Rôle du client (sécurité : qui peut piloter une fenêtre administrateur).
    role: Role,
    /// Dernière notification de sécurité envoyée (`GRX-SEC`), pour n'émettre qu'aux changements.
    sec_notice: Option<SecureNotice>,
    /// La situation précédente autorisait-elle le clavier/souris ? (pour relâcher au passage bloqué)
    sec_km_allowed: bool,
    probe: Option<Probe>,
    t0: Instant,
    last_rx: Instant,
    last_stats: Instant,
    last_ep_stats: Stats,
    last_enc_frames: u64,
    started: Instant,
    /// Curseur du host envoyé à part (forme fiable + position non fiable), voir `cursor.rs`.
    cursor: crate::cursor::CursorSender,
    /// Génération de la liste d'écrans déjà annoncée au client (`GRX-DSP`).
    displays_sent: u64,
}

impl Active {
    fn now_us(&self) -> Micros {
        self.t0.elapsed().as_micros() as Micros
    }
}

pub struct Server {
    sock: UdpSocket,
    auth: HostAuth,
    opts: RunOpts,
    active: Option<Active>,
    pending: Option<Pending>,
    limiter: InitLimiter,
    log: LogLimiter,
    counters: Counters,
    last_trust_check: Instant,
    trust_failures: u32,
}

/// Socket UDP du host. Une adresse IPv6 (`[::]:7777`) est ouverte en double pile (IPv4 + IPv6) :
/// sous Windows `IPV6_V6ONLY` vaut vrai par défaut, on le désactive explicitement.
pub fn bind_socket(addr: SocketAddr) -> Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let s = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP)).context("socket UDP")?;
    if addr.is_ipv6() {
        let _ = s.set_only_v6(false);
    }
    s.bind(&addr.into()).with_context(|| format!("bind {addr}"))?;
    let _ = s.set_recv_buffer_size(4 << 20);
    let _ = s.set_send_buffer_size(4 << 20);
    Ok(s.into())
}

/// Sert sur une socket déjà liée (voir [`bind_socket`]) jusqu'à Ctrl+C.
pub fn run(sock: UdpSocket, auth: HostAuth, opts: RunOpts) -> Result<()> {
    let quit = Arc::new(AtomicBool::new(false));
    {
        let q = quit.clone();
        let _ = ctrlc::set_handler(move || q.store(true, Ordering::SeqCst));
    }
    serve(sock, auth, opts, quit)
}

/// Boucle du host sur une socket déjà liée, jusqu'à ce que `quit` passe à vrai.
pub fn serve(sock: UdpSocket, mut auth: HostAuth, opts: RunOpts, quit: Arc<AtomicBool>) -> Result<()> {
    // Aucun INIT capturé avant ce démarrage ne pourra être rejoué (`last_ts` est en mémoire).
    auth.set_boot_ms(now_ms());
    println!("Host {} à l'écoute sur {}", auth.identity().fingerprint(), sock.local_addr()?);
    let mut srv = Server::new(sock, auth, opts);
    srv.run_loop(&quit)?;
    println!("Arrêt demandé.");
    if let Some(a) = srv.active.take() {
        end_session(&srv.sock, a, "arrêt du host", true);
    }
    Ok(())
}

impl Server {
    fn new(sock: UdpSocket, auth: HostAuth, opts: RunOpts) -> Self {
        let now = Instant::now();
        Self {
            sock,
            auth,
            opts,
            active: None,
            pending: None,
            limiter: InitLimiter::new(now),
            log: LogLimiter::new(),
            counters: Counters::default(),
            last_trust_check: now,
            trust_failures: 0,
        }
    }

    /// Journal d'erreur limité en débit (1 ligne/s) : un attaquant ne peut pas noyer la console
    /// ni le disque, ni nous faire dépenser du CPU à formater.
    fn warn(&mut self, msg: impl FnOnce() -> String) {
        if let Some(n) = self.log.note(Instant::now()) {
            if n > 0 {
                eprintln!("{} (+{n} messages similaires supprimés)", msg());
            } else {
                eprintln!("{}", msg());
            }
        }
    }

    fn run_loop(&mut self, quit: &AtomicBool) -> Result<()> {
        // 64 Kio : la taille maximale d'un datagramme UDP. Un tampon plus petit fait échouer
        // `recv_from` (WSAEMSGSIZE sous Windows) sur un gros datagramme, et un seul paquet non
        // authentifié arrêterait alors le host.
        let mut buf = vec![0u8; 65536];
        let mut blocking = true;
        let mut fatal_errors = 0u32;
        self.sock.set_read_timeout(Some(Duration::from_millis(200)))?;

        while !quit.load(Ordering::Relaxed) {
            // Sans session : on dort dans recv (aucune charge). Avec session : on poll finement.
            if self.active.is_none() != blocking {
                blocking = self.active.is_none();
                self.sock.set_nonblocking(!blocking)?;
                if blocking {
                    self.sock.set_read_timeout(Some(Duration::from_millis(200)))?;
                }
            }

            // 1. Datagrammes entrants.
            let mut budget = 2048;
            while budget > 0 {
                budget -= 1;
                match self.sock.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        fatal_errors = 0;
                        self.handle_packet(&buf[..n], from);
                        // Après le premier paquet en mode bloquant, on passe en polling.
                        if blocking && self.active.is_some() {
                            break;
                        }
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                    Err(e) if is_transient_recv_error(&e) => {
                        // ICMP « port unreachable » d'un envoi précédent, datagramme tronqué... :
                        // jamais fatal. On journalise (limité) et on continue.
                        self.warn(|| format!("- recv ignoré : {e}"));
                    }
                    Err(e) => {
                        fatal_errors += 1;
                        if fatal_errors >= MAX_FATAL_RECV_ERRORS {
                            return Err(e).context("recv");
                        }
                        self.warn(|| format!("! recv : {e}"));
                        thread::sleep(Duration::from_millis(1));
                        break;
                    }
                }
            }

            // 2. Session active : événements, médias sortants, timers, émission.
            self.tick();

            if let Some(a) = self.active.as_ref() {
                let now = a.now_us();
                let sleep = match a.ep.next_wakeup(now) {
                    Some(t) if t > now => (t - now).min(250),
                    Some(_) => 0,
                    None => 250,
                };
                if sleep > 0 {
                    thread::sleep(Duration::from_micros(sleep));
                }
            }
        }
        Ok(())
    }

    /// Un tour de timers : expiration de l'attente, session active, revérification de la confiance.
    fn tick(&mut self) {
        if self.pending.as_ref().is_some_and(|p| p.created.elapsed() > PENDING_TTL) {
            self.pending = None;
        }

        let mut end: Option<&'static str> = None;
        if let Some(a) = self.active.as_mut() {
            end = drive(&self.sock, a, &self.opts);
            if end.is_none() && a.last_rx.elapsed() > self.opts.idle_timeout {
                end = Some("inactivité");
            }
        }
        if end.is_none() {
            end = self.check_trust();
        }
        if let Some(reason) = end {
            if let Some(a) = self.active.take() {
                end_session(&self.sock, a, reason, !matches!(reason, "inactivité" | "déconnexion du client"));
            }
        }
    }

    /// Revérifie ~1 fois/s que le client de la session active est toujours autorisé (révocation
    /// par la CLI dans un autre processus) et que son rôle n'a pas changé.
    fn check_trust(&mut self) -> Option<&'static str> {
        let a = self.active.as_ref()?;
        if self.last_trust_check.elapsed() < TRUST_CHECK_EVERY {
            return None;
        }
        self.last_trust_check = Instant::now();
        let (key, role) = (a.sess.peer_key(), a.role);
        match self.auth.trust.refresh() {
            Ok(_) => {
                self.trust_failures = 0;
                if !self.auth.trust.is_authorized(&key) {
                    return Some("clé révoquée");
                }
                if self.auth.trust.role_of(&key) != Some(role) {
                    return Some("rôle modifié (reconnexion nécessaire)");
                }
                None
            }
            Err(e) => {
                // Fail-closed : une liste illisible ne protège plus contre une révocation.
                self.trust_failures += 1;
                let n = self.trust_failures;
                self.warn(|| format!("! trust.json illisible ({n}/{TRUST_MAX_FAILURES}) : {e}"));
                (n >= TRUST_MAX_FAILURES).then_some("liste de confiance illisible")
            }
        }
    }

    fn handle_packet(&mut self, pkt: &[u8], from: SocketAddr) {
        match pkt.first() {
            Some(&PKT_INIT) => self.handle_init(pkt, from),
            Some(&PKT_DATA) => self.handle_data(pkt, from),
            _ => {}
        }
    }

    fn handle_init(&mut self, pkt: &[u8], from: SocketAddr) {
        let now = Instant::now();
        // 1. Taille : un vrai INIT est dans [MIN_INIT_LEN, MAX_INIT_LEN]. Gratuit.
        if !(MIN_INIT_LEN..=MAX_INIT_LEN).contains(&pkt.len()) {
            self.counters.init_bad_size += 1;
            return;
        }
        // 2. Débit, AVANT tout travail coûteux (DH Noise, lecture disque).
        if !self.limiter.allow(from.ip(), now) {
            self.counters.init_rate_limited += 1;
            self.warn(|| format!("- INIT ignoré (trop fréquent) depuis {from}"));
            return;
        }
        self.counters.init_processed += 1;

        // Une AUTRE clé ne peut pas écraser une session récemment vivante ; la même clé peut
        // toujours reprendre la main (reconnexion après un plantage du client).
        let guard = self
            .active
            .as_ref()
            .filter(|a| a.last_rx.elapsed() < TAKEOVER_GUARD)
            .map(|a| a.sess.peer_key());
        let res = self.auth.accept_if(pkt, now_ms(), |k| guard.is_none_or(|g| g == *k));
        match res {
            Ok((resp, sess)) => {
                if let Err(e) = self.sock.send_to(&resp, from) {
                    self.warn(|| format!("! envoi RESP : {e}"));
                    return;
                }
                println!(
                    "+ {} ({}) authentifié depuis {from}, session {:08x} : attente du premier paquet",
                    sess.peer_name(),
                    fingerprint(&sess.peer_key()),
                    sess.sid()
                );
                // La session précédente (même clé) continue jusqu'à ce que celle-ci soit validée.
                let role = self.auth.trust.role_of(&sess.peer_key()).unwrap_or(Role::ViewOnly);
                let view_only = role == Role::ViewOnly;
                self.pending = Some(Pending { sess, peer: from, view_only, role, created: now });
            }
            // Volontairement silencieux envers le pair ; journal local seulement.
            Err(e) => self.warn(|| format!("- INIT refusé depuis {from}: {e}")),
        }
    }

    fn handle_data(&mut self, pkt: &[u8], from: SocketAddr) {
        let Some(sid) = peek_sid(pkt) else { return };

        // Premier paquet authentifié d'une session en attente : preuve de vivacité.
        if self.pending.as_ref().is_some_and(|p| p.sess.sid() == sid) {
            let p = self.pending.as_mut().unwrap();
            if from != p.peer {
                return;
            }
            if let Ok(payload) = p.sess.open(pkt) {
                let p = self.pending.take().unwrap();
                self.promote(p, &payload);
            }
            return;
        }

        let Some(a) = self.active.as_mut() else { return };
        if a.sess.sid() != sid {
            return;
        }
        let Ok(payload) = a.sess.open(pkt) else { return }; // rejeu / forgé : ignoré en silence
        a.last_rx = Instant::now();

        // Réponse à un challenge de chemin : migration si elle vient bien de l'adresse testée.
        if let Some(token) = parse_path_response(&payload) {
            if let Some(pr) = a.probe.take_if(|pr| pr.token == token && pr.addr == from && pr.sent.elapsed() < PROBE_TTL) {
                println!("~ [{:08x}] itinérance validée : {} -> {}", a.sess.sid(), a.peer, pr.addr);
                a.peer = pr.addr;
            }
            return;
        }
        // Paquet authentique venant d'une AUTRE adresse (itinérance, NAT, ou rejeu usurpé) : on
        // n'y envoie rien (hormis un petit challenge) tant que le chemin n'est pas validé. Un
        // paquet capturé et rejoué depuis une adresse usurpée ne peut pas produire la réponse.
        if from != a.peer {
            let due = a.probe.as_ref().is_none_or(|pr| pr.sent.elapsed() >= PROBE_EVERY);
            if due {
                let mut token = [0u8; 8];
                rand::rngs::OsRng.fill_bytes(&mut token);
                if let Ok(sealed) = a.sess.seal(&path_challenge(token)) {
                    let _ = self.sock.send_to(&sealed, from);
                    a.probe = Some(Probe { addr: from, token, sent: Instant::now() });
                }
            }
        }
        let now = a.now_us();
        a.ep.handle_datagram(now, &payload);
    }

    /// Le pair d'une session en attente vient de prouver qu'il est vivant : on démarre pour de
    /// bon (threads média, Endpoint) et on remplace l'éventuelle session précédente.
    fn promote(&mut self, p: Pending, first_payload: &[u8]) {
        if let Some(old) = self.active.take() {
            end_session(&self.sock, old, "remplacée par une nouvelle connexion", true);
        }
        let mut a = start_session(p.sess, p.peer, p.view_only, p.role, &self.opts);
        let now = a.now_us();
        a.ep.handle_datagram(now, first_payload);
        // Limites et réglages courants, pour l'overlay du client.
        let _ = a.ep.send_control(&crate::control::host_info(&a.ep, &a.pipeline.shared, &self.opts.pipeline).encode());
        println!(
            "+ [{:08x}] session démarrée avec {} ({}){}",
            a.sess.sid(),
            a.sess.peer_name(),
            a.peer,
            if a.view_only { " [spectateur : aucune entrée injectée]" } else { "" }
        );
        self.active = Some(a);
    }
}

/// Options du pipeline pour une session : un spectateur n'a AUCUN thread d'injection.
fn session_pipeline_opts(base: &PipelineOpts, ep: &Endpoint, view_only: bool) -> PipelineOpts {
    let mut o = base.clone();
    o.initial_bitrate_bps = ep.target_bitrate_bps();
    o.input = base.input && !view_only;
    o
}

fn start_session(sess: Session, peer: SocketAddr, view_only: bool, role: Role, opts: &RunOpts) -> Active {
    let o = &opts.pipeline;
    let cfg = Config {
        bitrate: BitrateConfig {
            start_bps: o.initial_bitrate_bps,
            max_bps: o.max_bitrate_bps,
            ..Default::default()
        },
        // Un client n'envoie ni vidéo ni audio : on les ignore (aucun coût de réassemblage).
        accept_media: false,
        ..Default::default()
    };
    let ep = Endpoint::new(cfg);
    let pipeline = Pipeline::start(&session_pipeline_opts(o, &ep, view_only));
    let now = Instant::now();
    let cursor = crate::cursor::CursorSender::new(pipeline.shared.cursor.clone());
    Active {
        cursor,
        displays_sent: 0,
        sess,
        ep,
        peer,
        pipeline,
        view_only,
        role,
        sec_notice: None,
        sec_km_allowed: true,
        probe: None,
        t0: now,
        last_rx: now,
        last_stats: now,
        last_ep_stats: Stats::default(),
        last_enc_frames: 0,
        started: now,
    }
}

/// Ferme la session ; `notify` prévient le pair (adieu fiable best-effort, 1 envoi).
fn end_session(sock: &UdpSocket, mut a: Active, reason: &str, notify: bool) {
    if notify {
        let _ = a.ep.send_control(BYE);
        let now = a.now_us();
        let mut n = 0;
        while let Some(p) = a.ep.poll_transmit(now) {
            if let Ok(sealed) = a.sess.seal(&p) {
                let _ = sock.send_to(&sealed, a.peer);
            }
            n += 1;
            if n >= 8 {
                break;
            }
        }
    }
    let st = a.ep.stats();
    println!(
        "- [{:08x}] session terminée ({reason}) après {:.1} s : {} images envoyées, {} jetées à l'émission, {} entrées non injectées",
        a.sess.sid(),
        a.started.elapsed().as_secs_f64(),
        st.frames_sent,
        st.frames_dropped_tx,
        a.pipeline.shared.inputs_other.load(Ordering::Relaxed)
    );
    // Arrête les threads ; le thread d'entrée relâche toutes les touches/boutons/manettes.
    a.pipeline.stop();
}

/// L'événement d'entrée est-il une manette (non concernée par l'UIPI) ? Tout octet malformé est
/// traité comme « non-manette » (sera de toute façon ignoré par l'injecteur).
fn is_gamepad_event(bytes: &[u8]) -> bool {
    matches!(
        grx_input::InputEvent::decode(bytes),
        Ok(grx_input::InputEvent::Gamepad(_))
            | Ok(grx_input::InputEvent::GamepadConnect { .. })
            | Ok(grx_input::InputEvent::GamepadDisconnect { .. })
    )
}

/// Envoie `GRX-SEC` au client si la notification a changé depuis la dernière (ou la première fois
/// qu'il y a quelque chose à signaler).
fn maybe_send_secure_notice(a: &mut Active, sit: &HostSituation) {
    let n = secure_notice(a.role, sit);
    if a.sec_notice == Some(n) {
        return;
    }
    if a.ep.send_control(&n.encode()).is_ok() {
        a.sec_notice = Some(n);
    }
}

/// Un tour de boucle pour la session active. Retourne `Some(raison)` si elle doit se terminer.
fn drive(sock: &UdpSocket, a: &mut Active, opts: &RunOpts) -> Option<&'static str> {
    let now = a.now_us();

    // Situation de sécurité observée (bureau sécurisé / fenêtre administrateur) et droits du
    // client pour ce tour. Aucun appel système ici : lecture d'un atomique rempli par `grx-secure`.
    let sit = crate::secure::current();
    let access = secure_access(a.role, &sit);

    // Images / audio produits par les threads média. Sur le bureau sécurisé (UAC, verrouillage),
    // aucune image n'est envoyée à personne (défense en profondeur : DXGI n'en produit de toute
    // façon pas) ; les images sont vidées sans être transmises.
    loop {
        match a.pipeline.video_rx.try_recv() {
            Ok(v) => {
                if access.video {
                    if let Err(e) = a.ep.send_video_frame(now, &v.data, v.keyframe, v.pts_us) {
                        eprintln!("! send_video_frame : {e}");
                    }
                }
            }
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
        }
    }
    while let Ok(au) = a.pipeline.audio_rx.try_recv() {
        let _ = a.ep.send_audio(au.pts_us, &au.data);
    }
    // Vibrations des manettes virtuelles -> client (canal Control fiable ; dédoublonnées à la
    // source, au plus 16 par tour pour borner le canal).
    for _ in 0..16 {
        match a.pipeline.rumble_rx.try_recv() {
            Ok(r) => {
                let _ = a.ep.send_control(&r.encode());
            }
            Err(_) => break,
        }
    }

    // Événements du pair.
    let mut bye = false;
    while let Some(ev) = a.ep.poll_event() {
        match ev {
            Event::KeyframeRequested => a.pipeline.shared.want_key.store(true, Ordering::SeqCst),
            Event::Input(m) => {
                // Un spectateur n'a pas de thread d'injection ; sinon, le droit dépend de la
                // catégorie et de la situation (fenêtre administrateur : clavier/souris bloqués
                // sauf « Contrôle total » avec host élevé ; manette jamais concernée par l'UIPI).
                let ok = !a.view_only
                    && if is_gamepad_event(&m) { access.gamepad } else { access.keyboard_mouse };
                if ok {
                    a.pipeline.push_input(m);
                } else {
                    a.pipeline.shared.inputs_other.fetch_add(1, Ordering::Relaxed);
                }
            }
            Event::Control(m) if m == BYE => bye = true,
            // Réglages de l'overlay (débit/FPS max) : permis aussi en lecture seule.
            Event::Control(m) if grx_core::stream_settings::is_settings(&m) => {
                if let Some(reply) = crate::control::apply(&m, &mut a.ep, &a.pipeline.shared, &opts.pipeline) {
                    let _ = a.ep.send_control(&reply);
                }
            }
            // Décodeurs du client (négociation du codec, `codec.rs`) : permis aussi en lecture seule.
            Event::Control(m) if grx_core::codec_neg::is_client_codecs(&m) => {
                a.pipeline.shared.codec.on_client(&m);
            }
            // Choix de l'écran capturé (overlay du client) : permis aussi en lecture seule.
            Event::Control(m) if grx_core::displays::is_display_select(&m) => {
                if let Some(sel) = grx_core::displays::DisplaySelect::decode(&m) {
                    println!("  client : écran {} demandé", sel.index as u32 + 1);
                    a.pipeline.shared.displays.request(sel.index);
                }
            }
            _ => {}
        }
    }
    if bye {
        return Some("déconnexion du client");
    }

    // Sécurité : si le clavier/souris vient de passer de permis à bloqué, demander au thread
    // d'entrée de tout relâcher (pas de touche « coincée » enfoncée pendant le blocage).
    if a.sec_km_allowed && !access.keyboard_mouse && !a.view_only {
        a.pipeline.shared.release_input.store(true, Ordering::SeqCst);
    }
    a.sec_km_allowed = access.keyboard_mouse;
    // Notifier le client (GRX-SEC) à chaque changement de situation, pour le bandeau de l'overlay.
    maybe_send_secure_notice(a, &sit);

    a.pipeline.shared.target_bps.store(a.ep.target_bitrate_bps(), Ordering::Relaxed);
    a.cursor.drive(&mut a.ep);
    crate::codec::announce(&mut a.ep, &a.pipeline.shared.codec);
    // Liste des écrans / écran actif : (ré)annoncée à chaque changement.
    let g = a.pipeline.shared.displays.generation();
    if g != a.displays_sent && a.ep.send_control(&a.pipeline.shared.displays.message().encode()).is_ok() {
        a.displays_sent = g;
    }

    // Émission.
    let mut sent = 0;
    while let Some(p) = a.ep.poll_transmit(now) {
        match a.sess.seal(&p) {
            Ok(sealed) => {
                if let Err(e) = sock.send_to(&sealed, a.peer) {
                    if !is_transient_recv_error(&e) {
                        eprintln!("! send : {e}");
                    }
                }
            }
            Err(_) => return Some("erreur de chiffrement"),
        }
        sent += 1;
        if sent >= 512 {
            break;
        }
    }

    if a.last_stats.elapsed() >= Duration::from_secs(5) {
        log_stats(a, opts);
    }
    None
}

fn log_stats(a: &mut Active, opts: &RunOpts) {
    let dt = a.last_stats.elapsed().as_secs_f64();
    let st = a.ep.stats();
    let sh = &a.pipeline.shared;
    let enc_frames = sh.enc_frames.load(Ordering::Relaxed);
    let dframes = enc_frames - a.last_enc_frames;
    let avg_enc_ms = if enc_frames > 0 {
        sh.enc_time_us.load(Ordering::Relaxed) as f64 / enc_frames as f64 / 1000.0
    } else {
        0.0
    };
    println!(
        "  [{:08x}] {:.1} fps | cible enc {:.1} Mb/s | fil {:.1} Mb/s | rtt {} us | perte pair {:.1}% | FEC {}‰ | jetées {} | enc {:.1} ms | audio {} pkts (silence {}) | entrées {}",
        a.sess.sid(),
        dframes as f64 / dt,
        st.target_bitrate_bps as f64 / 1e6,
        (st.bytes_tx - a.last_ep_stats.bytes_tx) as f64 * 8.0 / dt / 1e6,
        st.rtt_us.unwrap_or(0),
        st.peer_loss * 100.0,
        st.fec_permille,
        st.frames_dropped_tx,
        avg_enc_ms,
        sh.audio_packets.load(Ordering::Relaxed),
        sh.audio_silent.load(Ordering::Relaxed),
        if opts.pipeline.input && !a.view_only {
            sh.inputs_injected.load(Ordering::Relaxed)
        } else {
            sh.inputs_other.load(Ordering::Relaxed)
        },
    );
    a.last_enc_frames = enc_frames;
    a.last_ep_stats = st;
    a.last_stats = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_core::{ClientHandshake, Identity, Token, TrustStore};
    use grx_input::InjectorConfig;
    use grx_transport::wire::{b0, K_PATH_CHALLENGE, K_VIDEO};
    use std::path::PathBuf;

    fn test_opts(input: bool) -> RunOpts {
        RunOpts {
            bind: "127.0.0.1:0".parse().unwrap(),
            idle_timeout: Duration::from_secs(5),
            pipeline: PipelineOpts {
                codec: grx_core::codec_neg::CodecPref::Force(grx_core::codec_neg::VideoCodec::H264),
                fps: 60,
                fill: false,
                audio: false,
                input,
                video: false, // pas de capture/encodage dans les tests du serveur
                // Même si un bug laissait passer une entrée, rien ne serait injecté.
                injector: InjectorConfig {
                    allow_keyboard: false,
                    allow_mouse: false,
                    allow_gamepad: false,
                    ..Default::default()
                },
                initial_bitrate_bps: 10_000_000,
                max_bitrate_bps: 20_000_000,
                display: None,
            encoder: Default::default(),
            chroma: Default::default(),
            test_pattern_hdr: false,
            },
        }
    }

    fn server_with(trust: TrustStore, input: bool) -> Server {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let auth = HostAuth::new(Identity::generate(), trust, "host-test");
        Server::new(sock, auth, test_opts(input))
    }

    fn server() -> Server {
        server_with(TrustStore::in_memory(), false)
    }

    /// `--bind [::]:port` : la socket du host reçoit ET répond en IPv4 et en IPv6.
    #[test]
    fn ipv6_bind_is_dual_stack() {
        let host = bind_socket("[::]:0".parse().unwrap()).unwrap();
        host.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let port = host.local_addr().unwrap().port();
        for (client_bind, target) in [("127.0.0.1:0", format!("127.0.0.1:{port}")), ("[::1]:0", format!("[::1]:{port}"))] {
            let c = UdpSocket::bind(client_bind).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            c.send_to(b"ping", &target).unwrap();
            let mut buf = [0u8; 16];
            let (n, from) = host.recv_from(&mut buf).expect("le host reçoit");
            assert_eq!(&buf[..n], b"ping");
            host.send_to(b"pong", from).unwrap();
            let (n, _) = c.recv_from(&mut buf).expect("le client reçoit la réponse");
            assert_eq!(&buf[..n], b"pong", "{client_bind}");
        }
        // Une adresse IPv4 reste en IPv4 seule.
        let v4 = bind_socket("127.0.0.1:0".parse().unwrap()).unwrap();
        assert!(v4.local_addr().unwrap().is_ipv4());
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("grx-srv-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Faux client : socket réelle + Session + Endpoint. Les paquets sont remis au serveur par
    /// `handle_packet` (sans passer par la boucle), les réponses lues sur la socket.
    struct Client {
        id: Identity,
        sock: UdpSocket,
        addr: SocketAddr,
        sess: Option<Session>,
        ep: Endpoint,
        clock: Micros,
    }

    impl Client {
        fn new() -> Self {
            Self::with_identity(Identity::generate())
        }

        fn with_identity(id: Identity) -> Self {
            let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
            sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
            let addr = sock.local_addr().unwrap();
            Self { id, sock, addr, sess: None, ep: Endpoint::new(Config::default()), clock: 0 }
        }

        fn init_packet(&self, srv: &Server, token: Option<&Token>) -> (ClientHandshake, Vec<u8>) {
            thread::sleep(Duration::from_millis(3)); // horodatage strictement croissant entre deux INIT
            ClientHandshake::start(&self.id, srv.auth.identity().public(), "t", token, now_ms()).unwrap()
        }

        /// INIT + lecture de la réponse. Retourne `true` si le host a répondu.
        fn connect(&mut self, srv: &mut Server, token: Option<&Token>) -> bool {
            let (hs, init) = self.init_packet(srv, token);
            srv.handle_packet(&init, self.addr);
            let mut buf = vec![0u8; 4096];
            match self.sock.recv(&mut buf) {
                Ok(n) => {
                    self.sess = Some(hs.finish(&buf[..n]).unwrap());
                    self.ep = Endpoint::new(Config::default());
                    true
                }
                Err(_) => false,
            }
        }

        /// Tous les paquets que l'Endpoint du client veut émettre maintenant (l'horloge avance d' 1 s à
        /// chaque appel : le ping périodique est toujours dû), chiffrés.
        fn all_sealed(&mut self) -> Vec<Vec<u8>> {
            self.clock += 1_000_000;
            let mut out = Vec::new();
            while let Some(p) = self.ep.poll_transmit(self.clock) {
                out.push(self.sess.as_mut().unwrap().seal(&p).unwrap());
            }
            out
        }

        fn next_sealed(&mut self) -> Vec<u8> {
            self.all_sealed().remove(0)
        }

        fn send_first_data(&mut self, srv: &mut Server) {
            let pkt = self.next_sealed();
            srv.handle_packet(&pkt, self.addr);
        }

        /// Paquets déchiffrés reçus du host (sans attendre).
        fn drain(&mut self) -> Vec<Vec<u8>> {
            self.sock.set_nonblocking(true).unwrap();
            let mut out = Vec::new();
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = self.sock.recv(&mut buf) {
                if let Ok(p) = self.sess.as_mut().unwrap().open(&buf[..n]) {
                    out.push(p);
                }
            }
            self.sock.set_nonblocking(false).unwrap();
            out
        }
    }

    fn invite(srv: &mut Server) -> Token {
        srv.auth.trust.create_invite(60_000, now_ms()).unwrap()
    }

    // ── A1 ──

    #[test]
    fn a1_oversized_datagrams_do_not_stop_the_host() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = sock.local_addr().unwrap();
        let mut auth = HostAuth::new(Identity::generate(), TrustStore::in_memory(), "h");
        let host_key = auth.identity().public();
        let tok = auth.trust.create_invite(60_000, now_ms()).unwrap();
        let quit = Arc::new(AtomicBool::new(false));
        let q = quit.clone();
        let th = thread::spawn(move || serve(sock, auth, test_opts(false), q));

        let attacker = UdpSocket::bind("127.0.0.1:0").unwrap();
        for len in [2049, 3000, 9000, 65000] {
            let mut d = vec![0xABu8; len];
            attacker.send_to(&d, host_addr).unwrap(); // type inconnu
            d[0] = PKT_INIT;
            attacker.send_to(&d, host_addr).unwrap(); // faux INIT géant
            d[0] = PKT_DATA;
            attacker.send_to(&d, host_addr).unwrap(); // faux DATA géant
        }
        thread::sleep(Duration::from_millis(400));
        assert!(!th.is_finished(), "le host s'est arrêté sur un gros datagramme non authentifié");

        // Il répond toujours à un client légitime.
        let c = UdpSocket::bind("127.0.0.1:0").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let id = Identity::generate();
        let (hs, init) = ClientHandshake::start(&id, host_key, "legit", Some(&tok), now_ms()).unwrap();
        c.send_to(&init, host_addr).unwrap();
        let mut buf = vec![0u8; 4096];
        let n = c.recv(&mut buf).expect("réponse du host après les gros datagrammes");
        assert!(hs.finish(&buf[..n]).is_ok());

        quit.store(true, Ordering::SeqCst);
        th.join().unwrap().expect("le host se termine proprement");
    }

    // ── A2 ──

    #[test]
    fn a2_init_flood_is_bounded_and_legit_client_still_served() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let flood_from: SocketAddr = "127.0.0.9:4000".parse().unwrap();
        // 5000 faux INIT (octets aléatoires, type 0x01) de taille plausible, en rafale.
        let mut junk = vec![0u8; 200];
        for i in 0..5000u32 {
            rand::rngs::OsRng.fill_bytes(&mut junk);
            junk[0] = PKT_INIT;
            srv.handle_packet(&junk, flood_from);
            if i % 3 == 0 {
                srv.handle_packet(&junk[..50], flood_from); // trop petit : gratuit
            }
        }
        // Seuls ~burst-par-IP INIT ont atteint le déchiffrement.
        assert!(srv.counters.init_processed <= INIT_IP_BURST as u64 + 2, "{:?}", srv.counters);
        assert!(srv.counters.init_rate_limited >= 4900, "{:?}", srv.counters);
        assert!(srv.counters.init_bad_size >= 1600, "{:?}", srv.counters);
        // Un client légitime (autre IP) passe malgré l'inondation.
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)), "client légitime refusé pendant l'inondation");
    }

    #[test]
    fn a2_global_limit_caps_spoofed_sources() {
        let mut srv = server();
        let junk = {
            let mut j = vec![7u8; 200];
            j[0] = PKT_INIT;
            j
        };
        // 1000 adresses sources distinctes : chacune dans son quota, mais le global plafonne.
        for i in 0..1000u32 {
            let from = SocketAddr::from(([10, 0, (i / 250) as u8, (i % 250) as u8], 1000));
            srv.handle_packet(&junk, from);
        }
        assert!(srv.counters.init_processed <= INIT_GLOBAL_BURST as u64 + 2, "{:?}", srv.counters);
    }

    #[test]
    fn a2_log_limiter_emits_at_most_one_line_per_second_with_counter() {
        let mut l = LogLimiter::new();
        let t0 = Instant::now();
        assert_eq!(l.note(t0), Some(0));
        for i in 1..=100 {
            assert_eq!(l.note(t0 + Duration::from_millis(i)), None);
        }
        assert_eq!(l.note(t0 + Duration::from_millis(1100)), Some(100));
        assert_eq!(l.note(t0 + Duration::from_millis(1200)), None);
    }

    #[test]
    fn a2_token_bucket_refills() {
        let t0 = Instant::now();
        let mut b = TokenBucket::new(5.0, 10.0, t0);
        assert_eq!((0..20).filter(|_| b.take(t0)).count(), 10);
        assert!(!b.take(t0 + Duration::from_millis(100)));
        assert!(b.take(t0 + Duration::from_millis(250)));
    }

    // ── A3 ──

    #[test]
    fn a3_nothing_is_sent_or_started_before_first_authenticated_data() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        assert!(srv.active.is_none() && srv.pending.is_some());
        // Même en laissant tourner les timers, le host n'émet rien de plus (aucun média).
        for _ in 0..5 {
            srv.tick();
            thread::sleep(Duration::from_millis(5));
        }
        assert!(c.sock.set_nonblocking(true).is_ok());
        let mut buf = [0u8; 2048];
        assert!(c.sock.recv(&mut buf).is_err(), "le host a émis avant le premier DATA");
        c.sock.set_nonblocking(false).unwrap();

        // Un DATA forgé (mauvaise clé) ne valide rien.
        let mut forged = c.next_sealed();
        let last = forged.len() - 1;
        forged[last] ^= 1;
        srv.handle_packet(&forged, c.addr);
        assert!(srv.active.is_none());
        // Un DATA authentique depuis une AUTRE adresse (usurpation) ne valide rien non plus.
        let good = c.next_sealed();
        srv.handle_packet(&good, "127.0.0.1:9".parse().unwrap());
        assert!(srv.active.is_none());
        // Le premier DATA authentique du bon pair démarre la session.
        c.send_first_data(&mut srv);
        assert!(srv.active.is_some() && srv.pending.is_none());
    }

    #[test]
    fn a3_replayed_init_never_kicks_active_session() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        let (hs, init) = c.init_packet(&srv, Some(&tok));
        srv.handle_packet(&init, c.addr);
        let mut buf = vec![0u8; 4096];
        let n = c.sock.recv(&mut buf).unwrap();
        c.sess = Some(hs.finish(&buf[..n]).unwrap());
        c.send_first_data(&mut srv);
        let sid = srv.active.as_ref().unwrap().sess.sid();

        // Rejeu de l'INIT capturé (même horodatage), depuis une adresse usurpée.
        let victim: SocketAddr = "127.0.0.1:9".parse().unwrap();
        srv.handle_packet(&init, victim);
        assert!(srv.pending.is_none(), "un INIT rejoué ne doit rien créer");
        assert_eq!(srv.active.as_ref().unwrap().sess.sid(), sid);
    }

    #[test]
    fn a3_reconnect_of_same_key_replaces_only_once_validated() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        let old_sid = srv.active.as_ref().unwrap().sess.sid();
        // Reconnexion de la même clé : en attente, l'ancienne session continue.
        assert!(c.connect(&mut srv, None));
        assert!(srv.pending.is_some());
        assert_eq!(srv.active.as_ref().unwrap().sess.sid(), old_sid);
        c.send_first_data(&mut srv);
        assert_ne!(srv.active.as_ref().unwrap().sess.sid(), old_sid);
        assert!(srv.pending.is_none());
    }

    // ── A4 ──

    #[test]
    fn a4_address_migration_requires_path_validation() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        let orig = c.addr;
        assert_eq!(srv.active.as_ref().unwrap().peer, orig);

        // Le client « change d'adresse » : même session, autre socket.
        let new_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        new_sock.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let new_addr = new_sock.local_addr().unwrap();
        let pkt = c.next_sealed();
        srv.handle_packet(&pkt, new_addr);
        // Pas de migration immédiate : le chemin n'est pas validé.
        assert_eq!(srv.active.as_ref().unwrap().peer, orig);
        // Un petit challenge a été envoyé à la nouvelle adresse (et rien d'autre).
        let mut buf = vec![0u8; 4096];
        let n = new_sock.recv(&mut buf).expect("challenge de chemin");
        assert!(n <= 64, "le challenge doit être minuscule ({n} octets)");
        let ch = c.sess.as_mut().unwrap().open(&buf[..n]).unwrap();
        assert_eq!(grx_transport::wire::kind_of(ch[0]), grx_transport::wire::K_PATH_CHALLENGE);
        new_sock.set_nonblocking(true).unwrap();
        assert!(new_sock.recv(&mut buf).is_err(), "un seul paquet vers une adresse non validée");

        // Le client répond (son Endpoint renvoie les 8 octets) depuis la nouvelle adresse.
        c.ep.handle_datagram(0, &ch);
        let resp = c.next_sealed();
        // D'abord depuis une mauvaise adresse : refusé.
        srv.handle_packet(&resp, "127.0.0.1:9".parse().unwrap());
        assert_eq!(srv.active.as_ref().unwrap().peer, orig);
        // Réponse rejouée sous un autre nonce n'existe pas : on re-fabrique un paquet avec le
        // bon token mais un challenge inventé -> refus.
        let mut fake_ep = Endpoint::new(Config::default());
        fake_ep.handle_datagram(0, &[b0(K_PATH_CHALLENGE, 0), 9, 9, 9, 9, 9, 9, 9, 9]);
        let fake = fake_ep.poll_transmit(0).unwrap();
        let fake = c.sess.as_mut().unwrap().seal(&fake).unwrap();
        srv.handle_packet(&fake, new_addr);
        assert_eq!(srv.active.as_ref().unwrap().peer, orig, "token inventé accepté");
        // Réponse correcte depuis la bonne adresse : migration. (Le paquet `resp` a déjà été
        // consommé par la fenêtre anti-rejeu quand il est arrivé d'une mauvaise adresse : on
        // refait un tour complet de challenge.)
        let pkt = c.next_sealed();
        // Le challenge précédent a expiré côté « une par seconde » : on force un nouveau challenge.
        srv.active.as_mut().unwrap().probe = None;
        srv.handle_packet(&pkt, new_addr);
        let n = new_sock.recv(&mut buf);
        new_sock.set_nonblocking(false).unwrap();
        let n = n.expect("nouveau challenge");
        let ch = c.sess.as_mut().unwrap().open(&buf[..n]).unwrap();
        c.ep.handle_datagram(0, &ch);
        for pkt in c.all_sealed() {
            srv.handle_packet(&pkt, new_addr);
        }
        assert_eq!(srv.active.as_ref().unwrap().peer, new_addr, "migration non effectuée après validation");
    }

    #[test]
    fn a4_replayed_packet_from_spoofed_address_gets_only_a_tiny_challenge() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        // Paquet authentique capturé par un tiers, rejoué (une fois) depuis l'adresse de la victime.
        let victim_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        victim_sock.set_nonblocking(true).unwrap();
        let victim = victim_sock.local_addr().unwrap();
        let captured = c.next_sealed();
        srv.handle_packet(&captured, victim);
        // Beaucoup de tentatives : toujours un challenge par seconde au plus, jamais de média.
        for _ in 0..50 {
            let p = c.next_sealed();
            srv.handle_packet(&p, victim);
            srv.tick();
        }
        assert_eq!(srv.active.as_ref().unwrap().peer, c.addr);
        let mut got = 0;
        let mut bytes = 0;
        let mut buf = vec![0u8; 4096];
        while let Ok(n) = victim_sock.recv(&mut buf) {
            got += 1;
            bytes += n;
        }
        assert_eq!(got, 1, "au plus un challenge par seconde vers une adresse non validée");
        assert!(bytes <= 64);
    }

    // ── A5 ──

    #[test]
    fn a5_other_key_cannot_take_over_a_live_session_but_same_key_can() {
        let mut srv = server();
        let t1 = invite(&mut srv);
        let t2 = invite(&mut srv);
        let mut a = Client::new();
        assert!(a.connect(&mut srv, Some(&t1)));
        a.send_first_data(&mut srv);

        // Une autre clé (même munie d'une invitation valide) est refusée en silence...
        let mut b = Client::new();
        assert!(!b.connect(&mut srv, Some(&t2)));
        assert!(srv.pending.is_none());
        // ... et son jeton n'a pas été brûlé, ni sa clé autorisée.
        assert!(!srv.auth.trust.is_authorized(&b.id.public()));
        // La même clé peut reprendre la main.
        assert!(a.connect(&mut srv, None));
        // Si la session active est silencieuse depuis > 3 s, une autre clé peut la remplacer.
        srv.pending = None;
        srv.active.as_mut().unwrap().last_rx = Instant::now().checked_sub(Duration::from_secs(4)).unwrap();
        assert!(b.connect(&mut srv, Some(&t2)));
        assert!(srv.pending.is_some());
    }

    #[test]
    fn a5_revocation_cuts_the_active_session() {
        let dir = tmp_dir("revoke");
        let p = dir.join("trust.json");
        let mut srv = server_with(TrustStore::load(&p).unwrap(), false);
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        assert!(srv.active.is_some());
        srv.tick();
        assert!(srv.active.is_some(), "client autorisé : session conservée");

        // Révocation par un autre « processus » (la CLI).
        let mut cli = TrustStore::load(&p).unwrap();
        assert_eq!(cli.revoke(&fingerprint(&c.id.public())).unwrap(), 1);
        srv.last_trust_check = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        srv.tick();
        assert!(srv.active.is_none(), "la session d'un client révoqué doit être coupée");
        // Le client reçoit un adieu (best effort).
        for p in c.drain() {
            c.ep.handle_datagram(0, &p);
        }
        let mut bye = false;
        while let Some(ev) = c.ep.poll_event() {
            bye |= matches!(ev, Event::Control(m) if m == BYE);
        }
        assert!(bye, "le client révoqué doit recevoir GRX-BYE");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a6_unreadable_trust_file_refuses_inits_and_eventually_cuts_session() {
        let dir = tmp_dir("failclosed");
        let p = dir.join("trust.json");
        let mut srv = server_with(TrustStore::load(&p).unwrap(), false);
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);

        std::fs::write(&p, b"{ corrompu, plus long que l'original ............").unwrap();
        // Un INIT d'un client connu est refusé (fail-closed).
        assert!(!c.connect(&mut srv, None), "INIT accepté alors que trust.json est illisible");
        // La session active n'est coupée qu'après plusieurs échecs consécutifs.
        for _ in 0..TRUST_MAX_FAILURES {
            srv.last_trust_check = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
            srv.tick();
        }
        assert!(srv.active.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── A7 ──

    #[test]
    fn a7_host_ignores_incoming_media_packets() {
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        // Un en-tête vidéo annonçant une énorme image (réassemblage coûteux si accepté).
        let mut v = vec![b0(K_VIDEO, 1), 0, 0, 1, 0, 0, 0];
        v.extend_from_slice(&(10_000_000u32).to_le_bytes());
        v.extend_from_slice(&1182u16.to_le_bytes());
        v.extend_from_slice(&[10, 1, 0, 0, 0]);
        v.extend_from_slice(&[0u8; 64]);
        let sealed = c.sess.as_mut().unwrap().seal(&v).unwrap();
        let before = srv.active.as_ref().unwrap().ep.stats().packets_invalid;
        srv.handle_packet(&sealed, c.addr);
        let st = srv.active.as_ref().unwrap().ep.stats();
        assert_eq!(st.packets_invalid, before + 1);
        assert_eq!(st.video_rx.frames_lost, 0);
    }

    // ── A8 ──

    #[test]
    fn a8_view_only_client_never_gets_an_injection_path() {
        let dir = tmp_dir("viewonly");
        let p = dir.join("trust.json");
        let mut srv = server_with(TrustStore::load(&p).unwrap(), true); // pipeline.input = true
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        assert!(!srv.active.as_ref().unwrap().view_only);
        let fp = fingerprint(&c.id.public());
        srv.active = None; // on repart d'une session propre avec le rôle spectateur
        TrustStore::load(&p).unwrap().set_view_only(&fp, true).unwrap();

        assert!(c.connect(&mut srv, None));
        assert!(srv.pending.as_ref().unwrap().view_only);
        c.send_first_data(&mut srv);
        let a = srv.active.as_mut().unwrap();
        assert!(a.view_only);
        // Des entrées (inoffensives : roulette à zéro) arrivent : comptées, jamais injectées.
        for _ in 0..3 {
            c.ep.send_input(&grx_input::InputEvent::Wheel { horizontal: 0, vertical: 0 }.encode()).unwrap();
        }
        for sealed in c.all_sealed() {
            srv.handle_packet(&sealed, c.addr);
        }
        srv.tick();
        let a = srv.active.as_ref().unwrap();
        assert_eq!(a.pipeline.shared.inputs_injected.load(Ordering::Relaxed), 0);
        assert_eq!(a.pipeline.shared.inputs_other.load(Ordering::Relaxed), 3);

        // Le changement de rôle coupe la session (pour relâcher proprement les entrées).
        TrustStore::load(&p).unwrap().set_view_only(&fp, false).unwrap();
        srv.last_trust_check = Instant::now().checked_sub(Duration::from_secs(2)).unwrap();
        srv.tick();
        assert!(srv.active.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_settings_apply_even_for_view_only_client_and_are_acknowledged() {
        use grx_core::stream_settings::{HostInfo, StreamSettings};
        let dir = tmp_dir("settings");
        let p = dir.join("trust.json");
        let mut srv = server_with(TrustStore::load(&p).unwrap(), false);
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        let fp = fingerprint(&c.id.public());
        TrustStore::load(&p).unwrap().set_view_only(&fp, true).unwrap();
        assert!(c.connect(&mut srv, None));
        c.send_first_data(&mut srv);
        assert!(srv.active.as_ref().unwrap().view_only);

        let host_infos = |c: &mut Client, srv: &mut Server| {
            srv.tick();
            for p in c.drain() {
                c.ep.handle_datagram(c.clock, &p);
            }
            let mut v = Vec::new();
            while let Some(ev) = c.ep.poll_event() {
                if let Event::Control(m) = ev {
                    v.extend(HostInfo::decode(&m));
                }
            }
            v
        };
        // Annonce initiale : limites du host, aucun plafond.
        let first = host_infos(&mut c, &mut srv);
        assert_eq!(first.last().map(|h| (h.host_fps, h.host_max_bitrate_kbps, h.bitrate_cap_kbps)), Some((60, 20_000, None)));

        // Réglage, plus un message malformé qui doit être ignoré sans effet.
        c.ep.send_control(b"GRX-SET	").unwrap();
        c.ep.send_control(&StreamSettings { max_bitrate_kbps: Some(4_000), max_fps: Some(30) }.encode()).unwrap();
        for sealed in c.all_sealed() {
            srv.handle_packet(&sealed, c.addr);
        }
        let infos = host_infos(&mut c, &mut srv);
        assert_eq!(infos.last().map(|h| (h.bitrate_cap_kbps, h.fps_cap)), Some((Some(4_000), Some(30))));
        let a = srv.active.as_ref().unwrap();
        assert!(a.ep.stats().total_bitrate_bps <= 4_000_000);
        assert_eq!(a.pipeline.shared.max_fps.load(Ordering::Relaxed), 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_list_is_announced_and_selection_reaches_video_thread() {
        use grx_core::displays::{DisplayList, DisplaySelect};
        let mut srv = server();
        let tok = invite(&mut srv);
        let mut c = Client::new();
        assert!(c.connect(&mut srv, Some(&tok)));
        c.send_first_data(&mut srv);
        let lists = |c: &mut Client, srv: &mut Server| {
            srv.tick();
            for p in c.drain() {
                c.ep.handle_datagram(c.clock, &p);
            }
            let mut v = Vec::new();
            while let Some(ev) = c.ep.poll_event() {
                if let Event::Control(m) = ev {
                    v.extend(DisplayList::decode(&m));
                }
            }
            v
        };
        assert!(lists(&mut c, &mut srv).is_empty(), "rien tant que la liste est inconnue (pas de capture)");

        // Le thread vidéo publierait ceci : deux écrans, le second capturé.
        let out = |name: &str, left: i32, oi: u32| grx_video::OutputInfo {
            adapter_index: 0,
            output_index: oi,
            adapter_name: "GPU".into(),
            vendor_id: 0,
            width: 2560,
            height: 1440,
            left,
            top: 0,
            attached_to_desktop: true,
            primary: left == 0,
            device_name: name.into(),
            friendly_name: String::new(),
            is_virtual: false,
        };
        let d = srv.active.as_ref().unwrap().pipeline.shared.displays.clone();
        d.set_list(vec![out("DISPLAY2", -2560, 1), out("DISPLAY1", 0, 0)]);
        d.set_active("DISPLAY1");
        let l = lists(&mut c, &mut srv);
        assert_eq!(l.len(), 1, "une seule annonce par changement");
        assert_eq!((l[0].displays.len(), l[0].active), (2, Some(1)));
        assert!(lists(&mut c, &mut srv).is_empty(), "pas de réannonce sans changement");

        // Choix du client (plus un message malformé ignoré) -> demande pour le thread vidéo.
        c.ep.send_control(&[b"GRX-MON".as_slice(), &[1, 99]].concat()).unwrap();
        c.ep.send_control(&DisplaySelect { index: 0 }.encode()).unwrap();
        for sealed in c.all_sealed() {
            srv.handle_packet(&sealed, c.addr);
        }
        srv.tick();
        assert_eq!(d.take_request().map(|o| o.output_index), Some(1));
        assert!(d.take_request().is_none());
    }

    #[test]
    fn a8_session_pipeline_opts_disable_input_for_view_only() {
        let base = test_opts(true).pipeline;
        let ep = Endpoint::new(Config::default());
        assert!(session_pipeline_opts(&base, &ep, false).input);
        assert!(!session_pipeline_opts(&base, &ep, true).input);
    }
}

#![allow(dead_code)] // API destinée à être réutilisée par le client fenêtré
//! Connexion réseau côté client, réutilisable par un client fenêtré ou headless.
//!
//! [`ClientConnection::connect`] effectue la poignée de main Noise (bloquante), puis lance un
//! thread réseau propriétaire de la socket UDP, de la [`Session`] (chiffrement) et de
//! l'[`Endpoint`] de transport. L'application parle à ce thread par des files :
//!
//! ```text
//! let conn = ClientConnection::connect(&identity, &invite, ConnectOptions::default())?;
//! conn.request_keyframe();
//! while conn.is_connected() {
//!     if let Some(frame) = conn.recv_video_timeout(Duration::from_millis(5)) { /* décoder frame.data */ }
//!     while let Some(pkt) = conn.poll_audio() { /* Opus -> JitterBuffer */ }
//!     conn.send_input(&input_event.encode());           // fiable, ordonné
//!     let s = conn.stats();                              // RTT, perte, débit...
//! }
//! ```
//!
//! Garanties : toutes les méthodes sont non bloquantes (sauf `recv_*_timeout`), `Sync`
//! (partageables entre threads via `Arc`), et la connexion se ferme proprement au `Drop`
//! (message d'adieu au host, qui libère alors sa session tout de suite).

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use grx_core::util::{is_transient_recv_error, now_ms};
use grx_core::{fingerprint, ClientHandshake, Identity, Invite, Session, PKT_DATA, PKT_RESP};
use grx_transport::sim::Rng;
pub use grx_transport::{AudioPacket, VideoFrame};
use grx_transport::{Config, Endpoint, Event, Micros, Stats};

/// Message `Control` envoyé par le client quand il se ferme : le host libère la session.
pub const BYE: &[u8] = b"GRX-BYE";

/// Microsecondes depuis l'epoch Unix : même horloge que le `pts_us` des images du host.
pub fn unix_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct ConnectOptions {
    /// Nom annoncé au host.
    pub name: String,
    /// Perte simulée (0..1) appliquée aux datagrammes reçus ET émis (test uniquement).
    pub sim_loss: f64,
    pub handshake_attempts: u32,
    pub handshake_timeout: Duration,
    /// Aucun paquet authentifié du host pendant ce délai : la connexion est fermée.
    pub idle_timeout: Duration,
    /// Images vidéo conservées en attente du consommateur avant purge (jusqu'à l'image clé suivante).
    pub video_queue: usize,
    pub audio_queue: usize,
    /// Adresses du host à essayer dans l'ordre (host mémorisé à plusieurs adresses) ; vide =
    /// l'adresse de l'invitation. La clé du host est épinglée : une adresse qui mène ailleurs
    /// échoue à la poignée de main, elle ne peut pas usurper le host.
    pub addrs: Vec<String>,
    /// Délai par adresse au premier tour quand il y en a plusieurs (les suivants : `handshake_timeout`).
    pub first_round_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            name: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "client".into()),
            sim_loss: 0.0,
            handshake_attempts: 3,
            handshake_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(5),
            video_queue: 120,
            audio_queue: 200,
            addrs: Vec::new(),
            first_round_timeout: Duration::from_millis(800),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    /// Fermée (timeout, `close()`, erreur) avec la raison.
    Closed(String),
}

/// Instantané des compteurs de la connexion.
#[derive(Clone, Debug, Default)]
pub struct ConnStats {
    /// Statistiques complètes du transport (RTT, perte, débit reçu, vidéo_rx...).
    pub transport: Stats,
    pub video_frames: u64,
    pub video_keyframes: u64,
    /// Demandes d'image clé faites par l'application (démarrage, erreur de décodage, attente d'IDR).
    pub keyframe_requests_app: u64,
    pub video_bytes: u64,
    /// Images purgées car le consommateur ne suivait pas (voir `ConnectOptions::video_queue`).
    pub video_overflow_dropped: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    pub input_sent: u64,
    /// Messages d'entrée envoyés mais pas encore acquittés par le host.
    pub input_unacked: usize,
    /// Datagrammes jetés par la simulation de perte (rx, tx).
    pub sim_dropped_rx: u64,
    pub sim_dropped_tx: u64,
    /// Datagrammes reçus mais rejetés (forgés/rejoués).
    pub rejected: u64,
    pub uptime: Duration,
}

enum Cmd {
    Input(Vec<u8>),
    Control(Vec<u8>),
    Keyframe,
}

/// File bornée des vibrations en attente (une par manette suffit).
const RUMBLE_QUEUE: usize = grx_input::MAX_GAMEPADS;

struct Shared {
    video: Mutex<VecDeque<VideoFrame>>,
    video_cv: Condvar,
    audio: Mutex<VecDeque<AudioPacket>>,
    audio_cv: Condvar,
    state: Mutex<ConnState>,
    stats: Mutex<ConnStats>,
    stop: AtomicBool,
    /// Curseur distant (forme + position), voir `cursor.rs`.
    cursor: Arc<crate::cursor::CursorInbox>,
    /// Dernière annonce `GRX-HST` du host (limites et plafonds appliqués), pour l'overlay.
    host_info: Mutex<Option<grx_core::stream_settings::HostInfo>>,
    /// Vibrations reçues du host (`GRX-RMB`), consommées par le fil des manettes.
    rumble: Mutex<VecDeque<grx_input::Rumble>>,
    /// Dernière liste d'écrans du host (`GRX-DSP`) et compteur de réceptions (0 = jamais reçue).
    displays: Mutex<(u64, Option<grx_core::displays::DisplayList>)>,
    /// Dernier état de sécurité du host (`GRX-SEC`) et compteur de réceptions (0 = jamais reçu),
    /// pour le bandeau de l'overlay.
    security: Mutex<(u64, Option<grx_core::secure_notice::SecureNotice>)>,
    /// Dernière annonce de codec du host (`GRX-CDC`) et compteur de réceptions (0 = jamais reçue :
    /// ancien host sans négociation).
    host_codec: Mutex<(u64, Option<grx_core::codec_neg::HostCodec>)>,
}

pub struct ClientConnection {
    shared: Arc<Shared>,
    cmd_tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    host_name: String,
    host_fingerprint: String,
    session_id: u32,
    remote: SocketAddr,
    /// Adresse (telle que mémorisée) qui a servi.
    addr_used: String,
}

/// Poignée de main sur `sock` (déjà connectée) jusqu'à `timeout`. `Ok(None)` = pas de réponse.
fn handshake_once(
    sock: &UdpSocket,
    buf: &mut [u8],
    id: &Identity,
    inv: &Invite,
    name: &str,
    timeout: Duration,
) -> Result<Option<Session>> {
    // Le host ignore un INIT rejoué (même horodatage) : chaque essai repart d'une poignée neuve.
    let (hs, init) = ClientHandshake::start(id, inv.host_key, name, inv.token.as_ref(), now_ms())?;
    sock.send(&init)?;
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        sock.set_read_timeout(Some(left))?;
        match sock.recv(buf) {
            Ok(n) => {
                if n == 0 || buf[0] != PKT_RESP {
                    continue; // paquet parasite : on continue d'attendre la réponse
                }
                // Réponse indéchiffrable (autre host, paquet forgé) : cette adresse échoue, on
                // passe à la suivante sans abandonner la connexion.
                return match hs.finish(&buf[..n]) {
                    Ok(s) => Ok(Some(s)),
                    Err(e) => {
                        eprintln!("! réponse invalide ignorée : {e}");
                        Ok(None)
                    }
                };
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => return Ok(None),
            Err(e) if is_transient_recv_error(&e) => {
                thread::sleep(Duration::from_millis(100)); // port fermé, datagramme tronqué... : essai suivant
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Socket UDP connectée à `addr` (résolution DNS comprise).
fn socket_for(addr: &str) -> Result<(UdpSocket, SocketAddr)> {
    let remote = addr
        .to_socket_addrs()
        .with_context(|| format!("résolution de {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("adresse {addr} introuvable"))?;
    let sock = UdpSocket::bind(if remote.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    sock.connect(remote)?;
    {
        let r = socket2::SockRef::from(&sock);
        let _ = r.set_recv_buffer_size(4 << 20);
        let _ = r.set_send_buffer_size(1 << 20);
    }
    Ok((sock, remote))
}

impl ClientConnection {
    /// Poignée de main (bloquante, `handshake_attempts` tours) puis démarrage du thread réseau.
    ///
    /// Avec plusieurs adresses (`opts.addrs`), elles sont essayées dans l'ordre, une à la fois (un
    /// seul INIT en vol : le host ne garde qu'une session en attente), avec un délai court au
    /// premier tour. La clé du host étant épinglée, seule une réponse du vrai host est acceptée.
    ///
    /// Échoue si le host ne répond pas : il ne répond jamais aux clients non autorisés.
    pub fn connect(id: &Identity, inv: &Invite, opts: ConnectOptions) -> Result<Self> {
        let addrs: Vec<String> = if opts.addrs.is_empty() { vec![inv.addr.clone()] } else { opts.addrs.clone() };
        let multi = addrs.len() > 1;
        // 64 Kio : un datagramme plus gros que le tampon fait échouer `recv` (WSAEMSGSIZE sous
        // Windows) ; un paquet parasite ne doit jamais faire échouer la connexion.
        let mut buf = vec![0u8; 65536];
        let mut found = None;
        let mut errors: Vec<String> = Vec::new();
        let rounds = opts.handshake_attempts.max(1);
        'rounds: for round in 1..=rounds {
            let timeout =
                if multi && round == 1 { opts.first_round_timeout.min(opts.handshake_timeout) } else { opts.handshake_timeout };
            for addr in &addrs {
                let (sock, remote) = match socket_for(addr) {
                    Ok(s) => s,
                    Err(e) => {
                        if round == 1 {
                            eprintln!("! {addr} : {e:#}");
                            errors.push(format!("{addr} : {e:#}"));
                        }
                        continue;
                    }
                };
                if let Some(sess) = handshake_once(&sock, &mut buf, id, inv, &opts.name, timeout)? {
                    found = Some((sock, remote, addr.clone(), sess));
                    break 'rounds;
                }
                if multi {
                    eprintln!("pas de réponse de {addr} (tour {round}/{rounds})");
                }
            }
            if !multi {
                eprintln!("pas de réponse (essai {round}/{rounds})");
            }
        }
        let Some((sock, remote, addr_used, sess)) = found else {
            if multi {
                bail!(
                    "aucune réponse sur les {} adresses de ce host ({}) : host éteint, adresses périmées, ou client non autorisé \
                     (le host ne répond jamais aux inconnus)",
                    addrs.len(),
                    addrs.join(", ")
                );
            }
            match errors.pop() {
                Some(e) => bail!("{e}"),
                None => bail!(
                    "aucune réponse : host éteint, mauvaise adresse, ou client non autorisé (le host ne répond jamais aux inconnus)"
                ),
            }
        };
        if multi {
            println!("Connecté via {addr_used}");
        }
        sock.set_nonblocking(true)?;

        let host_name = sess.peer_name().to_string();
        let host_fingerprint = fingerprint(&sess.peer_key());
        let session_id = sess.sid();
        let shared = Arc::new(Shared {
            video: Mutex::new(VecDeque::new()),
            video_cv: Condvar::new(),
            audio: Mutex::new(VecDeque::new()),
            audio_cv: Condvar::new(),
            state: Mutex::new(ConnState::Connected),
            stats: Mutex::new(ConnStats::default()),
            stop: AtomicBool::new(false),
            cursor: Default::default(),
            host_info: Mutex::new(None),
            rumble: Mutex::new(VecDeque::new()),
            displays: Mutex::new((0, None)),
            security: Mutex::new((0, None)),
            host_codec: Mutex::new((0, None)),
        });
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let sh = shared.clone();
        let thread = thread::Builder::new()
            .name("grx-client-net".into())
            .spawn(move || net_loop(sh, sock, sess, cmd_rx, opts))?;
        Ok(Self { shared, cmd_tx, thread: Some(thread), host_name, host_fingerprint, session_id, remote, addr_used })
    }

    /// Adresse (telle que mémorisée ou saisie) par laquelle la connexion s'est faite.
    pub fn addr_used(&self) -> &str {
        &self.addr_used
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }
    pub fn host_fingerprint(&self) -> &str {
        &self.host_fingerprint
    }
    pub fn session_id(&self) -> u32 {
        self.session_id
    }
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote
    }

    /// Prochaine image vidéo complète (dans l'ordre, jamais périmée), sans attendre.
    pub fn poll_video(&self) -> Option<VideoFrame> {
        self.shared.video.lock().unwrap().pop_front()
    }

    /// Comme `poll_video` mais attend jusqu'à `timeout` (réveil immédiat à l'arrivée d'une image).
    pub fn recv_video_timeout(&self, timeout: Duration) -> Option<VideoFrame> {
        let q = self.shared.video.lock().unwrap();
        let (mut q, _) = self.shared.video_cv.wait_timeout_while(q, timeout, |q| q.is_empty()).unwrap();
        q.pop_front()
    }

    /// Prochain paquet Opus (un trou de `seq` = paquet perdu ou trame silencieuse non envoyée).
    pub fn poll_audio(&self) -> Option<AudioPacket> {
        self.shared.audio.lock().unwrap().pop_front()
    }

    pub fn recv_audio_timeout(&self, timeout: Duration) -> Option<AudioPacket> {
        let q = self.shared.audio.lock().unwrap();
        let (mut q, _) = self.shared.audio_cv.wait_timeout_while(q, timeout, |q| q.is_empty()).unwrap();
        q.pop_front()
    }

    /// Envoie un événement d'entrée encodé (`grx_input::InputEvent::encode`). Canal fiable.
    /// Retourne `false` si la connexion est fermée.
    pub fn send_input(&self, encoded_event: &[u8]) -> bool {
        self.cmd_tx.send(Cmd::Input(encoded_event.to_vec())).is_ok()
    }

    /// Message applicatif sur le canal de contrôle fiable.
    pub fn send_control(&self, data: &[u8]) -> bool {
        self.cmd_tx.send(Cmd::Control(data.to_vec())).is_ok()
    }

    /// Demande une image clé au host (à faire au démarrage et après une erreur de décodage).
    pub fn request_keyframe(&self) -> bool {
        self.cmd_tx.send(Cmd::Keyframe).is_ok()
    }

    /// Curseur distant reçu du host (dernier état gagnant).
    pub fn cursor_inbox(&self) -> Arc<crate::cursor::CursorInbox> {
        self.shared.cursor.clone()
    }

    pub fn stats(&self) -> ConnStats {
        self.shared.stats.lock().unwrap().clone()
    }

    /// Dernière annonce du host (limites, plafonds appliqués) ; `None` avec un host ancien.
    /// Vibrations reçues depuis le dernier appel (au plus `RUMBLE_QUEUE` en attente).
    pub fn take_rumble(&self) -> Vec<grx_input::Rumble> {
        self.shared.rumble.lock().unwrap().drain(..).collect()
    }

    /// Dernière annonce de codec du host (`GRX-CDC`) et nombre de réceptions (0 : host sans négociation).
    pub fn host_codec(&self) -> (u64, Option<grx_core::codec_neg::HostCodec>) {
        self.shared.host_codec.lock().unwrap().clone()
    }

    pub fn host_info(&self) -> Option<grx_core::stream_settings::HostInfo> {
        *self.shared.host_info.lock().unwrap()
    }

    /// Dernière liste d'écrans annoncée par le host et son numéro de réception (0 = aucune,
    /// host ancien) : un numéro qui change = liste ou écran actif à rafraîchir dans l'overlay.
    pub fn displays(&self) -> (u64, Option<grx_core::displays::DisplayList>) {
        self.shared.displays.lock().unwrap().clone()
    }

    /// Numéro de réception de la liste d'écrans (lecture bon marché, voir [`Self::displays`]).
    pub fn displays_serial(&self) -> u64 {
        self.shared.displays.lock().unwrap().0
    }

    /// Dernier état de sécurité du host (`GRX-SEC`) et son numéro de réception (0 = jamais reçu).
    pub fn security(&self) -> (u64, Option<grx_core::secure_notice::SecureNotice>) {
        *self.shared.security.lock().unwrap()
    }

    /// Numéro de réception de l'état de sécurité (lecture bon marché).
    pub fn security_serial(&self) -> u64 {
        self.shared.security.lock().unwrap().0
    }

    pub fn state(&self) -> ConnState {
        self.shared.state.lock().unwrap().clone()
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.state(), ConnState::Connected)
    }

    /// Ferme proprement (adieu au host) et attend la fin du thread réseau.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ClientConnection {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn push_bounded<T>(q: &Mutex<VecDeque<T>>, cv: &Condvar, item: T, cap: usize) {
    let mut g = q.lock().unwrap();
    if g.len() >= cap {
        g.pop_front();
    }
    g.push_back(item);
    cv.notify_one();
}

struct Net {
    sock: UdpSocket,
    sess: Session,
    ep: Endpoint,
    t0: Instant,
    rng: Rng,
    loss: f64,
    sim_rx: u64,
    sim_tx: u64,
    rejected: u64,
}

impl Net {
    fn now_us(&self) -> Micros {
        self.t0.elapsed().as_micros() as Micros
    }

    fn flush(&mut self) -> bool {
        let now = self.now_us();
        let mut n = 0;
        while let Some(p) = self.ep.poll_transmit(now) {
            let Ok(sealed) = self.sess.seal(&p) else { return false };
            if self.loss > 0.0 && self.rng.f64() < self.loss {
                self.sim_tx += 1;
            } else if let Err(e) = self.sock.send(&sealed) {
                if !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted) {
                    return false;
                }
            }
            n += 1;
            if n >= 512 {
                break;
            }
        }
        true
    }
}

fn net_loop(sh: Arc<Shared>, sock: UdpSocket, sess: Session, cmd_rx: Receiver<Cmd>, opts: ConnectOptions) {
    let seed = unix_micros() ^ 0x5DEECE66D;
    let mut net = Net {
        sock,
        sess,
        ep: Endpoint::new(Config::default()),
        t0: Instant::now(),
        rng: Rng::new(seed),
        loss: opts.sim_loss,
        sim_rx: 0,
        sim_tx: 0,
        rejected: 0,
    };
    let started = Instant::now();
    let mut last_rx = Instant::now();
    let mut last_pub = Instant::now() - Duration::from_secs(1);
    let mut buf = vec![0u8; 65536];
    let mut st = ConnStats::default();
    let mut waiting_key = false;
    let mut reason = String::from("fermée par l'application");

    'outer: loop {
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
        // Réception.
        let mut budget = 2048;
        loop {
            match net.sock.recv(&mut buf) {
                Ok(n) => {
                    if n == 0 || buf[0] != PKT_DATA {
                        continue;
                    }
                    if net.loss > 0.0 && net.rng.f64() < net.loss {
                        net.sim_rx += 1;
                        continue;
                    }
                    match net.sess.open(&buf[..n]) {
                        Ok(payload) => {
                            last_rx = Instant::now();
                            let now = net.now_us();
                            net.ep.handle_datagram(now, &payload);
                        }
                        Err(_) => net.rejected += 1,
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                // Erreur transitoire (ICMP, datagramme tronqué...) : jamais fatale.
                Err(e) if is_transient_recv_error(&e) => {}
                Err(e) => {
                    reason = format!("erreur socket : {e}");
                    break 'outer;
                }
            }
            budget -= 1;
            if budget == 0 {
                break;
            }
        }
        // Commandes de l'application.
        while let Ok(c) = cmd_rx.try_recv() {
            match c {
                Cmd::Input(m) => {
                    if net.ep.send_input(&m).is_ok() {
                        st.input_sent += 1;
                    }
                }
                Cmd::Control(m) => {
                    let _ = net.ep.send_control(&m);
                }
                Cmd::Keyframe => {
                    st.keyframe_requests_app += 1;
                    let _ = net.ep.request_keyframe();
                }
            }
        }
        // Événements du transport.
        net.ep.tick(net.now_us());
        while let Some(ev) = net.ep.poll_event() {
            match ev {
                Event::Video(f) => {
                    if waiting_key && !f.keyframe {
                        continue;
                    }
                    waiting_key = false;
                    st.video_frames += 1;
                    st.video_keyframes += f.keyframe as u64;
                    st.video_bytes += f.data.len() as u64;
                    let mut q = sh.video.lock().unwrap();
                    if q.len() >= opts.video_queue {
                        // Consommateur à la traîne : purger (les deltas restants seraient
                        // inutilisables après un trou) et repartir d'une image clé.
                        q.clear();
                        st.video_overflow_dropped += 1;
                        if !f.keyframe {
                            waiting_key = true;
                            let _ = net.ep.request_keyframe();
                            continue;
                        }
                    }
                    q.push_back(f);
                    sh.video_cv.notify_one();
                }
                Event::Audio(p) => {
                    st.audio_packets += 1;
                    st.audio_bytes += p.data.len() as u64;
                    push_bounded(&sh.audio, &sh.audio_cv, p, opts.audio_queue);
                }
                Event::Control(m) if m == BYE => {
                    reason = "fermée par le host (session terminée ou remplacée)".into();
                    break 'outer;
                }
                Event::Control(m) if sh.cursor.on_control(&m) => {}
                Event::Control(m) if grx_core::codec_neg::is_host_codec(&m) => {
                    if let Some(c) = grx_core::codec_neg::HostCodec::decode(&m) {
                        let mut g = sh.host_codec.lock().unwrap();
                        *g = (g.0 + 1, Some(c));
                    }
                }
                Event::Control(m) if grx_input::rumble::is_rumble(&m) => {
                    if let Some(r) = grx_input::Rumble::decode(&m) {
                        let mut q = sh.rumble.lock().unwrap();
                        // Une seule valeur utile par manette : on remplace la précédente en attente.
                        q.retain(|x| x.index != r.index);
                        if q.len() < RUMBLE_QUEUE {
                            q.push_back(r);
                        }
                    }
                }
                Event::Control(m) if grx_core::stream_settings::is_host_info(&m) => {
                    if let Some(h) = grx_core::stream_settings::HostInfo::decode(&m) {
                        *sh.host_info.lock().unwrap() = Some(h);
                    }
                }
                Event::Control(m) if grx_core::displays::is_display_list(&m) => {
                    if let Some(l) = grx_core::displays::DisplayList::decode(&m) {
                        let mut d = sh.displays.lock().unwrap();
                        *d = (d.0 + 1, Some(l));
                    }
                }
                Event::Control(m) if grx_core::secure_notice::is_secure_notice(&m) => {
                    if let Some(n) = grx_core::secure_notice::SecureNotice::decode(&m) {
                        let mut s = sh.security.lock().unwrap();
                        *s = (s.0 + 1, Some(n));
                    }
                }
                Event::Datagram(d) => {
                    sh.cursor.on_datagram(&d);
                }
                _ => {}
            }
        }
        if !net.flush() {
            reason = "erreur d'envoi/chiffrement".into();
            break;
        }
        if last_rx.elapsed() > opts.idle_timeout {
            reason = format!("aucun paquet du host depuis {:.0} s", opts.idle_timeout.as_secs_f64());
            break;
        }
        if last_pub.elapsed() >= Duration::from_millis(100) {
            last_pub = Instant::now();
            publish(&sh, &net, &mut st, started);
        }
        let now = net.now_us();
        let sleep = match net.ep.next_wakeup(now) {
            Some(t) if t > now => (t - now).min(250),
            Some(_) => 0,
            None => 250,
        };
        if sleep > 0 {
            thread::sleep(Duration::from_micros(sleep));
        }
    }

    // Adieu : message de contrôle fiable (si perdu, le host expirera de lui-même).
    if sh.stop.load(Ordering::Relaxed) {
        let _ = net.ep.send_control(BYE);
        for _ in 0..3 {
            let _ = net.flush();
            thread::sleep(Duration::from_millis(2));
        }
    }
    publish(&sh, &net, &mut st, started);
    *sh.state.lock().unwrap() = ConnState::Closed(reason);
    sh.video_cv.notify_all();
    sh.audio_cv.notify_all();
}

fn publish(sh: &Shared, net: &Net, st: &mut ConnStats, started: Instant) {
    st.transport = net.ep.stats();
    st.input_unacked = net.ep.reliable_unacked().0;
    st.sim_dropped_rx = net.sim_rx;
    st.sim_dropped_tx = net.sim_tx;
    st.rejected = net.rejected;
    st.uptime = started.elapsed();
    *sh.stats.lock().unwrap() = st.clone();
}

#[cfg(test)]
mod tests {
    use super::*;
    use grx_core::{HostAuth, TrustStore};

    /// Un faux host qui inonde le client de datagrammes énormes ou parasites avant et après la
    /// poignée de main : la connexion doit s'établir et rester ouverte.
    #[test]
    fn client_survives_oversized_and_junk_datagrams() {
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_addr = host_sock.local_addr().unwrap();
        let host_id = Identity::generate();
        let host_key = host_id.public();
        let mut auth = HostAuth::new(host_id, TrustStore::in_memory(), "faux-host");
        let token = auth.trust.create_invite(60_000, now_ms()).unwrap();

        let th = thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let (n, from) = host_sock.recv_from(&mut buf).unwrap();
            // Avant la réponse : gros datagramme et paquets parasites.
            host_sock.send_to(&vec![0xFFu8; 3000], from).unwrap();
            host_sock.send_to(&[PKT_DATA], from).unwrap();
            let (resp, _sess) = auth.accept(&buf[..n], now_ms()).unwrap();
            host_sock.send_to(&resp, from).unwrap();
            // Après : datagrammes > 2048 octets (WSAEMSGSIZE avec l'ancien tampon).
            for len in [2049usize, 3000, 20_000, 65_000] {
                let mut d = vec![0xABu8; len];
                host_sock.send_to(&d, from).unwrap();
                d[0] = PKT_DATA;
                host_sock.send_to(&d, from).unwrap();
            }
            thread::sleep(Duration::from_millis(1200));
        });

        let invite = Invite { addr: host_addr.to_string(), host_key, token: Some(token) };
        let conn = ClientConnection::connect(&Identity::generate(), &invite, ConnectOptions::default())
            .expect("poignée de main malgré les datagrammes parasites");
        thread::sleep(Duration::from_millis(800));
        assert!(conn.is_connected(), "la connexion a été fermée : {:?}", conn.state());
        drop(conn);
        th.join().unwrap();
    }

    /// Host mémorisé à plusieurs adresses : une muette, une qui répond n'importe quoi, puis le
    /// vrai host. La connexion passe par la troisième et l'indique.
    #[test]
    fn multi_address_skips_mute_and_bogus_addresses() {
        let mute = UdpSocket::bind("127.0.0.1:0").unwrap();
        let bogus = UdpSocket::bind("127.0.0.1:0").unwrap();
        let host_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (mute_addr, bogus_addr, host_addr) =
            (mute.local_addr().unwrap(), bogus.local_addr().unwrap(), host_sock.local_addr().unwrap());
        let host_id = Identity::generate();
        let host_key = host_id.public();
        let client = Identity::generate();
        let mut auth = HostAuth::new(host_id, TrustStore::in_memory(), "vrai-host");
        let tok = auth.trust.create_invite(60_000, now_ms()).unwrap();
        // Pré-autorise le client (comme un host déjà appairé).
        let (_, init) = ClientHandshake::start(&client, host_key, "c", Some(&tok), now_ms()).unwrap();
        auth.accept(&init, now_ms()).unwrap();
        thread::sleep(Duration::from_millis(2));

        let bog = thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            let _ = bogus.set_read_timeout(Some(Duration::from_secs(5)));
            if let Ok((_, from)) = bogus.recv_from(&mut buf) {
                let mut junk = vec![0x55u8; 120];
                junk[0] = PKT_RESP;
                let _ = bogus.send_to(&junk, from);
            }
        });
        let th = thread::spawn(move || {
            let mut buf = vec![0u8; 65536];
            let (n, from) = host_sock.recv_from(&mut buf).unwrap();
            let (resp, _sess) = auth.accept(&buf[..n], now_ms()).unwrap();
            host_sock.send_to(&resp, from).unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let invite = Invite { addr: mute_addr.to_string(), host_key, token: None };
        let opts = ConnectOptions {
            addrs: vec![mute_addr.to_string(), bogus_addr.to_string(), host_addr.to_string()],
            first_round_timeout: Duration::from_millis(300),
            ..Default::default()
        };
        let t0 = Instant::now();
        let conn = ClientConnection::connect(&client, &invite, opts).expect("connexion via la 3e adresse");
        assert_eq!(conn.addr_used(), host_addr.to_string());
        assert_eq!(conn.remote_addr(), host_addr);
        assert!(t0.elapsed() < Duration::from_secs(2), "délai court par adresse : {:?}", t0.elapsed());
        drop(conn);
        drop(mute);
        bog.join().unwrap();
        th.join().unwrap();
    }

    /// Aucune adresse ne répond : erreur claire qui les cite toutes.
    #[test]
    fn multi_address_all_silent_lists_them() {
        let a = UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (aa, ba) = (a.local_addr().unwrap().to_string(), b.local_addr().unwrap().to_string());
        let invite = Invite { addr: aa.clone(), host_key: Identity::generate().public(), token: None };
        let opts = ConnectOptions {
            addrs: vec![aa.clone(), ba.clone()],
            handshake_attempts: 1,
            first_round_timeout: Duration::from_millis(150),
            ..Default::default()
        };
        let e = ClientConnection::connect(&Identity::generate(), &invite, opts).err().map(|e| e.to_string()).expect("échec attendu");
        assert!(e.contains(&aa) && e.contains(&ba), "{e}");
    }
}

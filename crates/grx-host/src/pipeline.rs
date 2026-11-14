//! Threads de production média d'une session : capture+encodage vidéo, capture+encodage audio,
//! injection des entrées. Ils ne touchent jamais au réseau : ils échangent avec la boucle
//! socket (`server.rs`) par des canaux et quelques atomiques.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use grx_audio::{AudioCapturer, CaptureConfig, OpusConfig, OpusEncoder};
use grx_input::{InjectorConfig, InputInjector, Rumble};
use grx_video::{Encoder, EncoderConfig, OutputInfo, ScreenCapturer};

/// Microsecondes depuis l'epoch Unix : horloge commune host/client (même machine ou NTP) qui
/// sert de `pts` pour mesurer la latence de bout en bout.
pub fn unix_micros() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0)
}

/// `pts` (epoch Unix, µs) d'un instant monotone passé.
fn pts_of(at: Instant) -> u64 {
    unix_micros().saturating_sub(at.elapsed().as_micros() as u64)
}

#[derive(Clone, Debug)]
pub struct PipelineOpts {
    /// `run --codec` : préférence du host, le codec effectif est négocié (`codec.rs`).
    pub codec: grx_core::codec_neg::CodecPref,
    pub fps: u32,
    pub fill: bool,
    pub audio: bool,
    pub input: bool,
    /// Capture+encode vidéo (faux uniquement dans les tests du serveur).
    pub video: bool,
    /// Politique de l'injecteur (catégories autorisées, filtre de la touche Windows).
    pub injector: InjectorConfig,
    pub initial_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    /// Écran de départ (nom GDI résolu depuis `--display N`), `None` = choix par défaut.
    pub display: Option<String>,
}

pub struct VideoOut {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
}

pub struct AudioOut {
    pub data: Vec<u8>,
    pub pts_us: u64,
}

/// État partagé entre la boucle réseau et les threads média.
pub struct Shared {
    pub stop: AtomicBool,
    /// Bitrate encodeur demandé par le contrôleur de débit (bits/s).
    pub target_bps: AtomicU64,
    /// Une image clé est demandée (KeyframeRequested).
    pub want_key: AtomicBool,
    // Compteurs pour le journal.
    pub enc_frames: AtomicU64,
    pub enc_bytes: AtomicU64,
    /// Somme des temps de convert+encode en µs.
    pub enc_time_us: AtomicU64,
    pub video_overflow: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_silent: AtomicU64,
    pub inputs_injected: AtomicU64,
    pub inputs_other: AtomicU64,
    pub video_ready: AtomicBool,
    /// Demande de relâchement immédiat de toutes les touches/boutons/manettes par le thread
    /// d'entrée (sécurité : passage à un état où l'injection est bloquée, pour ne pas laisser une
    /// touche « coincée » enfoncée). Remise à faux par le thread une fois le relâchement fait.
    pub release_input: AtomicBool,
    /// Plafond d'images/s demandé par le client (0 = aucun), voir `control.rs`.
    pub max_fps: AtomicU32,
    /// Curseur lu dans DXGI par le thread vidéo, envoyé à part par la boucle réseau (`cursor.rs`).
    pub cursor: Arc<grx_video::CursorTracker>,
    /// Écrans du host, écran capturé, demande de bascule du client (`display.rs`).
    pub displays: Arc<crate::display::Displays>,
    /// Négociation du codec avec le client (`codec.rs`).
    pub codec: Arc<crate::codec::CodecState>,
}

pub struct Pipeline {
    pub shared: Arc<Shared>,
    pub video_rx: Receiver<VideoOut>,
    pub audio_rx: Receiver<AudioOut>,
    /// Vibrations demandées par les jeux aux manettes virtuelles (à relayer au client).
    pub rumble_rx: Receiver<Rumble>,
    input_tx: Option<SyncSender<Vec<u8>>>,
    threads: Vec<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start(opts: &PipelineOpts) -> Pipeline {
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            target_bps: AtomicU64::new(opts.initial_bitrate_bps),
            want_key: AtomicBool::new(true),
            enc_frames: AtomicU64::new(0),
            enc_bytes: AtomicU64::new(0),
            enc_time_us: AtomicU64::new(0),
            video_overflow: AtomicU64::new(0),
            audio_packets: AtomicU64::new(0),
            audio_silent: AtomicU64::new(0),
            inputs_injected: AtomicU64::new(0),
            inputs_other: AtomicU64::new(0),
            video_ready: AtomicBool::new(false),
            release_input: AtomicBool::new(false),
            max_fps: AtomicU32::new(0),
            cursor: Arc::new(grx_video::CursorTracker::new()),
            displays: Arc::new(crate::display::Displays::new()),
            codec: Arc::new(crate::codec::CodecState::new(opts.codec)),
        });
        let mut threads = Vec::new();

        let (vtx, video_rx) = mpsc::sync_channel::<VideoOut>(8);
        if !opts.video {
            drop(vtx);
        } else {
            let sh = shared.clone();
            threads.push(
                thread::Builder::new()
                    .name("grx-displays".into())
                    .spawn(move || crate::display::watch(sh.displays.clone(), &sh.stop))
                    .expect("spawn displays"),
            );
            let (sh, o) = (shared.clone(), opts.clone());
            threads.push(
                thread::Builder::new()
                    .name("grx-video".into())
                    .spawn(move || video_thread(sh, o, vtx))
                    .expect("spawn video"),
            );
        }

        let (atx, audio_rx) = mpsc::sync_channel::<AudioOut>(64);
        if opts.audio {
            let sh = shared.clone();
            threads.push(
                thread::Builder::new().name("grx-audio".into()).spawn(move || audio_thread(sh, atx)).expect("spawn audio"),
            );
        } else {
            drop(atx);
        }

        let (rtx, rumble_rx) = mpsc::sync_channel::<Rumble>(64);
        let input_tx = if opts.input {
            let (itx, irx) = mpsc::sync_channel::<Vec<u8>>(512);
            let sh = shared.clone();
            let icfg = opts.injector.clone();
            let target = shared.displays.abs_target.clone();
            threads.push(
                thread::Builder::new()
                    .name("grx-input".into())
                    .spawn(move || input_thread(sh, irx, icfg, rtx, target))
                    .expect("spawn input"),
            );
            Some(itx)
        } else {
            None
        };
        Pipeline { shared, video_rx, audio_rx, rumble_rx, input_tx, threads }
    }

    /// Transmet un événement d'entrée reçu du réseau (jamais bloquant).
    pub fn push_input(&self, ev: Vec<u8>) {
        match &self.input_tx {
            Some(tx) => {
                if let Err(TrySendError::Full(_)) = tx.try_send(ev) {
                    self.shared.inputs_other.fetch_add(1, Ordering::Relaxed);
                }
            }
            None => {
                self.shared.inputs_other.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Arrête et attend les threads. Le thread d'entrée appelle `release_all()` en sortant.
    pub fn stop(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.input_tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

// ───────────────────────────── vidéo ─────────────────────────────

fn video_thread(sh: Arc<Shared>, o: PipelineOpts, tx: SyncSender<VideoOut>) {
    if let Err(e) = video_loop(&sh, &o, &tx) {
        eprintln!("! thread vidéo arrêté : {e}");
    }
}

/// Ouvre la capture de `target` et un encodeur à sa taille. `reuse` : capturer + encodeur en
/// cours ; s'ils sont sur le même adaptateur, le device est gardé (bascule de sortie seulement) et
/// l'encodeur aussi tant que la taille ne change pas. Sinon tout est recréé sur le bon adaptateur.
fn open_display(
    target: &OutputInfo,
    reuse: Option<(ScreenCapturer, Encoder)>,
    sh: &Shared,
    o: &PipelineOpts,
    kbps: u32,
) -> Result<(ScreenCapturer, Encoder), String> {
    let make_enc = |cap: &ScreenCapturer| {
        let (w, h) = cap.size();
        crate::codec::open_encoder(&sh.codec, |codec| {
            Encoder::with_device(cap.device(), EncoderConfig { codec, width: w, height: h, fps: o.fps, bitrate_kbps: kbps })
        })
    };
    if let Some((mut cap, mut enc)) = reuse {
        if cap.adapter_output().0 == target.adapter_index {
            cap.switch_output(target.output_index).map_err(|e| format!("capture : {e}"))?;
            let (w, h) = cap.size();
            let c = enc.config();
            if (c.width, c.height) != (w, h) {
                // Autre résolution : encodeur recréé à la taille native (le client suit via le SPS).
                drop(enc);
                enc = make_enc(&cap)?;
            } else {
                enc.request_keyframe();
            }
            return Ok((cap, enc));
        }
    }
    let mut cap = ScreenCapturer::with_output(target.adapter_index, target.output_index).map_err(|e| format!("capture : {e}"))?;
    cap.set_cursor_tracker(sh.cursor.clone());
    let enc = make_enc(&cap)?;
    Ok((cap, enc))
}

/// Curseur juste après une bascule (DXGI ne redonne la position qu'au prochain mouvement) :
/// visible seulement s'il est sur l'écran capturé, position relative à cet écran.
fn reset_cursor(sh: &Shared, target: &OutputInfo) {
    let st = sh.cursor.snapshot();
    let (hx, hy) = st.shape.as_ref().map_or((0, 0), |s| (s.hot_x as i32, s.hot_y as i32));
    let (vis, x, y) = match grx_video::cursor_screen_pos() {
        Some((showing, cx, cy)) => {
            (showing && crate::display::rect_of(target).contains(cx, cy), cx - target.left - hx, cy - target.top - hy)
        }
        None => (false, 0, 0),
    };
    sh.cursor.update(Some((vis, x, y)), None, target.width, target.height);
}

fn video_loop(sh: &Shared, o: &PipelineOpts, tx: &SyncSender<VideoOut>) -> Result<(), String> {
    let kbps = |bps: u64| (bps.clamp(500_000, o.max_bitrate_bps.max(500_000)) / 1000) as u32;
    let mut cur_kbps = kbps(sh.target_bps.load(Ordering::Relaxed));
    let displays = &sh.displays;
    let list = grx_video::list_displays().unwrap_or_default();
    // `--display N` : cet écran ; sinon le choix historique (adaptateur NVIDIA, écran principal).
    let start = o.display.as_deref().and_then(|_| crate::display::pick_initial(&list, o.display.as_deref()).cloned());
    // Codec : on attend (brièvement) les décodeurs annoncés par le client (`codec.rs`).
    sh.codec.wait_decided(crate::codec::DECIDE_TIMEOUT, &sh.stop);
    let (mut cap, mut enc) = match &start {
        Some(t) => open_display(t, None, sh, o, cur_kbps)?,
        None => {
            let mut cap = ScreenCapturer::new().map_err(|e| format!("capture : {e}"))?;
            cap.set_cursor_tracker(sh.cursor.clone());
            let (w, h) = cap.size();
            let enc = crate::codec::open_encoder(&sh.codec, |codec| {
                Encoder::with_device(
                    cap.device(),
                    EncoderConfig { codec, width: w, height: h, fps: o.fps, bitrate_kbps: cur_kbps },
                )
            })?;
            (cap, enc)
        }
    };
    let mut active = {
        let (a, i) = cap.adapter_output();
        list.iter().find(|d| (d.adapter_index, d.output_index) == (a, i)).cloned()
    };
    displays.set_list(list.clone());
    if let Some(a) = &active {
        displays.set_active(&a.device_name);
    }
    let (w, h) = cap.size();
    println!("  vidéo : {w}x{h} {:?} {} fps, {:?}, {} kb/s initial", enc.config().codec, o.fps, enc.backend(), cur_kbps);
    for (i, d) in list.iter().enumerate() {
        let mark = if active.as_ref().is_some_and(|a| a.device_name == d.device_name) { "  <- capturé" } else { "" };
        println!("  écran {}{mark}", crate::display::describe(i, d));
    }
    for w in enc.warnings() {
        println!("  vidéo (avertissement) : {w}");
    }
    sh.video_ready.store(true, Ordering::SeqCst);
    // Bascule en cours : instant de la demande, pour mesurer demande -> première image encodée.
    let mut switch_t0: Option<(Instant, String)> = None;

    let mut period = Duration::from_secs_f64(1.0 / o.fps.max(1) as f64);
    let mut eff_fps = o.fps.max(1);
    let mut next_due = Instant::now();
    let mut last_rate_check = Instant::now();
    let mut errors = 0u32;
    let fill_ms = (1000 / o.fps.max(1)).max(1);

    while !sh.stop.load(Ordering::Relaxed) {
        // Changement de codec négocié (renégociation du client) : encodeur recréé, image clé.
        if let Some(c) = sh.codec.take_switch(enc.config().codec) {
            let (t0, old) = (Instant::now(), enc.config().codec);
            let (w, h) = cap.size();
            drop(enc);
            enc = crate::codec::open_encoder(&sh.codec, |codec| {
                Encoder::with_device(cap.device(), EncoderConfig { codec, width: w, height: h, fps: o.fps, bitrate_kbps: cur_kbps })
            })?;
            println!("  vidéo : codec {old:?} -> {:?} (demandé {c:?}, {:?}) en {:.1} ms", enc.config().codec, enc.backend(), t0.elapsed().as_secs_f64() * 1000.0);
            sh.want_key.store(true, Ordering::SeqCst);
        }
        // Bascule d'écran demandée par le client (GRX-MON) ou repli après disparition de l'écran.
        if let Some(target) = displays.take_request() {
            if active.as_ref().is_none_or(|a| a.device_name != target.device_name) {
                let t0 = Instant::now();
                match open_display(&target, Some((cap, enc)), sh, o, cur_kbps) {
                    Ok(ce) => (cap, enc) = ce,
                    Err(err) => {
                        eprintln!("  écran : bascule vers {} impossible ({err})", target.display_name());
                        // Le capturer a été consommé : on rouvre l'écran précédent (ou le principal).
                        let list = displays.list();
                        let back = active
                            .clone()
                            .filter(|a| list.iter().any(|d| d.device_name == a.device_name))
                            .or_else(|| list.iter().find(|d| d.primary).cloned())
                            .or_else(|| list.first().cloned())
                            .ok_or_else(|| "aucun écran à rouvrir".to_string())?;
                        (cap, enc) = open_display(&back, None, sh, o, cur_kbps)?;
                        displays.set_active(&back.device_name);
                        active = Some(back);
                        sh.want_key.store(true, Ordering::SeqCst);
                        continue;
                    }
                }
                let (w, h) = cap.size();
                println!(
                    "  écran : bascule vers {} ({w}x{h} @({},{})) faite en {:.1} ms",
                    target.display_name(),
                    target.left,
                    target.top,
                    t0.elapsed().as_secs_f64() * 1000.0,
                );
                reset_cursor(sh, &target);
                displays.set_active(&target.device_name);
                active = Some(target.clone());
                switch_t0 = Some((t0, target.display_name()));
                sh.want_key.store(true, Ordering::SeqCst);
                next_due = Instant::now();
                errors = 0;
            }
        }
        let got = cap.capture(if o.fill { fill_ms } else { 100 });
        let frame = match got {
            Ok(Some(f)) => {
                errors = 0;
                f
            }
            Ok(None) if o.fill => match cap.last_frame() {
                Some(f) => f,
                None => continue,
            },
            Ok(None) => continue,
            Err(e) => {
                errors += 1;
                // Écran capturé débranché/désactivé : repli sur l'écran principal.
                if errors >= 5 {
                    let list = displays.list();
                    let gone = active.as_ref().is_some_and(|a| !list.iter().any(|d| d.device_name == a.device_name));
                    if let Some(p) = list.iter().position(|d| d.primary).or((!list.is_empty()).then_some(0)).filter(|_| gone) {
                        eprintln!("  écran capturé indisponible : repli sur {}", list[p].display_name());
                        displays.request(p as u8);
                        active = None;
                        continue;
                    }
                }
                if errors > 50 {
                    return Err(format!("capture : {e}"));
                }
                eprintln!("  capture (réessai {errors}) : {e}");
                cap.reset();
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        // FPS max du client (overlay), appliqué à chaud.
        let f = crate::control::effective_fps(o.fps, sh.max_fps.load(Ordering::Relaxed));
        if f != eff_fps {
            eff_fps = f;
            period = Duration::from_secs_f64(1.0 / f as f64);
            last_rate_check = Instant::now() - Duration::from_secs(1);
            println!("  vidéo : cadence {f} img/s");
        }
        let now = Instant::now();
        if now + Duration::from_millis(1) < next_due {
            continue; // plus rapide que --fps : on saute l'image
        }
        next_due += period;
        if next_due + period < now {
            next_due = now + period;
        }

        if sh.want_key.swap(false, Ordering::SeqCst) {
            enc.request_keyframe();
        }
        if last_rate_check.elapsed() >= Duration::from_millis(250) {
            last_rate_check = Instant::now();
            let want = crate::control::scale_kbps(kbps(sh.target_bps.load(Ordering::Relaxed)), o.fps, eff_fps);
            let diff = want.abs_diff(cur_kbps);
            if diff * 100 >= cur_kbps.max(1) * 5 {
                if enc.set_bitrate(want).is_ok() {
                    cur_kbps = want;
                }
            }
        }

        let t_enc = Instant::now();
        let ef = match enc.encode(&frame) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("  encodage : {e}");
                errors += 1;
                if errors > 50 {
                    return Err(format!("encodage : {e}"));
                }
                continue;
            }
        };
        if let Some((t0, name)) = switch_t0.take() {
            println!(
                "  écran : première image de {name} encodée {:.1} ms après le début de la bascule ({}x{}, image clé : {})",
                t0.elapsed().as_secs_f64() * 1000.0,
                ef.width,
                ef.height,
                if ef.is_keyframe { "oui" } else { "non" }
            );
        }
        sh.enc_time_us.fetch_add(t_enc.elapsed().as_micros() as u64, Ordering::Relaxed);
        sh.enc_frames.fetch_add(1, Ordering::Relaxed);
        sh.enc_bytes.fetch_add(ef.data.len() as u64, Ordering::Relaxed);
        let out = VideoOut { data: ef.data, keyframe: ef.is_keyframe, pts_us: pts_of(frame.captured_at) };
        match tx.try_send(out) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // La boucle réseau ne suit pas : on jette et on repart d'une image clé.
                sh.video_overflow.fetch_add(1, Ordering::Relaxed);
                sh.want_key.store(true, Ordering::SeqCst);
            }
            Err(TrySendError::Disconnected(_)) => return Ok(()),
        }
    }
    Ok(())
}

// ───────────────────────────── audio ─────────────────────────────

fn audio_thread(sh: Arc<Shared>, tx: SyncSender<AudioOut>) {
    let cap = match AudioCapturer::start(CaptureConfig::default()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("! audio indisponible : {e}");
            return;
        }
    };
    let mut enc = match OpusEncoder::new(OpusConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("! opus : {e}");
            return;
        }
    };
    let st = cap.stats();
    println!("  audio : « {} » {} Hz {} ca -> Opus 10 ms", st.device_name, st.source_rate, st.source_channels);
    while !sh.stop.load(Ordering::Relaxed) {
        let Some(f) = cap.recv_timeout(Duration::from_millis(20)) else { continue };
        if f.silent {
            sh.audio_silent.fetch_add(1, Ordering::Relaxed);
            continue; // pas de trame silencieuse sur le réseau
        }
        match enc.encode(&f.pcm) {
            Ok(data) => {
                let out = AudioOut { data, pts_us: pts_of(f.captured_at) };
                match tx.try_send(out) {
                    Ok(()) => {
                        sh.audio_packets.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(e) => eprintln!("  opus : {e}"),
        }
    }
    cap.stop();
}

// ───────────────────────────── entrées ─────────────────────────────

/// « 0:Xbox360 1:DualShock4 » pour le journal du host.
fn describe_pads(g: &grx_input::VigemGamepads) -> String {
    (0..grx_input::MAX_GAMEPADS as u8)
        .filter_map(|i| g.plugged_kind(i).map(|k| format!("{i}:{k:?}")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Thread unique propriétaire de l'injecteur (ViGEm n'est pas `Send`, donc créé ici).
fn input_thread(
    sh: Arc<Shared>,
    rx: Receiver<Vec<u8>>,
    cfg: InjectorConfig,
    rumble: SyncSender<Rumble>,
    abs_target: Arc<grx_input::absmap::AbsTarget>,
) {
    let mut inj = InputInjector::new(cfg);
    // Souris absolue : vise l'écran capturé (bureau virtuel), suit les bascules d'écran.
    inj.keyboard_mouse_mut().abs_target = Some(abs_target);
    inj.gamepads_mut().set_rumble_sink(rumble);
    let mut pads_seen = String::new();
    loop {
        // Sécurité : relâchement demandé par la boucle réseau (fenêtre élevée / bureau sécurisé
        // au premier plan), pour ne pas laisser une touche enfoncée pendant que l'injection est
        // bloquée. Les événements déjà en file sont ignorés tant que le drapeau est levé.
        if sh.release_input.swap(false, Ordering::SeqCst) {
            inj.release_all();
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ev) => {
                match inj.inject_bytes(&ev) {
                    Ok(_) => {
                        sh.inputs_injected.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => eprintln!("  injection : {e}"),
                }
                // Journal des manettes virtuelles (branchement / débranchement / type).
                let pads = describe_pads(inj.gamepads());
                if pads != pads_seen {
                    println!("  manettes virtuelles : {}", if pads.is_empty() { "aucune" } else { &pads });
                    pads_seen = pads;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if sh.stop.load(Ordering::Relaxed) {
            break;
        }
    }
    inj.release_all();
    println!("  entrées : touches/boutons/manettes relâchés ({} événements injectés)", sh.inputs_injected.load(Ordering::Relaxed));
}

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
use grx_video::{Codec, Encoder, EncoderConfig, ScreenCapturer};

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
    pub codec: Codec,
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
    /// Plafond d'images/s demandé par le client (0 = aucun), voir `control.rs`.
    pub max_fps: AtomicU32,
    /// Curseur lu dans DXGI par le thread vidéo, envoyé à part par la boucle réseau (`cursor.rs`).
    pub cursor: Arc<grx_video::CursorTracker>,
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
            max_fps: AtomicU32::new(0),
            cursor: Arc::new(grx_video::CursorTracker::new()),
        });
        let mut threads = Vec::new();

        let (vtx, video_rx) = mpsc::sync_channel::<VideoOut>(8);
        if !opts.video {
            drop(vtx);
        } else {
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
            threads.push(
                thread::Builder::new()
                    .name("grx-input".into())
                    .spawn(move || input_thread(sh, irx, icfg, rtx))
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

fn video_loop(sh: &Shared, o: &PipelineOpts, tx: &SyncSender<VideoOut>) -> Result<(), String> {
    let mut cap = ScreenCapturer::new().map_err(|e| format!("capture : {e}"))?;
    cap.set_cursor_tracker(sh.cursor.clone());
    let (w, h) = cap.size();
    let kbps = |bps: u64| (bps.clamp(500_000, o.max_bitrate_bps.max(500_000)) / 1000) as u32;
    let mut cur_kbps = kbps(sh.target_bps.load(Ordering::Relaxed));
    let mut enc = Encoder::with_device(
        cap.device(),
        EncoderConfig { codec: o.codec, width: w, height: h, fps: o.fps, bitrate_kbps: cur_kbps },
    )
    .map_err(|e| format!("encodeur : {e}"))?;
    println!("  vidéo : {w}x{h} {:?} {} fps, {:?}, {} kb/s initial", o.codec, o.fps, enc.backend(), cur_kbps);
    for w in enc.warnings() {
        println!("  vidéo (avertissement) : {w}");
    }
    sh.video_ready.store(true, Ordering::SeqCst);

    let mut period = Duration::from_secs_f64(1.0 / o.fps.max(1) as f64);
    let mut eff_fps = o.fps.max(1);
    let mut next_due = Instant::now();
    let mut last_rate_check = Instant::now();
    let mut errors = 0u32;
    let fill_ms = (1000 / o.fps.max(1)).max(1);

    while !sh.stop.load(Ordering::Relaxed) {
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
fn input_thread(sh: Arc<Shared>, rx: Receiver<Vec<u8>>, cfg: InjectorConfig, rumble: SyncSender<Rumble>) {
    let mut inj = InputInjector::new(cfg);
    inj.gamepads_mut().set_rumble_sink(rumble);
    let mut pads_seen = String::new();
    loop {
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

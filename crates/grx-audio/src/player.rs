//! Lecture cote client : jitter buffer -> Opus -> (reechantillonneur) -> WASAPI shared event-driven.
//!
//! La lecture est cadencee par l'horloge du peripherique : a chaque evenement WASAPI on remplit l'espace
//! libre, en depilant le jitter buffer (et en decodant) seulement quand le reechantillonneur manque
//! d'entree. Ainsi la derive d'horloge host/client n'apparait que comme une lente variation de la
//! profondeur du jitter buffer ; un controleur proportionnel module le rapport de reechantillonnage
//! (+-0,5 % max) pour la ramener vers la cible.
//!
//! Format : 48 kHz stereo f32 demande avec `AUTOCONVERTPCM` (WASAPI reechantillonne si le peripherique
//! tourne a 44,1 kHz, etc.) ; en repli, format de mixage du peripherique + notre reechantillonneur cubique.

use crate::jitter::{DriftController, JitterBuffer, JitterConfig, JitterStats, Pop};
use crate::opus::{FrameDuration, OpusDecoder};
use crate::resample::StreamResampler;
use crate::winutil::{self, Kind};
use crate::{AudioError, Result, CHANNELS, SAMPLE_RATE};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::{
    initialize_mta, AudioClient, AudioRenderClient, DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration,
    Direction, Handle, SampleType, StreamMode, WaveFormat,
};

#[derive(Clone, Debug)]
pub struct PlayerConfig {
    /// Duree de trame du flux Opus recu.
    pub frame: FrameDuration,
    /// Profondeur minimale du jitter buffer (ms) ; ~20 ms par defaut, croit jusqu'a `max_jitter_ms`.
    pub min_jitter_ms: f64,
    pub max_jitter_ms: f64,
    /// Correction de derive d'horloge par reechantillonnage adaptatif.
    pub drift_correction: bool,
    /// Gain lineaire applique en sortie (1.0 = inchange).
    pub volume: f32,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self { frame: FrameDuration::Ms10, min_jitter_ms: 20.0, max_jitter_ms: 80.0, drift_correction: true, volume: 1.0 }
    }
}

#[derive(Clone, Debug, Default)]
pub struct PlayerStats {
    pub jitter: JitterStats,
    pub device_name: String,
    pub device_rate: u32,
    pub device_channels: u16,
    /// 48 kHz/stereo/f32 fournis par WASAPI (autoconvert) ; sinon repli sur le format de mixage.
    pub wasapi_converts: bool,
    /// Rapport de reechantillonnage courant relatif au nominal (1.0 = pas de correction).
    pub drift_adjust: f64,
    pub decoded_frames: u64,
    pub plc_frames: u64,
    pub fec_frames: u64,
    pub decode_errors: u64,
    /// Trames device remplies de silence faute de donnees.
    pub silence_frames: u64,
    pub reopens: u64,
    /// Latence amont estimee (jitter buffer + reechantillonneur), ms.
    pub buffered_ms: f64,
    pub last_error: Option<String>,
}

struct Shared {
    jb: Mutex<JitterBuffer>,
    stop: AtomicBool,
    stats: Mutex<PlayerStats>,
    volume: AtomicU32,
}

pub struct AudioPlayer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPlayer {
    pub fn start(cfg: PlayerConfig) -> Result<Self> {
        let shared = Arc::new(Shared {
            jb: Mutex::new(JitterBuffer::new(JitterConfig {
                frame_ms: cfg.frame.ms() as f64,
                min_ms: cfg.min_jitter_ms,
                max_ms: cfg.max_jitter_ms,
            })),
            stop: AtomicBool::new(false),
            stats: Mutex::new(PlayerStats::default()),
            volume: AtomicU32::new(cfg.volume.to_bits()),
        });
        let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("grx-audio-player".into())
            .spawn(move || run(sh, cfg, tx))
            .map_err(|e| AudioError::Device(e.to_string()))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { shared, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(AudioError::Device(e))
            }
            Err(_) => {
                shared.stop.store(true, Ordering::SeqCst);
                Err(AudioError::Device("delai depasse a l'ouverture du peripherique".into()))
            }
        }
    }

    /// Fournit un paquet Opus recu du reseau (non bloquant). `seq` : numero de sequence croissant (+1 par trame).
    pub fn push_packet(&self, seq: u32, payload: &[u8]) {
        self.shared.jb.lock().unwrap().push(seq, payload.to_vec());
    }

    pub fn set_volume(&self, v: f32) {
        self.shared.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn stats(&self) -> PlayerStats {
        let mut s = self.shared.stats.lock().unwrap().clone();
        s.jitter = self.shared.jb.lock().unwrap().stats();
        s
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Session {
    _enumerator: DeviceEnumerator,
    _reg: DeviceEventRegistration,
    client: AudioClient,
    render: AudioRenderClient,
    event: Handle,
    kind: Kind,
    channels: usize,
    rate: u32,
    converts: bool,
    changed: Arc<AtomicBool>,
    name: String,
}

fn open_session() -> std::result::Result<Session, String> {
    let e = |what: &str, err: &dyn std::fmt::Display| format!("{what}: {err}");
    let enumerator = DeviceEnumerator::new().map_err(|x| e("DeviceEnumerator", &x))?;
    let changed = Arc::new(AtomicBool::new(false));
    let mut cbs = DeviceEventCallbacks::new();
    {
        let c = changed.clone();
        cbs.set_default_device_callback(move |dir, _role, _id| {
            if dir == Direction::Render {
                c.store(true, Ordering::SeqCst);
            }
        });
    }
    let reg = enumerator.register_notification_callback(cbs).map_err(|x| e("register_notification", &x))?;
    let device = enumerator.get_default_device(Direction::Render).map_err(|x| e("get_default_device", &x))?;
    let name = device.get_friendlyname().unwrap_or_default();
    let desired = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
    let mix = device
        .get_iaudioclient()
        .and_then(|c| c.get_mixformat())
        .map_err(|x| e("get_mixformat", &x))?;
    let mut last_err = String::new();
    for (fmt, autoconvert) in [(&desired, true), (&mix, false)] {
        let mut client = device.get_iaudioclient().map_err(|x| e("get_iaudioclient", &x))?;
        let (def, _min) = client.get_device_period().map_err(|x| e("get_device_period", &x))?;
        let mode = StreamMode::EventsShared { autoconvert, buffer_duration_hns: def };
        if let Err(err) = client.initialize_client(fmt, Direction::Render, mode, None) {
            last_err = e("initialize_client", &err);
            continue;
        }
        let Some(kind) = winutil::kind_of(fmt) else {
            last_err = "format d'echantillon non gere".into();
            continue;
        };
        let event = client.set_get_eventhandle().map_err(|x| e("eventhandle", &x))?;
        let render = client.get_audiorenderclient().map_err(|x| e("get_audiorenderclient", &x))?;
        return Ok(Session {
            _enumerator: enumerator,
            _reg: reg,
            client,
            render,
            event,
            kind,
            channels: fmt.get_nchannels() as usize,
            rate: fmt.get_samplespersec(),
            converts: autoconvert,
            changed,
            name,
        });
    }
    Err(last_err)
}

fn run(shared: Arc<Shared>, cfg: PlayerConfig, first: mpsc::Sender<std::result::Result<(), String>>) {
    let _ = initialize_mta();
    winutil::boost_thread();
    let mut decoder = match OpusDecoder::new(CHANNELS, cfg.frame) {
        Ok(d) => d,
        Err(err) => {
            let _ = first.send(Err(err.to_string()));
            return;
        }
    };
    let mut first = Some(first);
    let mut opened_once = false;
    let mut resampler = StreamResampler::new(CHANNELS);
    let frame_ms = cfg.frame.ms() as f64;
    let mut adj: f64; // correction relative de rapport
    let mut drift: DriftController;
    let mut wbytes: Vec<u8> = Vec::new();

    while !shared.stop.load(Ordering::SeqCst) {
        let s = match open_session() {
            Ok(s) => s,
            Err(err) => {
                if let Some(tx) = first.take() {
                    let _ = tx.send(Err(err));
                    return;
                }
                shared.stats.lock().unwrap().last_error = Some(err);
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
        };
        {
            let mut st = shared.stats.lock().unwrap();
            st.device_name = s.name.clone();
            st.device_rate = s.rate;
            st.device_channels = s.channels as u16;
            st.wasapi_converts = s.converts;
            st.last_error = None;
            if opened_once {
                st.reopens += 1;
            }
        }
        opened_once = true;
        if let Some(tx) = first.take() {
            let _ = tx.send(Ok(()));
        }
        resampler.reset();
        drift = DriftController::default();
        adj = 0.0;
        let base_ratio = if s.converts { 1.0 } else { SAMPLE_RATE as f64 / s.rate as f64 };

        // Remplit le tampon de silence avant le demarrage (recommande par WASAPI).
        let bytes_per_frame = s.kind.bytes() * s.channels;
        if let Ok(avail) = s.client.get_available_space_in_frames() {
            let _ = s.render.write_to_device(&vec![0u8; avail as usize * bytes_per_frame], None);
        }
        if let Err(err) = s.client.start_stream() {
            shared.stats.lock().unwrap().last_error = Some(format!("start_stream: {err}"));
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }

        let mut last_event = Instant::now();
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            if s.changed.load(Ordering::SeqCst) {
                break; // le peripherique par defaut a change : on rouvre
            }
            if s.event.wait_for_event(100).is_err() {
                if last_event.elapsed() > Duration::from_secs(2) {
                    break; // peripherique fige : on rouvre
                }
                continue;
            }
            last_event = Instant::now();
            let avail = match s.client.get_available_space_in_frames() {
                Ok(a) => a as usize,
                Err(err) => {
                    shared.stats.lock().unwrap().last_error = Some(format!("available_space: {err}"));
                    break;
                }
            };
            if avail == 0 {
                continue;
            }
            let ratio = base_ratio * (1.0 + adj);
            let mut pcm: Vec<f32> = Vec::with_capacity(avail * 2);
            let mut st_delta = (0u64, 0u64, 0u64, 0u64); // decoded, plc, fec, errors
            while pcm.len() / 2 < avail {
                let want = avail - pcm.len() / 2;
                resampler.produce(ratio, want, &mut pcm);
                if pcm.len() / 2 >= avail {
                    break;
                }
                let popped = shared.jb.lock().unwrap().pop();
                let decoded = match popped {
                    Pop::Buffering => break,
                    Pop::Packet(d) => {
                        st_delta.0 += 1;
                        decoder.decode(Some(&d))
                    }
                    Pop::Lost { fec: Some(next) } => {
                        st_delta.2 += 1;
                        decoder.decode_fec(&next)
                    }
                    Pop::Lost { fec: None } => {
                        st_delta.1 += 1;
                        decoder.decode(None)
                    }
                };
                match decoded {
                    Ok(frames) => resampler.push(&frames),
                    Err(_) => st_delta.3 += 1,
                }
            }
            let silence = avail - pcm.len() / 2;
            pcm.resize(avail * 2, 0.0);

            // contrôle de derive : profondeur lissee (en trames) vs cible
            let (depth, target) = {
                let jb = shared.jb.lock().unwrap();
                (jb.depth() as f64, jb.target_frames() as f64)
            };
            let staging = resampler.buffered_frames() / (SAMPLE_RATE as f64 * frame_ms / 1000.0);
            let active = cfg.drift_correction && shared.jb.lock().unwrap().is_started();
            adj = drift.update(depth + staging, target, active);

            wbytes.clear();
            let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
            winutil::from_stereo_f32(&pcm, s.kind, s.channels, vol, &mut wbytes);
            if let Err(err) = s.render.write_to_device(&wbytes, None) {
                shared.stats.lock().unwrap().last_error = Some(format!("write: {err}"));
                break;
            }

            let mut st = shared.stats.lock().unwrap();
            st.decoded_frames += st_delta.0;
            st.plc_frames += st_delta.1;
            st.fec_frames += st_delta.2;
            st.decode_errors += st_delta.3;
            st.silence_frames += silence as u64;
            st.drift_adjust = adj;
            st.buffered_ms = (depth + staging) * frame_ms;
        }
        let _ = s.client.stop_stream();
    }
}

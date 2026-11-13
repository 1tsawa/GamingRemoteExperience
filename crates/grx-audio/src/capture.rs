//! Capture du son systeme (WASAPI loopback du peripherique de rendu par defaut).
//!
//! Sortie : trames de `frame` ms (10 ms par defaut), PCM f32 stereo entrelace 48 kHz.
//!
//! * Format : on demande directement 48 kHz / stereo / f32 avec `AUTOCONVERTPCM` (WASAPI convertit) ;
//!   en repli on prend le format de mixage du peripherique et on convertit/reechantillonne nous-memes.
//! * Silence : le loopback ne livre RIEN quand plus aucune application ne joue. Le thread genere alors des
//!   trames de zeros cadencees sur l'horloge systeme (`synthesized = true`, `silent = true`).
//! * Changement de peripherique par defaut (ou peripherique invalide/debranche) : detecte via
//!   `IMMNotificationClient` et via les erreurs de lecture ; la session est rouverte sur le nouveau
//!   peripherique, avec des trames de silence pendant la transition (le flux reste continu).

use crate::opus::FrameDuration;
use crate::resample::StreamResampler;
use crate::winutil::{self, Kind};
use crate::{AudioError, Result, CHANNELS, SAMPLE_RATE};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, DeviceEnumerator, DeviceEventCallbacks, DeviceEventRegistration,
    Direction, Handle, SampleType, StreamMode, WaveFormat,
};

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub frame: FrameDuration,
    /// Taille max de la file de sortie en trames (les plus anciennes sont jetees si le consommateur est lent).
    pub max_queue_frames: usize,
    /// Delai sans donnee WASAPI apres lequel on bascule en generation de silence.
    pub silence_after_ms: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self { frame: FrameDuration::Ms10, max_queue_frames: 50, silence_after_ms: 30 }
    }
}

#[derive(Clone, Debug)]
pub struct AudioFrame {
    /// PCM f32 stereo entrelace, `frame.samples() * 2` valeurs.
    pub pcm: Vec<f32>,
    /// Tous les echantillons sont nuls (l'appelant peut ne pas l'envoyer / utiliser le DTX).
    pub silent: bool,
    /// Trame fabriquee par nous (le loopback n'a rien livre), pas lue du peripherique.
    pub synthesized: bool,
    /// Instant ou la trame est complete cote host (lecture WASAPI achevee).
    pub captured_at: Instant,
    /// Numero de trame (consecutif, sans trou tant que le consommateur suit).
    pub index: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CaptureStats {
    pub frames: u64,
    pub synthesized_frames: u64,
    pub dropped_frames: u64,
    /// Nombre de (re)ouvertures de session apres la premiere (changement de peripherique, erreur).
    pub reopens: u64,
    pub device_name: String,
    pub source_rate: u32,
    pub source_channels: u16,
    pub event_driven: bool,
    pub last_error: Option<String>,
}

struct Shared {
    queue: Mutex<VecDeque<AudioFrame>>,
    cv: Condvar,
    stop: AtomicBool,
    stats: Mutex<CaptureStats>,
}

pub struct AudioCapturer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl AudioCapturer {
    /// Demarre la capture. Echoue si aucun peripherique de rendu ne peut etre ouvert a ce moment-la ;
    /// ensuite les pannes sont absorbees (silence + reouverture automatique).
    pub fn start(cfg: CaptureConfig) -> Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            stats: Mutex::new(CaptureStats::default()),
        });
        let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
        let sh = shared.clone();
        let thread = std::thread::Builder::new()
            .name("grx-audio-capture".into())
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

    /// Attend la prochaine trame (au plus `timeout`).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<AudioFrame> {
        let mut q = self.shared.queue.lock().unwrap();
        if let Some(f) = q.pop_front() {
            return Some(f);
        }
        let (mut q, _) = self.shared.cv.wait_timeout_while(q, timeout, |q| q.is_empty()).unwrap();
        q.pop_front()
    }

    pub fn try_recv(&self) -> Option<AudioFrame> {
        self.shared.queue.lock().unwrap().pop_front()
    }

    pub fn stats(&self) -> CaptureStats {
        self.shared.stats.lock().unwrap().clone()
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

impl Drop for AudioCapturer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Session {
    _enumerator: DeviceEnumerator,
    _reg: DeviceEventRegistration,
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Option<Handle>,
    kind: Kind,
    channels: usize,
    rate: u32,
    changed: Arc<AtomicBool>,
    name: String,
    resampler: Option<StreamResampler>,
    raw: VecDeque<u8>,
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

    // 1) 48 kHz stereo f32 converti par WASAPI ; 2) format de mixage natif
    let desired = WaveFormat::new(32, 32, SampleType::Float, SAMPLE_RATE as usize, CHANNELS, None);
    let mix = {
        let c = device.get_iaudioclient().map_err(|x| e("get_iaudioclient", &x))?;
        c.get_mixformat().map_err(|x| e("get_mixformat", &x))?
    };
    let mut last_err = String::new();
    for (fmt, autoconvert) in [(&desired, true), (&mix, false)] {
        for events in [true, false] {
            let mut client = device.get_iaudioclient().map_err(|x| e("get_iaudioclient", &x))?;
            let (def, _min) = client.get_device_period().map_err(|x| e("get_device_period", &x))?;
            let mode = if events {
                StreamMode::EventsShared { autoconvert, buffer_duration_hns: def }
            } else {
                StreamMode::PollingShared { autoconvert, buffer_duration_hns: def }
            };
            if let Err(err) = client.initialize_client(fmt, Direction::Capture, mode, None) {
                last_err = e("initialize_client", &err);
                continue;
            }
            let Some(kind) = winutil::kind_of(fmt) else {
                last_err = "format d'echantillon non gere".into();
                continue;
            };
            let event = if events { Some(client.set_get_eventhandle().map_err(|x| e("eventhandle", &x))?) } else { None };
            let capture = client.get_audiocaptureclient().map_err(|x| e("get_audiocaptureclient", &x))?;
            client.start_stream().map_err(|x| e("start_stream", &x))?;
            let rate = fmt.get_samplespersec();
            return Ok(Session {
                _enumerator: enumerator,
                _reg: reg,
                client,
                capture,
                event,
                kind,
                channels: fmt.get_nchannels() as usize,
                rate,
                changed,
                name,
                resampler: if rate != SAMPLE_RATE { Some(StreamResampler::new(2)) } else { None },
                raw: VecDeque::new(),
            });
        }
    }
    Err(last_err)
}

struct Out<'a> {
    shared: &'a Shared,
    cfg: &'a CaptureConfig,
    index: u64,
}

impl Out<'_> {
    fn emit(&mut self, pcm: Vec<f32>, synthesized: bool) {
        let silent = pcm.iter().all(|&x| x == 0.0);
        let f = AudioFrame { pcm, silent, synthesized, captured_at: Instant::now(), index: self.index };
        self.index += 1;
        let mut dropped = 0;
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.push_back(f);
            while q.len() > self.cfg.max_queue_frames {
                q.pop_front();
                dropped += 1;
            }
        }
        self.shared.cv.notify_one();
        let mut s = self.shared.stats.lock().unwrap();
        s.frames += 1;
        s.dropped_frames += dropped;
        if synthesized {
            s.synthesized_frames += 1;
        }
    }
}

fn run(shared: Arc<Shared>, cfg: CaptureConfig, first: mpsc::Sender<std::result::Result<(), String>>) {
    let _ = initialize_mta();
    winutil::boost_thread();
    let frame_len = cfg.frame.samples() * CHANNELS;
    let frame_dur = Duration::from_millis(cfg.frame.ms() as u64);
    let silence_after = Duration::from_millis(cfg.silence_after_ms);
    let mut out = Out { shared: &shared, cfg: &cfg, index: 0 };
    let mut pending: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut session: Option<Session> = None;
    let mut first = Some(first);
    let mut opened_once = false;
    let mut last_data = Instant::now();
    let mut silence_anchor: Option<Instant> = None;
    let mut silence_emitted: u64 = 0;
    let mut next_retry = Instant::now();

    while !shared.stop.load(Ordering::SeqCst) {
        // (re)ouverture de session
        if session.is_none() && Instant::now() >= next_retry {
            match open_session() {
                Ok(s) => {
                    {
                        let mut st = shared.stats.lock().unwrap();
                        st.device_name = s.name.clone();
                        st.source_rate = s.rate;
                        st.source_channels = s.channels as u16;
                        st.event_driven = s.event.is_some();
                        if opened_once {
                            st.reopens += 1;
                        }
                        st.last_error = None;
                    }
                    opened_once = true;
                    if let Some(tx) = first.take() {
                        let _ = tx.send(Ok(()));
                    }
                    last_data = Instant::now();
                    session = Some(s);
                }
                Err(err) => {
                    if let Some(tx) = first.take() {
                        let _ = tx.send(Err(err));
                        return;
                    }
                    shared.stats.lock().unwrap().last_error = Some(err);
                    next_retry = Instant::now() + Duration::from_millis(250);
                }
            }
        }

        let mut got_data = false;
        let mut drop_session = false;
        if let Some(s) = session.as_mut() {
            match &s.event {
                Some(ev) => {
                    let _ = ev.wait_for_event(cfg.frame.ms());
                }
                None => std::thread::sleep(Duration::from_millis(2)),
            }
            // vide tous les paquets disponibles
            loop {
                match s.capture.get_next_packet_size() {
                    Ok(Some(n)) if n > 0 => {}
                    Ok(_) => break,
                    Err(err) => {
                        shared.stats.lock().unwrap().last_error = Some(format!("get_next_packet_size: {err}"));
                        drop_session = true;
                        break;
                    }
                }
                s.raw.clear();
                if let Err(err) = s.capture.read_from_device_to_deque(&mut s.raw) {
                    shared.stats.lock().unwrap().last_error = Some(format!("read: {err}"));
                    drop_session = true;
                    break;
                }
                let bytes = s.raw.make_contiguous();
                let mut stereo = Vec::with_capacity(bytes.len() / s.kind.bytes());
                winutil::to_stereo_f32(bytes, s.kind, s.channels, &mut stereo);
                match s.resampler.as_mut() {
                    Some(r) => {
                        r.push(&stereo);
                        r.produce(s.rate as f64 / SAMPLE_RATE as f64, usize::MAX, &mut pending);
                    }
                    None => pending.extend_from_slice(&stereo),
                }
                got_data = true;
            }
            if s.changed.load(Ordering::SeqCst) {
                drop_session = true;
            }
        } else {
            std::thread::sleep(Duration::from_millis(2));
        }

        if got_data {
            last_data = Instant::now();
            silence_anchor = None;
        }
        if drop_session {
            if let Some(s) = session.take() {
                let _ = s.client.stop_stream();
            }
            next_retry = Instant::now() + Duration::from_millis(50);
        }

        // trames completes lues du peripherique
        while pending.len() >= frame_len {
            let rest = pending.split_off(frame_len);
            let frame = std::mem::replace(&mut pending, rest);
            out.emit(frame, false);
        }

        // silence : plus de donnees (ou pas de session) -> trames de zeros au rythme de l'horloge
        let now = Instant::now();
        if session.is_none() || now.duration_since(last_data) >= silence_after {
            if silence_anchor.is_none() {
                if !pending.is_empty() {
                    pending.resize(frame_len, 0.0);
                    let f = std::mem::take(&mut pending);
                    out.emit(f, false);
                }
                silence_anchor = Some(now);
                silence_emitted = 0;
            }
            let due = (now.duration_since(silence_anchor.unwrap()).as_micros() / frame_dur.as_micros()) as u64;
            while silence_emitted < due {
                out.emit(vec![0.0; frame_len], true);
                silence_emitted += 1;
            }
        }
    }
    if let Some(s) = session.take() {
        let _ = s.client.stop_stream();
    }
}

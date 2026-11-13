//! `grx-client play` : fenêtre + décodage matériel + audio + entrées.
//!
//! Threads :
//! * principal : `Presenter::run` (fenêtre, boucle de messages) ; le hook y convertit les
//!   événements en `InputEvent` ([`crate::input`]) ;
//! * `grx-client-video` : `recv_video_timeout` -> `VideoDecoder` (créé sur ce thread via
//!   `PresenterHandle::create_decoder`) -> `PresenterHandle::submit`, statistiques, demandes d'IDR ;
//! * `grx-client-audio` : `recv_audio_timeout` -> `AudioPlayer::push_packet` ;
//! * `grx-client-input` : fusion et envoi des mouvements (voir [`crate::input::InputSender`]) ;
//! * `grx-client-net` : réseau (dans [`crate::net`]).
//!
//! Raccourcis réservés au client (jamais transmis au host) : Ctrl+Maj+M (menu en overlay, voir
//! [`crate::overlay`]), Ctrl+Maj+S (stats à l'écran), Ctrl+Maj+Q (déconnexion), F11 (plein écran),
//! F10 (stats dans le titre), Alt+F4 (ferme la fenêtre) et le raccourci de capture
//! (`--capture-key`, défaut Ctrl+Alt+Maj+G : bascule souris absolue <-> relative confinée).
//!
//! Limites v1 :
//! * curseur : le host n'incruste pas son curseur dans l'image, il l'envoie à part (forme +
//!   position) et le client le dessine par-dessus la vidéo (`cursor.rs`), y compris en mode
//!   capture ; le curseur Windows local est masqué au-dessus de la vidéo (`--local-cursor` :
//!   ancien comportement, curseur local seul) ;
//! * la latence « capture->réception » suppose des horloges host/client synchrones (boucle
//!   locale ou machines synchronisées NTP, sinon la valeur est décalée) ;
//! * « décodage » = durée de l'appel MFT (le travail GPU est asynchrone).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use grx_audio::{AudioPlayer, PlayerConfig};
use grx_core::{Identity, Invite};
use grx_input::{InputEvent, MouseButton};
use grx_render::{sniff_codec, Presenter, PresenterConfig, PresenterEvent, PresenterHandle, VideoDecoder};

use crate::input::{Hotkey, InputHandler, InputSender, Msg};
use crate::net::{unix_micros, ClientConnection, ConnState, ConnectOptions};

pub struct PlayOpts {
    pub no_input: bool,
    pub no_audio: bool,
    /// Affiche une ligne de stats par seconde sur stdout (et dans le titre au départ).
    pub stats: bool,
    pub volume: f32,
    pub fullscreen: bool,
    pub vsync: bool,
    pub width: u32,
    pub height: u32,
    /// Quitte tout seul après cette durée (tests).
    pub secs: Option<f64>,
    pub capture_key: Hotkey,
    /// Envoie quelques événements synthétiques inoffensifs par le chemin normal (tests).
    pub selftest_input: bool,
    /// Fréquence maximale d'envoi des mouvements de souris.
    pub input_hz: u32,
    /// Garde le curseur Windows local au lieu de dessiner celui du host.
    pub local_cursor: bool,
    /// Ouvre le menu de l'overlay au démarrage (option cachée, tests/captures).
    pub open_overlay: bool,
    /// Affiche les statistiques de l'overlay au démarrage.
    pub overlay_stats: bool,
    /// Préférences de l'overlay (positions, réglages du menu), `None` = non mémorisées.
    pub prefs_path: Option<std::path::PathBuf>,
    /// Manettes physiques (`None` : `--no-gamepad`), voir `gamepads.rs`.
    pub gamepad: Option<crate::gamepads::PadOpts>,
}

/// Volume par défaut de `--volume` : une autre valeur sur la ligne de commande l'emporte sur la préférence.
pub const DEFAULT_VOLUME: f32 = 0.8;

#[derive(Clone, Debug)]
enum Outcome {
    /// Fenêtre fermée, Ctrl+C ou `--secs` écoulé.
    Closed,
    /// La connexion est tombée (timeout, host arrêté, session remplacée).
    Lost(String),
    Error(String),
}

/// Titre de la fenêtre : nom du host + (option) stats + état de la capture.
struct Ui {
    base: String,
    line: Mutex<String>,
    show_stats: AtomicBool,
    captured: AtomicBool,
    /// Menu en overlay + statistiques à l'écran (voir `overlay.rs`).
    ov: Mutex<crate::overlay::Overlay>,
    /// Volume effectif (bits f32) réglé par l'overlay, lu par le fil audio.
    volume: AtomicU32,
    /// Manettes détectées (ligne d'état de l'overlay) + activité (focus, menu).
    pads: Arc<crate::gamepads::PadShared>,
    /// La fenêtre a le focus clavier.
    focused: AtomicBool,
    /// Le fil des manettes tourne.
    pads_on: AtomicBool,
    /// Bascule d'écran demandée (mesure demande -> première image du nouvel écran).
    display_switch: Mutex<Option<Instant>>,
}

impl Ui {
    fn title(&self) -> String {
        let mut t = self.base.clone();
        if self.captured.load(Ordering::Relaxed) {
            t.push_str(" [souris capturée]");
        }
        if self.show_stats.load(Ordering::Relaxed) {
            let l = self.line.lock().unwrap();
            if !l.is_empty() {
                t.push_str(" | ");
                t.push_str(&l);
            }
        }
        t
    }
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn avg(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

/// Retourne le code de sortie du processus (0 : fermeture normale, 3 : connexion perdue).
pub fn run(
    id: &Identity,
    invite: &Invite,
    copts: ConnectOptions,
    p: PlayOpts,
    on_connected: impl FnOnce(&ClientConnection) -> Result<()>,
) -> Result<i32> {
    if p.selftest_input && p.no_input {
        bail!("--selftest-input est incompatible avec --no-input");
    }
    let t_conn = Instant::now();
    let conn = ClientConnection::connect(id, invite, copts)?;
    println!(
        "Authentifié auprès de « {} » ({}), session {:08x} (poignée de main {:.0} ms)",
        conn.host_name(),
        conn.host_fingerprint(),
        conn.session_id(),
        t_conn.elapsed().as_secs_f64() * 1e3
    );
    on_connected(&conn)?;
    let conn = Arc::new(conn);

    let base_title = format!("GRX - {}", conn.host_name());
    // Préférences mémorisées (fichier absent ou corrompu = défauts) ; les options explicites de la
    // ligne de commande (--vsync, --volume différent du défaut) l'emportent.
    let prefs = p.prefs_path.as_deref().map(crate::overlay_prefs::OverlayPrefs::load).unwrap_or_default();
    let mut ov = crate::overlay::Overlay::new(conn.host_name(), p.capture_key.to_string());
    ov.apply_prefs(&prefs);
    if p.volume != DEFAULT_VOLUME {
        ov.volume = p.volume;
        ov.muted = false;
    }
    ov.vsync |= p.vsync;
    ov.show_stats |= p.overlay_stats;
    ov.fullscreen = p.fullscreen;
    ov.input_enabled = !p.no_input;
    ov.audio_enabled = !p.no_audio;
    ov.capture &= !p.no_input;
    let start_volume = ov.effective_volume();
    let start_prefs = ov.prefs();
    let presenter = Presenter::new(PresenterConfig {
        title: base_title.clone(),
        width: p.width,
        height: p.height,
        fullscreen: p.fullscreen,
        vsync: ov.vsync,
        ..Default::default()
    })?;
    let handle = presenter.handle();
    let ui = Arc::new(Ui {
        base: base_title,
        line: Mutex::new(String::new()),
        show_stats: AtomicBool::new(p.stats),
        captured: AtomicBool::new(false),
        ov: Mutex::new(ov),
        volume: AtomicU32::new(start_volume.to_bits()),
        pads: Default::default(),
        focused: AtomicBool::new(true),
        pads_on: AtomicBool::new(false),
        display_switch: Mutex::new(None),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let outcome: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));

    {
        let (s, h) = (stop.clone(), handle.clone());
        let _ = ctrlc::set_handler(move || {
            s.store(true, Ordering::SeqCst);
            h.close();
        });
    }

    // Entrées.
    let sender = if p.no_input {
        None
    } else {
        let c = conn.clone();
        Some(InputSender::spawn(Duration::from_secs_f64(1.0 / p.input_hz.max(1) as f64), move |b| c.send_input(b)))
    };
    let mut handler = InputHandler::new(sender.as_ref().map(|s| s.sender()), handle.clone(), p.capture_key);
    let selftest = match (&sender, p.selftest_input) {
        (Some(s), true) => Some(spawn_selftest(s.sender(), stop.clone())),
        _ => None,
    };
    // Manettes : même fil d'envoi que le clavier (canal Input fiable, ordonné).
    let pads = match (&sender, p.gamepad.clone()) {
        (Some(s), Some(mut go)) => {
            go.loopback = conn.remote_addr().ip().is_loopback();
            let c = conn.clone();
            ui.pads_on.store(true, Ordering::Relaxed);
            Some(crate::gamepads::spawn(s.sender(), ui.pads.clone(), go, stop.clone(), move || c.take_rumble()))
        }
        _ => None,
    };

    // Audio.
    let audio = if p.no_audio {
        None
    } else {
        match AudioPlayer::start(PlayerConfig { volume: start_volume, ..Default::default() }) {
            Ok(pl) => {
                let (c, st, u) = (conn.clone(), stop.clone(), ui.clone());
                Some(thread::Builder::new().name("grx-client-audio".into()).spawn(move || audio_loop(c, pl, st, u))?)
            }
            Err(e) => {
                eprintln!("audio désactivé : {e}");
                None
            }
        }
    };

    // Vidéo.
    let video = {
        let (c, h, u, st, o) = (conn.clone(), handle.clone(), ui.clone(), stop.clone(), outcome.clone());
        let (print_stats, secs) = (p.stats, p.secs);
        thread::Builder::new()
            .name("grx-client-video".into())
            .spawn(move || video_loop(c, h, u, st, o, print_stats, secs))?
    };

    // Curseur distant (voir `cursor.rs`) : dessiné par le présentateur, curseur local masqué sur la vidéo.
    let cursor = (!p.local_cursor).then(|| crate::cursor::spawn_forwarder(conn.cursor_inbox(), handle.clone(), stop.clone(), p.stats));
    let mut hider = (!p.local_cursor).then(|| crate::cursor::LocalCursorHider::new(conn.cursor_inbox(), handle.clone()));

    println!(
        "Fenêtre ouverte. Ctrl+Maj+M : menu, Ctrl+Maj+S : stats à l'écran, F11 : plein écran, F10 : stats dans le titre, {} : capture souris{}.",
        p.capture_key,
        if p.no_input { " (désactivée : --no-input)" } else { "" }
    );

    let ui_hook = ui.clone();
    let mut glue = OverlayGlue {
        input: Default::default(),
        kbd: None,
        kbd_tx: sender.as_ref().map(|s| s.sender()),
        conn: conn.clone(),
        handle: handle.clone(),
        open_at_start: p.open_overlay,
        prefs_path: p.prefs_path.clone(),
        saved_prefs: start_prefs,
        move_cursor: false,
    };
    let run_res = presenter.run(move |ev, win| {
        if let PresenterEvent::Ready = ev {
            let captured = glue.ready(win, &ui_hook, &mut handler);
            win.set_title(&ui_hook.title());
            if let (Some(h), Some(c)) = (hider.as_mut(), captured) {
                h.update(ev, win, Some(c));
            }
            return false;
        }
        // Overlay d'abord : ce qu'il consomme n'atteint jamais l'InputHandler (donc le host).
        let ov_out = glue.event(ev, &ui_hook);
        let r = if ov_out.consumed { Default::default() } else { handler.handle(ev, win) };
        let menu_open = ui_hook.ov.lock().unwrap().menu_open;
        if r.toggle_stats {
            ui_hook.show_stats.fetch_xor(true, Ordering::Relaxed);
        }
        if let Some(c) = r.capture {
            ui_hook.captured.store(c, Ordering::Relaxed);
        }
        if r.toggle_stats || r.capture.is_some() {
            win.set_title(&ui_hook.title());
        }
        if let (Some(h), false) = (hider.as_mut(), menu_open) {
            h.update(ev, win, r.capture);
        }
        glue.after(ov_out.actions, r.capture, ev, win, &ui_hook, &mut handler, hider.as_mut());
        // Manettes au neutre sans focus ou menu ouvert.
        if let PresenterEvent::Window(grx_render::winit::event::WindowEvent::Focused(f)) = ev {
            ui_hook.focused.store(*f, Ordering::Relaxed);
        }
        let menu_now = ui_hook.ov.lock().unwrap().menu_open;
        ui_hook.pads.set_active(ui_hook.focused.load(Ordering::Relaxed) && !menu_now);
        ov_out.consumed // F11 reste traité par le présentateur (jamais consommé par l'overlay)
    });

    // Arrêt ordonné.
    stop.store(true, Ordering::SeqCst);
    handle.close();
    let _ = video.join();
    if let Some(c) = cursor {
        let _ = c.join();
    }
    if let Some(a) = audio {
        let _ = a.join();
    }
    if let Some(t) = selftest {
        let _ = t.join();
    }
    if let Some(t) = pads {
        let _ = t.join(); // neutre + débranchement envoyés avant la fin du fil d'envoi
    }
    if let Some(s) = sender {
        s.finish();
    }
    let st = conn.stats();
    println!(
        "entrées : {} envoyées, {} non acquittées à la fin, {} retransmissions",
        st.input_sent, st.transport.input_retransmits, st.input_unacked
    );
    match Arc::try_unwrap(conn) {
        Ok(c) => c.close(), // adieu au host : il libère sa session tout de suite
        Err(_) => eprintln!("! connexion encore référencée, fermeture au Drop"),
    }
    run_res?;

    let out = outcome.lock().unwrap().take().unwrap_or(Outcome::Closed);
    Ok(match out {
        Outcome::Closed => {
            println!("Fenêtre fermée, déconnecté proprement du host.");
            0
        }
        Outcome::Lost(r) => {
            eprintln!("Connexion perdue : {r}");
            3
        }
        Outcome::Error(e) => return Err(anyhow!(e)),
    })
}

// ---------------------------------------------------------------------------------------------
// Vidéo

fn video_loop(
    conn: Arc<ClientConnection>,
    handle: PresenterHandle,
    ui: Arc<Ui>,
    stop: Arc<AtomicBool>,
    outcome: Arc<Mutex<Option<Outcome>>>,
    print_stats: bool,
    secs: Option<f64>,
) {
    let finish = |o: Outcome| {
        let mut g = outcome.lock().unwrap();
        if g.is_none() {
            *g = Some(o);
        }
        drop(g);
        stop.store(true, Ordering::SeqCst);
        handle.close();
    };

    let start = Instant::now();
    let mut dec: Option<VideoDecoder> = None;
    let mut last_kf_req = Instant::now();
    conn.request_keyframe();

    // Fenêtre d'une seconde.
    let mut next_tick = start + Duration::from_secs(1);
    let (mut sec_rx, mut sec_dec_n) = (0u64, 0u64);
    let (mut sec_dec_ms, mut sec_rx_ms): (Vec<f64>, Vec<f64>) = (vec![], vec![]);
    let mut last = conn.stats();
    let mut last_pres = handle.stats();
    // Cumul pour le résumé final.
    let (mut all_dec_ms, mut all_rx_ms): (Vec<f64>, Vec<f64>) = (vec![], vec![]);
    let (mut all_pres_ms, mut all_e2e_ms, mut all_fps, mut all_rtt_ms): (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) =
        (vec![], vec![], vec![], vec![]);
    let mut all_loss = Vec::<f64>::new();
    let (mut decode_errors, mut presented_total, mut dropped_total, mut decoded_total) = (0u64, 0u64, 0u64, 0u64);
    let mut err_logged = 0;
    // Pour l'overlay : résolution et codec du flux.
    let (mut res, mut codec_name) = ((0u32, 0u32), String::new());
    // Liste d'écrans du host déjà vue ; bascule confirmée par le host, en attente de sa 1re image.
    let mut displays_seen = 0u64;
    let mut switch_confirmed: Option<Instant> = None;

    while !stop.load(Ordering::Relaxed) && !handle.is_closed() {
        if let ConnState::Closed(r) = conn.state() {
            finish(Outcome::Lost(r));
            break;
        }
        if secs.is_some_and(|s| start.elapsed().as_secs_f64() >= s) {
            finish(Outcome::Closed);
            break;
        }

        // Écrans du host : liste/écran actif pour l'overlay, et choix mémorisé redemandé.
        let ds = conn.displays_serial();
        if ds != displays_seen {
            displays_seen = ds;
            if let (_, Some(l)) = conn.displays() {
                let mut ov = ui.ov.lock().unwrap();
                let before = ov.active_display;
                let actions = ov.set_displays(l.clone());
                println!("écrans du host : {} | capturé : {}", l.displays.len(), ov.display_summary());
                for a in actions {
                    if let crate::overlay::Action::SelectDisplay(i) = a {
                        *ui.display_switch.lock().unwrap() = Some(Instant::now());
                        send_display_select(&conn, i);
                    }
                }
                if l.active.map(usize::from) != before || ui.display_switch.lock().unwrap().is_some() {
                    switch_confirmed = *ui.display_switch.lock().unwrap();
                }
                handle.set_overlay(ov.scene());
            }
        }

        if let Some(f) = conn.recv_video_timeout(Duration::from_millis(20)) {
            if f.keyframe {
                if let Some(t0) = switch_confirmed.take() {
                    *ui.display_switch.lock().unwrap() = None;
                    println!("écran : première image clé après la bascule reçue {:.1} ms après la demande", t0.elapsed().as_secs_f64() * 1000.0);
                }
            }
            let rx_ms = unix_micros().saturating_sub(f.pts_us) as f64 / 1000.0;
            sec_rx += 1;
            if dec.is_none() {
                match sniff_codec(&f.data) {
                    Some(codec) => match handle.create_decoder(codec) {
                        Ok(d) => {
                            println!("Décodeur : {} ({codec:?})", d.backend_name());
                            codec_name = format!("{codec:?}").to_uppercase();
                            dec = Some(d);
                        }
                        Err(e) => {
                            finish(Outcome::Error(format!("création du décodeur {codec:?} : {e}")));
                            break;
                        }
                    },
                    None => {
                        // Image delta avant tout IDR : on ne peut pas deviner le codec.
                        conn.request_keyframe();
                        continue;
                    }
                }
            }
            let d = dec.as_mut().unwrap();
            let t0 = Instant::now();
            match d.decode_pts(&f.data, f.pts_us) {
                Ok(Some(frame)) => {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    sec_dec_n += 1;
                    decoded_total += 1;
                    sec_dec_ms.push(ms);
                    sec_rx_ms.push(rx_ms);
                    all_dec_ms.push(ms);
                    all_rx_ms.push(rx_ms);
                    res = (frame.width, frame.height);
                    handle.submit(frame);
                }
                Ok(None) => {}
                Err(e) => {
                    decode_errors += 1;
                    if err_logged < 5 {
                        err_logged += 1;
                        eprintln!("! erreur de décodage : {e} (reset + demande d'image clé)");
                    }
                    let _ = d.reset();
                }
            }
        }
        if dec.as_ref().is_none_or(|d| d.needs_keyframe()) && last_kf_req.elapsed() >= Duration::from_millis(500) {
            conn.request_keyframe();
            last_kf_req = Instant::now();
        }

        if Instant::now() >= next_tick {
            next_tick += Duration::from_secs(1);
            let s = conn.stats();
            let pres = handle.stats();
            let lat = handle.take_latency_stats();
            let n = sec_dec_n as f64;
            let video_mbps = (s.video_bytes - last.video_bytes) as f64 * 8.0 / 1e6;
            let wire_mbps = (s.transport.bytes_rx - last.transport.bytes_rx) as f64 * 8.0 / 1e6;
            let rtt_ms = s.transport.rtt_us.unwrap_or(0) as f64 / 1000.0;
            let loss = s.transport.rx_loss as f64 * 100.0;
            let dec_avg = avg(&sec_dec_ms);
            let dec_p99 = pct(&mut sec_dec_ms, 0.99);
            let rx_avg = avg(&sec_rx_ms);
            let pres_avg = lat.avg_us as f64 / 1000.0;
            let pres_p99 = lat.p99_us as f64 / 1000.0;
            let e2e = rx_avg + dec_avg + pres_avg;
            let presented = pres.presented - last_pres.presented;
            let dropped = pres.dropped - last_pres.dropped;
            let secs_n = ((Instant::now() - start).as_secs()).max(1);

            *ui.line.lock().unwrap() =
                format!("{n:.0} img/s | {video_mbps:.1} Mb/s | rtt {rtt_ms:.1} ms | perte {loss:.1}% | dec {dec_avg:.1} ms | ->ecran {e2e:.1} ms");
            if ui.show_stats.load(Ordering::Relaxed) {
                handle.set_title(ui.title());
            }
            {
                let mut ov = ui.ov.lock().unwrap();
                ov.stats = Some(crate::overlay::StatsSnapshot {
                    fps: n,
                    video_mbps,
                    rtt_ms,
                    loss_pct: loss,
                    rx_ms: rx_avg,
                    dec_ms: dec_avg,
                    pres_ms: pres_avg,
                    e2e_ms: e2e,
                    width: res.0,
                    height: res.1,
                    codec: codec_name.clone(),
                    frames_lost: s.transport.video_rx.frames_lost,
                    dropped: pres.dropped,
                });
                ov.host = conn.host_info();
                // Ligne d'état seulement si les manettes sont lues (pas avec --no-gamepad / --no-input).
                if ui.pads_on.load(Ordering::Relaxed) {
                    ov.gamepads = crate::gamepads::status_line(&ui.pads.list());
                }
                if ov.is_visible() {
                    handle.set_overlay(ov.scene());
                }
            }
            if print_stats {
                println!(
                    "t={secs_n:>3}s  {n:>3.0} img/s (reçues {sec_rx})  {video_mbps:>6.1} Mb/s vidéo ({wire_mbps:>6.1} fil)  rtt {rtt_ms:>5.2} ms  perte {loss:>4.1}%  \
                     décodage {dec_avg:>4.2} ms (p99 {dec_p99:>4.2})  capture->réception {rx_avg:>5.1} ms  affichage {pres_avg:>4.2} ms (p99 {pres_p99:>4.2})  \
                     capture->écran ~{e2e:>5.1} ms  présentées {presented} écartées {dropped}  perdues {}",
                    s.transport.video_rx.frames_lost
                );
            }
            if secs_n == 1 {
                // Démarrage : l'encodeur du host tourne déjà pendant que la fenêtre/le décodeur s'initialisent
                // (rafale d'images en retard) ; on l'écarte des percentiles du résumé.
                all_dec_ms.clear();
                all_rx_ms.clear();
            }
            if secs_n > 1 {
                // La 1re seconde (démarrage encodeur/décodeur) est exclue du résumé.
                all_fps.push(n);
                all_pres_ms.push(pres_avg);
                all_e2e_ms.push(e2e);
                all_rtt_ms.push(rtt_ms);
                all_loss.push(loss);
            }
            presented_total += presented;
            dropped_total += dropped;
            last = s;
            last_pres = pres;
            sec_rx = 0;
            sec_dec_n = 0;
            sec_dec_ms.clear();
            sec_rx_ms.clear();
        }
    }
    let s = conn.stats();
    let v = s.transport.video_rx;
    let run_s = start.elapsed().as_secs_f64();
    println!("\n-- Résumé play ({run_s:.1} s) --");
    println!(
        "images : {} reçues ({} clés), {} décodées, {} présentées, {} écartées à l'affichage, {} erreurs de décodage",
        s.video_frames,
        s.video_keyframes,
        decoded_total,
        presented_total,
        dropped_total,
        decode_errors
    );
    let fmin = all_fps.iter().cloned().fold(f64::MAX, f64::min);
    println!(
        "FPS    : moyen {:.1} (min {:.0} / max {:.0} par seconde, 1re exclue)",
        avg(&all_fps),
        if all_fps.is_empty() { 0.0 } else { fmin },
        all_fps.iter().cloned().fold(0.0, f64::max)
    );
    println!(
        "décodage (appel MFT, 1re s exclue) : moy {:.2} ms, p50 {:.2}, p99 {:.2}, max {:.2}",
        avg(&all_dec_ms),
        pct(&mut all_dec_ms, 0.5),
        pct(&mut all_dec_ms, 0.99),
        pct(&mut all_dec_ms, 1.0)
    );
    println!(
        "capture->réception : moy {:.2} ms, p50 {:.2}, p99 {:.2} (horloges host/client supposées synchrones)",
        avg(&all_rx_ms),
        pct(&mut all_rx_ms, 0.5),
        pct(&mut all_rx_ms, 0.99)
    );
    println!(
        "affichage (soumission->Present) : moy {:.2} ms ; capture->écran estimé : moy {:.1} ms",
        avg(&all_pres_ms),
        avg(&all_e2e_ms)
    );
    println!(
        "réseau : RTT moy {:.2} ms (min {:.2}) | perte moy {:.2}% | images perdues {} | IDR demandées {} | purges file {}",
        avg(&all_rtt_ms),
        s.transport.min_rtt_us.unwrap_or(0) as f64 / 1000.0,
        avg(&all_loss),
        v.frames_lost,
        v.keyframe_requests,
        s.video_overflow_dropped
    );
}

// ---------------------------------------------------------------------------------------------
// Overlay (menu à la Parsec)

/// Lien entre la logique du menu (`overlay.rs`) et la fenêtre, les entrées, le réseau.
struct OverlayGlue {
    input: crate::overlay::OverlayInput,
    kbd: Option<crate::kbdlock::KeyboardLock>,
    kbd_tx: Option<std::sync::mpsc::Sender<Msg>>,
    conn: Arc<ClientConnection>,
    handle: PresenterHandle,
    open_at_start: bool,
    /// `overlay.json` du dossier de données du client.
    prefs_path: Option<std::path::PathBuf>,
    /// Dernières préférences écrites (on n'écrit que ce qui a changé).
    saved_prefs: crate::overlay_prefs::OverlayPrefs,
    /// Curseur « déplacer » actuellement affiché.
    move_cursor: bool,
}

impl OverlayGlue {
    /// Renvoie l'état de capture souris appliqué au démarrage (préférence mémorisée), s'il y en a un.
    fn ready(&mut self, win: &grx_render::winit::window::Window, ui: &Ui, handler: &mut InputHandler) -> Option<bool> {
        self.kbd = Some(crate::kbdlock::KeyboardLock::new(self.kbd_tx.take(), crate::kbdlock::hwnd_of(win)));
        let mut ov = ui.ov.lock().unwrap();
        let sz = win.inner_size();
        ov.set_view(sz.width, sz.height, win.scale_factor());
        ov.monitor_hz = win
            .current_monitor()
            .and_then(|m| m.refresh_rate_millihertz())
            .map(|mhz| (mhz + 500) / 1000)
            .unwrap_or(60);
        // Réglages de flux mémorisés : renvoyés au host dès la connexion.
        let st = ov.settings();
        if st != Default::default() {
            println!(
                "overlay : réglages mémorisés -> débit max {} | FPS max {}",
                st.max_bitrate_kbps.map_or("auto".into(), |k| format!("{} Mb/s", k / 1000)),
                st.max_fps.map_or("host".into(), |f| f.to_string())
            );
            self.conn.send_control(&st.encode());
        }
        // Capture souris mémorisée (avant une éventuelle ouverture du menu, qui la suspend).
        let captured = (ov.capture && ov.input_enabled).then(|| {
            let c = handler.set_capture_mode(true, win);
            ov.capture = c;
            ui.captured.store(c, Ordering::Relaxed);
            c
        });
        if self.open_at_start {
            let a = ov.open_menu();
            drop(ov);
            self.after(a, None, &PresenterEvent::Ready, win, ui, handler, None);
            return captured;
        }
        self.handle.set_overlay(ov.scene());
        captured
    }

    fn event(&mut self, ev: &PresenterEvent, ui: &Ui) -> crate::overlay::Outcome {
        let mut ov = ui.ov.lock().unwrap();
        match ev {
            PresenterEvent::Window(we) => {
                if let grx_render::winit::event::WindowEvent::Focused(false) = we {
                    if let Some(k) = self.kbd.as_mut() {
                        k.release_all();
                    }
                }
                self.input.window_event(&mut ov, we)
            }
            // Souris brute (mode capture) : jamais vers le host quand le menu est ouvert.
            _ => crate::overlay::Outcome { consumed: ov.menu_open, actions: vec![] },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn after(
        &mut self,
        actions: Vec<crate::overlay::Action>,
        capture: Option<bool>,
        ev: &PresenterEvent,
        win: &grx_render::winit::window::Window,
        ui: &Ui,
        handler: &mut InputHandler,
        hider: Option<&mut crate::cursor::LocalCursorHider>,
    ) {
        use crate::overlay::Action;
        let mut ov = ui.ov.lock().unwrap();
        if let Some(c) = capture {
            ov.capture = c;
        }
        let mut hider = hider;
        for a in actions {
            match a {
                Action::SetFullscreen(on) => self.handle.set_fullscreen(on),
                Action::SetVolume(v) => ui.volume.store(v.to_bits(), Ordering::Relaxed),
                Action::SendSettings(st) => {
                    println!(
                        "overlay : débit max {} | FPS max {}",
                        st.max_bitrate_kbps.map_or("auto".into(), |k| format!("{} Mb/s", k / 1000)),
                        st.max_fps.map_or("host".into(), |f| f.to_string())
                    );
                    self.conn.send_control(&st.encode());
                }
                Action::SetKbdLock(on) => {
                    let k = self.kbd.as_mut();
                    ov.kbd_lock = k.is_some_and(|k| {
                        let r = k.set_enabled(on);
                        k.set_suspended(ov.menu_open);
                        r
                    });
                }
                Action::SetVsync(on) => self.handle.set_vsync(on),
                Action::SelectDisplay(i) => {
                    *ui.display_switch.lock().unwrap() = Some(Instant::now());
                    send_display_select(&self.conn, i);
                }
                Action::Disconnect => {
                    println!("overlay : déconnexion demandée");
                    self.handle.close();
                }
                Action::MenuOpened => {
                    // Le host ne doit rien garder d'enfoncé (Ctrl/Maj du raccourci compris).
                    handler.release_all();
                    ov.capture = handler.is_captured();
                    handler.set_capture_mode(false, win);
                    win.set_cursor_visible(true);
                    if let Some(k) = self.kbd.as_mut() {
                        k.set_suspended(true);
                    }
                }
                Action::MenuClosed => {
                    if let Some(k) = self.kbd.as_mut() {
                        k.set_suspended(false);
                    }
                    let want = ov.capture && ov.input_enabled;
                    ov.capture = handler.set_capture_mode(want, win);
                    ui.captured.store(ov.capture, Ordering::Relaxed);
                    win.set_title(&ui.title());
                    if let Some(h) = hider.as_deref_mut() {
                        h.update(ev, win, Some(ov.capture));
                    }
                }
            }
        }
        ov.fullscreen = win.fullscreen().is_some();
        self.handle.set_overlay(ov.scene());
        // Curseur adapté au survol d'une poignée de déplacement.
        let mv = ov.wants_move_cursor();
        if mv != self.move_cursor {
            use grx_render::winit::window::CursorIcon;
            win.set_cursor(if mv { CursorIcon::Move } else { CursorIcon::Default });
            self.move_cursor = mv;
        }
        // Préférences : écrites quand elles changent, jamais pendant un glisser (écriture au relâchement).
        if let Some(path) = self.prefs_path.as_deref() {
            if !ov.is_dragging() {
                let cur = ov.prefs();
                if cur != self.saved_prefs {
                    if let Err(e) = cur.save(path) {
                        eprintln!("! préférences de l'overlay non enregistrées : {e}");
                    }
                    self.saved_prefs = cur;
                }
            }
        }
    }
}

/// Demande au host de capturer l'écran `i` (`GRX-MON`).
fn send_display_select(conn: &ClientConnection, i: u8) {
    println!("overlay : écran {} demandé", i as u32 + 1);
    conn.send_control(&grx_core::displays::DisplaySelect { index: i }.encode());
}

// ---------------------------------------------------------------------------------------------
// Audio

/// Dérouleur de numéros de séquence u16 (rebouclant) -> u32 croissant pour le jitter buffer.
struct SeqUnwrap {
    last: Option<(u16, u32)>,
}

impl SeqUnwrap {
    fn next(&mut self, seq: u16) -> u32 {
        let ext = match self.last {
            None => 1 << 16, // marge pour les paquets en retard au tout début
            Some((l, e)) => e.wrapping_add((seq.wrapping_sub(l) as i16) as i32 as u32),
        };
        // On avance seulement si le paquet est plus récent (réordonnancement toléré).
        match self.last {
            Some((_, e)) if (ext.wrapping_sub(e) as i32) <= 0 => {}
            _ => self.last = Some((seq, ext)),
        }
        ext
    }
}

fn audio_loop(conn: Arc<ClientConnection>, player: AudioPlayer, stop: Arc<AtomicBool>, ui: Arc<Ui>) {
    let mut seq = SeqUnwrap { last: None };
    let mut vol = ui.volume.load(Ordering::Relaxed);
    while !stop.load(Ordering::Relaxed) {
        let v = ui.volume.load(Ordering::Relaxed);
        if v != vol {
            vol = v;
            player.set_volume(f32::from_bits(v));
        }
        match conn.recv_audio_timeout(Duration::from_millis(10)) {
            Some(pkt) => {
                player.push_packet(seq.next(pkt.seq), &pkt.data);
                while let Some(p) = conn.poll_audio() {
                    player.push_packet(seq.next(p.seq), &p.data);
                }
            }
            None => {
                if !conn.is_connected() {
                    break;
                }
            }
        }
    }
    let st = player.stats();
    println!(
        "audio : {} trames décodées, {} PLC, {} FEC, {} trames de silence, périphérique « {} »",
        st.decoded_frames, st.plc_frames, st.fec_frames, st.silence_frames, st.device_name
    );
    player.stop();
}

// ---------------------------------------------------------------------------------------------
// Test automatique du chemin d'envoi des entrées

/// Événements inoffensifs (position au centre, quelques mouvements relatifs, F24, molette nulle)
/// passant par le même `InputSender` que les vraies entrées. Pas de clic.
fn spawn_selftest(tx: std::sync::mpsc::Sender<Msg>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let sleep = |ms| {
            // Interruptible : on ne retarde pas la fermeture.
            let t = Instant::now();
            while t.elapsed() < Duration::from_millis(ms) && !stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
            }
        };
        sleep(2000);
        let mut n = 0;
        let mut ev = |m: Msg| {
            n += 1;
            let _ = tx.send(m);
        };
        ev(Msg::Abs(32768, 32768));
        sleep(30);
        for i in 0..40 {
            ev(Msg::Rel(if i < 20 { 1 } else { -1 }, 0)); // somme nulle au total
            sleep(4);
        }
        ev(Msg::Event(InputEvent::Key { scancode: 0x76, extended: false, pressed: true }));
        ev(Msg::Event(InputEvent::Key { scancode: 0x76, extended: false, pressed: false }));
        ev(Msg::Event(InputEvent::MouseButton { button: MouseButton::X2, pressed: false })); // relâchement sans effet
        ev(Msg::Event(InputEvent::Wheel { horizontal: 0, vertical: 0 }));
        println!("selftest-input : {n} messages soumis au chemin d'envoi (Abs, 40 Rel (+20 puis -20) fusionnés, F24 haut/bas, relâche X2, molette nulle)");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_unwrap_monotonic_across_wrap_and_reorder() {
        let mut u = SeqUnwrap { last: None };
        let a = u.next(65534);
        assert_eq!(u.next(65535), a + 1);
        assert_eq!(u.next(0), a + 2);
        assert_eq!(u.next(1), a + 3);
        // paquet en retard : numéro plus petit, sans faire reculer la référence
        assert_eq!(u.next(65535), a + 1);
        assert_eq!(u.next(2), a + 4);
    }
}

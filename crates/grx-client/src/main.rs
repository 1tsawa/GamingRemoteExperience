mod bench;
mod codec;
mod cursor;
mod gamepads;
mod input;
mod kbdlock;
mod net;
mod overlay;
mod overlay_layout;
mod overlay_prefs;
mod play;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grx_core::util::default_data_dir;
use grx_core::{fingerprint, Identity, Invite, ProfileStore};

use crate::net::{ClientConnection, ConnectOptions};

#[derive(Parser)]
#[command(name = "grx-client", version, about = "Client GamingRemoteExperience")]
struct Cli {
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

/// `play --codec` : codec demandé au host.
#[derive(Clone, Copy, ValueEnum)]
pub enum PlayCodecArg {
    /// Le host choisit parmi les décodeurs de ce PC (HEVC, sinon H.264)
    Auto,
    Hevc,
    H264,
    Av1,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CodecArg {
    Hevc,
    H264,
    Av1,
}

#[derive(Subcommand)]
enum Cmd {
    /// Affiche l'empreinte de ce client
    Info,
    /// Liste les hosts mémorisés
    Hosts,
    /// Se connecte avec un lien grx:// (nouvelle invitation) ou le nom d'un host mémorisé,
    /// vérifie la session quelques secondes puis se déconnecte
    Connect {
        target: String,
        /// Nom à donner à ce host (première connexion)
        #[arg(long)]
        save_as: Option<String>,
        /// Durée de maintien de la session (s)
        #[arg(long, default_value_t = 3.0)]
        secs: f64,
    },
    /// Ouvre une fenêtre, affiche le flux du host (décodage matériel), joue l'audio et envoie clavier/souris.
    /// F11 plein écran, F10 stats dans le titre. Le curseur du host (forme et position, envoyés à
    /// part) est dessiné par-dessus la vidéo ; `--local-cursor` garde le curseur Windows local.
    /// Code de sortie 3 si la connexion est perdue.
    Play {
        /// Lien grx:// ou nom d'un host mémorisé
        target: String,
        /// Nom à donner à ce host (première connexion)
        #[arg(long)]
        save_as: Option<String>,
        /// N'envoie AUCUNE entrée au host (clavier/souris)
        #[arg(long)]
        no_input: bool,
        /// Pas de lecture audio
        #[arg(long)]
        no_audio: bool,
        /// Volume de lecture (0..1, 1 = inchangé)
        #[arg(long, default_value_t = play::DEFAULT_VOLUME)]
        volume: f32,
        /// Une ligne de stats par seconde sur stdout (FPS, débit, RTT, perte, latences) + stats dans le titre
        #[arg(long)]
        stats: bool,
        /// Démarre en plein écran (F11 bascule)
        #[arg(long)]
        fullscreen: bool,
        /// Synchronisation verticale (par défaut : sans VSync, latence minimale)
        #[arg(long)]
        vsync: bool,
        /// Taille initiale de la fenêtre
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        /// Quitte tout seul après N secondes (tests)
        #[arg(long)]
        secs: Option<f64>,
        /// Raccourci d'entrée/sortie du mode capture souris relative (jamais transmis au host)
        #[arg(long, default_value = "ctrl+alt+shift+g")]
        capture_key: input::Hotkey,
        /// Envoie quelques événements d'entrée synthétiques inoffensifs (position, mouvements, F24, molette nulle)
        #[arg(long)]
        selftest_input: bool,
        /// Fréquence maximale d'envoi des mouvements de souris (Hz)
        #[arg(long, default_value_t = 125)]
        input_hz: u32,
        /// Curseur Windows LOCAL au lieu du curseur du host dessiné sur la vidéo (ancien comportement)
        #[arg(long)]
        local_cursor: bool,
        /// (tests) ouvre le menu de l'overlay au démarrage
        #[arg(long, hide = true)]
        open_overlay: bool,
        /// (tests) affiche les statistiques de l'overlay au démarrage
        #[arg(long, hide = true)]
        overlay_stats: bool,
        /// Ne lit aucune manette (clavier/souris seulement)
        #[arg(long)]
        no_gamepad: bool,
        /// Manette virtuelle demandée au host : xbox (Xbox 360 pour toutes, défaut), auto (DualShock 4
        /// pour les manettes Sony) ou ds4 (DualShock 4 pour toutes). Le host peut imposer son choix.
        #[arg(long, default_value = "xbox")]
        gamepad_as: gamepads::VirtualPref,
        /// (tests) manette simulée scriptée : xbox, ds4 ou dualsense (remplace les manettes physiques)
        #[arg(long, hide = true)]
        sim_gamepad: Option<gamepads::SimPad>,
        /// (tests) journalise chaque événement manette envoyé
        #[arg(long, hide = true)]
        gamepad_verbose: bool,
        /// Codec demandé au host (auto : le host choisit selon les décodeurs de ce PC ; un codec
        /// que ce PC ne sait pas décoder n'est jamais imposé)
        #[arg(long, value_enum, default_value = "auto")]
        codec: PlayCodecArg,
        /// (tests) voies de décodage à ignorer, ex. mf-hevc, ffmpeg, av1, ffmpeg-h264
        #[arg(long, hide = true, default_value = "")]
        disable_decoder: String,
        /// (tests) panne simulée du décodeur : codec:images, ex. hevc:120
        #[arg(long, hide = true)]
        fail_decoder_after: Option<String>,
    },
    /// Liste les manettes détectées (XInput, DualShock 4 / DualSense, génériques) et affiche leur
    /// état pendant quelques secondes, sans connexion à un host
    Gamepads {
        /// Durée de l'observation (s)
        #[arg(long, default_value_t = 5.0)]
        secs: f64,
        /// (tests) ajoute une manette simulée : xbox, ds4 ou dualsense
        #[arg(long, hide = true)]
        sim_gamepad: Option<gamepads::SimPad>,
        /// Fait vibrer chaque manette détectée pendant 0,5 s
        #[arg(long)]
        rumble_test: bool,
    },
    /// Banc d'essai sans fenêtre : reçoit vidéo/audio, mesure FPS/débit/RTT/perte, valide le H.265
    Bench {
        /// Lien grx:// ou nom d'un host mémorisé
        target: String,
        /// Durée de la mesure (s)
        #[arg(long, default_value_t = 10.0)]
        secs: f64,
        /// Écrit le flux vidéo reçu (Annex-B brut) dans ce fichier
        #[arg(long)]
        save: Option<PathBuf>,
        /// Nom à donner à ce host (première connexion)
        #[arg(long)]
        save_as: Option<String>,
        /// Codec attendu pour la validation des images
        #[arg(long, value_enum, default_value = "hevc")]
        codec: CodecArg,
        /// Perte simulée (0..1) sur les datagrammes reçus et émis
        #[arg(long, default_value_t = 0.0)]
        sim_loss: f64,
        /// Pas d'événements d'entrée (par défaut : souris 0,0 à 100 Hz + F24 chaque seconde)
        #[arg(long)]
        no_input: bool,
        /// Pas de demande d'image clé au démarrage
        #[arg(long)]
        no_keyframe_request: bool,
        /// Ne pas imprimer la ligne de statistiques par seconde
        #[arg(long)]
        quiet: bool,
    },
}

/// Hosts mémorisés : `hosts.json` (logique partagée avec `grx-gui`, voir `grx_core::profiles`).
struct Store(ProfileStore);

impl Store {
    fn load(dir: &std::path::Path) -> Result<Self> {
        Ok(Store(ProfileStore::load(dir).map_err(|e| anyhow!("hosts.json illisible ({e}) : corrigez ou supprimez le fichier"))?))
    }

    /// Lien d'invitation ou nom mémorisé -> `Invite` ; `bool` = host déjà connu.
    fn resolve(&self, target: &str) -> Result<(Invite, bool)> {
        Ok(self.0.resolve(target)?)
    }

    fn remember(&mut self, invite: &Invite, save_as: Option<String>) -> Result<()> {
        let name = self.0.remember(invite, save_as)?;
        println!("Host mémorisé sous le nom « {name} » (reconnexion sans jeton).");
        Ok(())
    }
}

/// `-` comme cible : le lien (ou le nom) est lu sur l'entrée standard, pour ne pas exposer le
/// jeton d'invitation dans la ligne de commande du processus (utilisé par `grx-gui`).
fn read_target(target: String) -> Result<String> {
    if target != "-" {
        return Ok(target);
    }
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim_start_matches('\u{feff}').trim().to_string();
    if line.is_empty() {
        return Err(anyhow!("aucune cible reçue sur l'entrée standard"));
    }
    Ok(line)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let dir = cli.data_dir.unwrap_or_else(|| default_data_dir("client"));
    let identity = Identity::load_or_create(&dir.join("identity.json"))?;
    let mut store = Store::load(&dir)?;

    match cli.cmd {
        Cmd::Info => println!("Empreinte de ce client : {}", identity.fingerprint()),
        Cmd::Hosts => {
            for h in store.0.hosts() {
                let fp = h.fingerprint().unwrap_or_default();
                println!("{:<16} {:<28} {fp}", h.name, h.addr);
            }
        }
        Cmd::Connect { target, save_as, secs } => {
            let (invite, known) = store.resolve(&read_target(target)?)?;
            println!("Connexion à {} — empreinte attendue : {}", invite.addr, fingerprint(&invite.host_key));
            let conn = ClientConnection::connect(&identity, &invite, ConnectOptions::default())?;
            println!(
                "Authentifié auprès de « {} » ({}), session {:08x}",
                conn.host_name(),
                conn.host_fingerprint(),
                conn.session_id()
            );
            if !known {
                store.remember(&invite, save_as)?;
            }
            conn.request_keyframe();
            let t0 = std::time::Instant::now();
            let mut frames = 0u64;
            while t0.elapsed() < Duration::from_secs_f64(secs) && conn.is_connected() {
                if conn.recv_video_timeout(Duration::from_millis(20)).is_some() {
                    frames += 1;
                }
            }
            let s = conn.stats();
            println!(
                "{} images reçues en {:.1} s, RTT {} us, état {:?}",
                frames,
                t0.elapsed().as_secs_f64(),
                s.transport.rtt_us.unwrap_or(0),
                conn.state()
            );
            conn.close();
        }
        Cmd::Play {
            target,
            save_as,
            no_input,
            no_audio,
            volume,
            stats,
            fullscreen,
            vsync,
            width,
            height,
            secs,
            capture_key,
            selftest_input,
            input_hz,
            local_cursor,
            open_overlay,
            overlay_stats,
            no_gamepad,
            gamepad_as,
            sim_gamepad,
            gamepad_verbose,
            codec,
            disable_decoder,
            fail_decoder_after,
        } => {
            let (invite, known) = store.resolve(&read_target(target)?)?;
            println!("Connexion à {} — empreinte attendue : {}", invite.addr, fingerprint(&invite.host_key));
            let code = play::run(
                &identity,
                &invite,
                ConnectOptions::default(),
                play::PlayOpts {
                    no_input,
                    no_audio,
                    stats,
                    volume: volume.clamp(0.0, 1.0),
                    fullscreen,
                    vsync,
                    width,
                    height,
                    secs,
                    capture_key,
                    selftest_input,
                    input_hz,
                    local_cursor,
                    open_overlay,
                    overlay_stats,
                    prefs_path: Some(dir.join("overlay.json")),
                    gamepad: (!no_gamepad).then(|| gamepads::PadOpts {
                        pref: gamepad_as,
                        sim: sim_gamepad,
                        verbose: gamepad_verbose,
                        ..Default::default()
                    }),
                    codec: {
                        use grx_core::codec_neg::VideoCodec;
                        match codec {
                            PlayCodecArg::Auto => None,
                            PlayCodecArg::Hevc => Some(VideoCodec::Hevc),
                            PlayCodecArg::H264 => Some(VideoCodec::H264),
                            PlayCodecArg::Av1 => Some(VideoCodec::Av1),
                        }
                    },
                    disabled_decoders: grx_render::DisabledDecoders::parse(&disable_decoder)
                        .map_err(|e| anyhow!("--disable-decoder : {e}"))?,
                    fail_decoder_after: match fail_decoder_after.as_deref().map(|v| v.split_once(':')) {
                        None => None,
                        Some(Some((c, n))) => Some((
                            match c {
                                "h264" => grx_video::Codec::H264,
                                "hevc" => grx_video::Codec::Hevc,
                                "av1" => grx_video::Codec::Av1,
                                _ => bail!("--fail-decoder-after : codec h264|hevc|av1"),
                            },
                            n.parse().map_err(|_| anyhow!("--fail-decoder-after : nombre d'images"))?,
                        )),
                        Some(None) => bail!("--fail-decoder-after : codec:images"),
                    },
                },
                |_| if !known { store.remember(&invite, save_as.clone()) } else { Ok(()) },
            )?;
            if code != 0 {
                std::process::exit(code);
            }
        }
        Cmd::Gamepads { secs, sim_gamepad, rumble_test } => gamepads::probe(secs, sim_gamepad, rumble_test),
        Cmd::Bench { target, secs, save, save_as, codec, sim_loss, no_input, no_keyframe_request, quiet } => {
            let (invite, known) = store.resolve(&read_target(target)?)?;
            println!("Connexion à {} — empreinte attendue : {}", invite.addr, fingerprint(&invite.host_key));
            let opts = ConnectOptions { sim_loss, ..Default::default() };
            let ok = bench::run(
                &identity,
                &invite,
                opts,
                bench::BenchOpts { secs, save, codec, input: !no_input, keyframe_request: !no_keyframe_request, quiet },
                |_| {
                    if !known {
                        store.remember(&invite, save_as.clone())
                    } else {
                        Ok(())
                    }
                },
            )?;
            if !ok {
                std::process::exit(2);
            }
        }
    }
    Ok(())
}

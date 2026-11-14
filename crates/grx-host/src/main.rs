mod access;
mod codec;
mod control;
mod cursor;
mod display;
mod elevation;
mod encprobe;
mod firewall;
mod pipeline;
mod portmap_task;
mod secure;
mod server;
mod status;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand, ValueEnum};
use grx_core::util::{default_data_dir, now_ms};
use grx_core::trust::MAX_INVITE_TTL_MS;
use grx_core::{fingerprint, HostAuth, Identity, Invite, TrustStore};
use grx_input::{GamepadPolicy, InjectorConfig};

use crate::pipeline::PipelineOpts;
use crate::server::RunOpts;

#[derive(Clone, Copy, ValueEnum)]
enum EncoderArg {
    Auto,
    Nvidia,
    Amd,
    Intel,
    /// OpenH264 de Cisco (à installer : `grx-host fetch-openh264`)
    Software,
}

#[derive(Clone, Copy, ValueEnum)]
enum ChromaArg {
    Auto,
    #[value(name = "420")]
    _420,
    #[value(name = "444")]
    _444,
}

#[derive(Clone, Copy, ValueEnum)]
enum CodecArg {
    /// Négocié avec le client : HEVC si son PC le décode, sinon H.264 (AV1 seulement sur demande)
    Auto,
    Hevc,
    H264,
    /// AV1 (encodeur NVIDIA RTX 40+ requis ; repli automatique sinon)
    Av1,
}

/// Rôle d'un client autorisé.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum GamepadTypeArg {
    Auto,
    Xbox,
    Ds4,
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    /// Voit l'écran et entend le son, n'injecte aucune entrée
    ViewOnly,
    /// Contrôle complet (clavier, souris, manette) sur le bureau normal
    Full,
    /// Comme « full », et pilote aussi les fenêtres administrateur si le host est élevé
    Total,
}

impl RoleArg {
    fn role(self) -> grx_core::Role {
        match self {
            RoleArg::ViewOnly => grx_core::Role::ViewOnly,
            RoleArg::Full => grx_core::Role::Full,
            RoleArg::Total => grx_core::Role::Total,
        }
    }
}

/// Situation simulée pour `run --simulate-secure` (mode de test caché).
#[derive(Clone, Copy, ValueEnum)]
enum SimulateArg {
    Normal,
    Elevated,
    Secure,
    Cycle,
}

#[derive(Parser)]
#[command(name = "grx-host", version, about = "Hôte GamingRemoteExperience")]
struct Cli {
    /// Dossier de données (identité, clients autorisés)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Affiche l'empreinte de ce host
    Info,
    /// Génère un lien d'invitation à usage unique
    Invite {
        /// Adresse publique que le client utilisera (ex: 203.0.113.5:7777, monpc.duckdns.org:7777,
        /// [2a01:…]:7777) ou `auto` : redirection active du host lancé, sinon IP externe de la box
        #[arg(long)]
        addr: String,
        /// Port utilisé avec `--addr auto` quand aucun host n'est lancé
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Durée de validité en minutes (plafonnée à 1440 = 24 h)
        #[arg(long, default_value_t = 10)]
        ttl_min: u64,
    },
    /// Liste les clients autorisés (et leur rôle)
    List,
    /// Révoque un client par préfixe d'empreinte (4 caractères minimum)
    Revoke { fingerprint: String },
    /// Change le rôle d'un client : `view-only`, `full` ou `total`. Une session en cours est coupée.
    SetRole { fingerprint: String, #[arg(value_enum)] role: RoleArg },
    /// Télécharge depuis Cisco le codec OpenH264 (encodeur H.264 logiciel de secours, « OpenH264
    /// Video Codec provided by Cisco Systems, Inc. »), vérifié par SHA-256, à côté de l'exécutable
    FetchOpenh264 {
        /// Dossier d'installation (défaut : celui de grx-host.exe)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Supprime le codec (désactivation)
        #[arg(long)]
        remove: bool,
    },
    /// Mode « Contrôle total » : host lancé élevé par une tâche planifiée (voir la sous-commande)
    #[command(subcommand)]
    Elevation(ElevationCmd),
    /// Affiche les commandes (administrateur) pour autoriser grx-host dans le pare-feu Windows
    FirewallHelp {
        #[arg(long, default_value_t = 7777)]
        port: u16,
    },
    /// Sonde les encodeurs vidéo par test réel (création + 2 images 256x256 + validation du flux)
    /// et affiche le tableau (vendeur, codec, chroma, profondeur, résultat, temps). Résultats en
    /// cache (encoder-probe.json), invalidé si le GPU, son pilote ou la version de GRX change.
    Encoders {
        /// Ignore le cache et refait la sonde
        #[arg(long)]
        refresh: bool,
        /// Sortie JSON sur une ligne (utilisée par grx-gui)
        #[arg(long)]
        json: bool,
    },
    /// Écoute les connexions
    Run {
        /// Adresse d'écoute ; `[::]:7777` = IPv4 + IPv6 (double pile)
        #[arg(long, default_value = "0.0.0.0:7777")]
        bind: SocketAddr,
        /// N'ouvre PAS automatiquement le port sur la box (UPnP / PCP / NAT-PMP)
        #[arg(long)]
        no_upnp: bool,
        /// Port externe souhaité sur la box (défaut : le port d'écoute)
        #[arg(long)]
        external_port: Option<u16>,
        /// Durée du bail de redirection demandé à la box (s), renouvelé à mi-vie ; 0 = permanent
        #[arg(long, default_value_t = 3600)]
        upnp_lease: u64,
        /// Codec vidéo préféré ; le client annonce ses décodeurs et un codec qu'il ne sait pas
        /// décoder est remplacé automatiquement (H.264 en dernier recours)
        #[arg(long, value_enum, default_value = "auto")]
        codec: CodecArg,
        /// Encodeur : auto (celui du GPU de l'écran capturé, puis logiciel), nvidia, amd, intel,
        /// software (OpenH264, H.264 seulement)
        #[arg(long, value_enum, default_value = "auto")]
        encoder: EncoderArg,
        /// Sous-échantillonnage : auto (4:4:4 si le client l'active dans son menu), 420, 444
        /// (« Couleur 4:4:4 » de Parsec : NVIDIA HEVC uniquement, client capable de le décoder)
        #[arg(long, value_enum, default_value = "auto")]
        chroma: ChromaArg,
        /// Caché (tests) : mire HDR synthétique au lieu de la capture (voie 10 bits/HDR10)
        #[arg(long, hide = true)]
        test_pattern_hdr: bool,
        /// Cadence maximale de capture/encodage
        #[arg(long, default_value_t = 60)]
        fps: u32,
        /// Bitrate vidéo maximal (Mb/s, FEC comprise) que le contrôle de débit peut atteindre
        /// (30 par défaut depuis 0.2.3 : largement suffisant en 1440p60, ménage le décodeur des
        /// portables ; montez-le pour du 4K ou du 120 FPS)
        #[arg(long, default_value_t = 30.0)]
        max_bitrate: f64,
        /// Bitrate vidéo de départ (Mb/s)
        #[arg(long, default_value_t = 20.0)]
        start_bitrate: f64,
        /// Ré-encode la dernière image si l'écran est statique (FPS constant)
        #[arg(long)]
        fill: bool,
        /// N'injecte AUCUNE entrée (ni clavier, ni souris, ni manette) : pour les tests
        #[arg(long)]
        no_input: bool,
        /// N'injecte pas le clavier
        #[arg(long)]
        no_keyboard: bool,
        /// N'injecte pas la souris
        #[arg(long)]
        no_mouse: bool,
        /// N'injecte pas les manettes
        #[arg(long)]
        no_gamepad: bool,
        /// Manette virtuelle : auto (choix du client, Xbox 360 par défaut), xbox (toujours Xbox 360)
        /// ou ds4 (toujours DualShock 4)
        #[arg(long, value_enum, default_value_t = GamepadTypeArg::Auto)]
        gamepad_type: GamepadTypeArg,
        /// Filtre les touches Windows gauche/droite envoyées par le client
        #[arg(long)]
        block_win_key: bool,
        /// Ne capture/envoie pas l'audio
        #[arg(long)]
        no_audio: bool,
        /// Inactivité (s) avant de fermer la session
        #[arg(long, default_value_t = 5.0)]
        idle_timeout: f64,
        /// Écran capturé au début de chaque session (1, 2… de gauche à droite, voir le journal
        /// de démarrage) ; défaut : l'écran principal. Le client peut en changer depuis son menu.
        #[arg(long)]
        display: Option<usize>,
        /// Interne : lancé par la tâche planifiée « Contrôle total » ; lit les options dans
        /// host-options.json et installe l'arrêt propre par événement nommé.
        #[arg(long, hide = true)]
        from_task: bool,
        /// Caché (tests) : simule l'état de sécurité observé sans lire Windows (aucune UAC réelle).
        #[arg(long, value_enum, hide = true)]
        simulate_secure: Option<SimulateArg>,
    },
}

/// Sous-commandes du mode « Contrôle total ».
#[derive(Subcommand)]
enum ElevationCmd {
    /// Active le mode : crée la tâche planifiée (une seule invite UAC) et lance le host élevé
    Enable {
        /// Lancer aussi le host élevé à chaque ouverture de session
        #[arg(long)]
        at_logon: bool,
    },
    /// Désactive le mode : supprime la tâche planifiée (une invite UAC)
    Disable,
    /// Affiche l'état de la tâche et si le processus courant est élevé
    Status,
    /// Interne : crée la tâche (déjà élevé via UAC) — appelé par `enable`
    #[command(hide = true)]
    InstallTask {
        #[arg(long)]
        at_logon: bool,
    },
    /// Interne : supprime la tâche (déjà élevé) — appelé par `disable`
    #[command(hide = true)]
    RemoveTask,
    /// Démarre le host via la tâche planifiée (pas d'UAC) : utilisé par grx-gui
    RunTask,
    /// Demande l'arrêt propre d'un host élevé (événement nommé) : utilisé par grx-gui
    #[command(hide = true)]
    Stop {
        #[arg(long)]
        pid: u32,
    },
}

fn main() -> Result<()> {
    let cli = load_cli();
    let dir = cli.data_dir.unwrap_or_else(|| default_data_dir("host"));
    let identity = Identity::load_or_create(&dir.join("identity.json"))?;
    let trust = TrustStore::load(&dir.join("trust.json"))?;
    let name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "host".into());
    let mut auth = HostAuth::new(identity, trust, &name);

    match cli.cmd {
        Cmd::Info => {
            println!("Empreinte du host : {}", auth.identity().fingerprint());
            println!("Dossier           : {}", dir.display());
        }
        Cmd::Invite { addr, port, ttl_min } => {
            let (addr, alt, notes) = if addr.eq_ignore_ascii_case("auto") {
                let a = access::resolve(&dir, port)?;
                (a.primary, a.ipv6_alt, a.notes)
            } else {
                (addr, None, Vec::new())
            };
            let max_min = MAX_INVITE_TTL_MS / 60_000;
            let ttl_min = ttl_min.min(max_min);
            let token = auth.trust.create_invite(ttl_min.saturating_mul(60_000), now_ms())?;
            let inv = Invite { addr, host_key: auth.identity().public(), token: Some(token) };
            println!("Invitation (valable {ttl_min} min, usage unique) :\n\n  {}\n", inv.to_uri());
            if let Some(alt) = alt {
                let inv6 = Invite { addr: alt, ..inv.clone() };
                println!("Alternative IPv6 (même jeton : un seul des deux liens servira) :\n\n  {}\n", inv6.to_uri());
            }
            for n in notes {
                println!("- {n}");
            }
            println!("Empreinte à vérifier côté client : {}", auth.identity().fingerprint());
        }
        Cmd::FirewallHelp { port } => {
            let exe = std::env::current_exe()?;
            let v = firewall::check(&exe, port);
            println!("État actuel ({}) : {}\n", v.state(), v.detail());
            println!("{}", firewall::help_text(&exe, port));
        }
        Cmd::List => {
            for c in auth.trust.clients() {
                let fp = grx_core::util::b32_decode(&c.key)
                    .and_then(|v| <[u8; 32]>::try_from(v).ok())
                    .map(|k| fingerprint(&k))
                    .unwrap_or_else(|| "?".into());
                let label = match c.role() {
                    grx_core::Role::ViewOnly => "spectateur",
                    grx_core::Role::Full => "complet",
                    grx_core::Role::Total => "contrôle total",
                };
                println!("{fp}  {label:<14}  {}", c.name);
            }
        }
        Cmd::Revoke { fingerprint } => {
            let n = auth.trust.revoke(&fingerprint)?;
            println!("{n} client(s) révoqué(s)");
        }
        Cmd::SetRole { fingerprint, role } => {
            let n = auth.trust.set_role(&fingerprint, role.role())?;
            if matches!(role, RoleArg::Total) {
                println!("ATTENTION : ce client (« Contrôle total ») pourra piloter les fenêtres administrateur du PC quand le host tourne élevé.");
            }
            println!("{n} client(s) modifié(s) ; un host en cours d'exécution coupe la session du client concerné (il doit se reconnecter)");
        }
        Cmd::Elevation(cmd) => return run_elevation(cmd),
        Cmd::Encoders { refresh, json } => encprobe::cmd_encoders(&dir, refresh, json)?,
        Cmd::FetchOpenh264 { dir, remove } => {
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_exe()?.parent().map(PathBuf::from).unwrap_or_default(),
            };
            if remove {
                grx_update::openh264::remove(&dir)?;
                println!("OpenH264 supprimé de {} (voie H.264 logicielle désactivée)", dir.display());
            } else {
                let p = grx_update::openh264::fetch(&dir).map_err(|e| anyhow::anyhow!(e))?;
                println!("{}\nInstallé : {} (SHA-256 vérifié)", grx_update::openh264::NOTICE, p.display());
            }
        }
        Cmd::Run {
            bind,
            no_upnp,
            external_port,
            upnp_lease,
            codec,
            encoder,
            chroma,
            test_pattern_hdr,
            fps,
            max_bitrate,
            start_bitrate,
            fill,
            no_input,
            no_keyboard,
            no_mouse,
            no_gamepad,
            gamepad_type,
            block_win_key,
            no_audio,
            idle_timeout,
            display,
            from_task,
            simulate_secure,
        } => {
            // Coordonnées physiques partout (DXGI, curseur, SendInput) même avec une mise à
            // l'échelle Windows différente par écran.
            set_dpi_aware();
            // Sonde des encodeurs en arrière-plan (cache sinon test réel) : l'énumération sert
            // tant qu'elle n'a pas fini.
            encprobe::start_background(dir.clone());
            let displays = grx_video::list_displays().unwrap_or_default();
            println!("Écrans capturables :");
            for (i, d) in displays.iter().enumerate() {
                println!("  {}", display::describe(i, d));
            }
            let display = match display {
                None => None,
                Some(n) if n >= 1 && n <= displays.len() => Some(displays[n - 1].device_name.clone()),
                Some(n) => bail!("--display {n} : écran inexistant (1 à {})", displays.len()),
            };
            if fps == 0 || max_bitrate <= 0.0 || start_bitrate <= 0.0 || idle_timeout <= 0.0 {
                bail!("--fps, --max-bitrate, --start-bitrate et --idle-timeout doivent être > 0");
            }
            if matches!(encoder, EncoderArg::Software) {
                if let Err(e) = grx_video::openh264::availability() {
                    bail!("--encoder software : {e}");
                }
            }
            let max_bps = (max_bitrate * 1e6) as u64;
            let opts = RunOpts {
                bind,
                idle_timeout: Duration::from_secs_f64(idle_timeout),
                pipeline: PipelineOpts {
                    codec: {
                        use grx_core::codec_neg::{CodecPref, VideoCodec};
                        match codec {
                            CodecArg::Auto => CodecPref::Auto,
                            CodecArg::Hevc => CodecPref::Force(VideoCodec::Hevc),
                            CodecArg::H264 => CodecPref::Force(VideoCodec::H264),
                            CodecArg::Av1 => CodecPref::Force(VideoCodec::Av1),
                        }
                    },
                    encoder: match encoder {
                        EncoderArg::Auto => grx_video::EncoderPref::Auto,
                        EncoderArg::Nvidia => grx_video::EncoderPref::Nvidia,
                        EncoderArg::Amd => grx_video::EncoderPref::Amd,
                        EncoderArg::Intel => grx_video::EncoderPref::Intel,
                        EncoderArg::Software => grx_video::EncoderPref::Software,
                    },
                    chroma: match chroma {
                        ChromaArg::Auto => grx_core::codec_neg::ChromaPref::Auto,
                        ChromaArg::_420 => grx_core::codec_neg::ChromaPref::Yuv420,
                        ChromaArg::_444 => grx_core::codec_neg::ChromaPref::Yuv444,
                    },
                    test_pattern_hdr,
                    fps,
                    fill,
                    audio: !no_audio,
                    input: !no_input,
                    video: true,
                    injector: InjectorConfig {
                        allow_keyboard: !no_keyboard,
                        allow_mouse: !no_mouse,
                        allow_gamepad: !no_gamepad,
                        block_win_key,
                        gamepad_policy: match gamepad_type {
                            GamepadTypeArg::Auto => GamepadPolicy::Auto,
                            GamepadTypeArg::Xbox => GamepadPolicy::Xbox,
                            GamepadTypeArg::Ds4 => GamepadPolicy::Ds4,
                        },
                        ..Default::default()
                    },
                    initial_bitrate_bps: ((start_bitrate * 1e6) as u64).min(max_bps),
                    max_bitrate_bps: max_bps,
                    display,
                },
            };
            if no_input {
                println!("Mode --no-input : aucune entrée ne sera injectée.");
            }
            if !no_input {
                for (off, what) in [(no_keyboard, "clavier"), (no_mouse, "souris"), (no_gamepad, "manettes")] {
                    if off {
                        println!("Entrées {what} désactivées (--no-{}).", match what { "clavier" => "keyboard", "souris" => "mouse", _ => "gamepad" });
                    }
                }
                if block_win_key {
                    println!("Touches Windows du client filtrées (--block-win-key).");
                }
            }
            let sim = simulate_secure.map(|s| match s {
                SimulateArg::Normal => secure::Simulate::Normal,
                SimulateArg::Elevated => secure::Simulate::Elevated,
                SimulateArg::Secure => secure::Simulate::Secure,
                SimulateArg::Cycle => secure::Simulate::Cycle,
            });
            // Observation lecture seule du bureau/intégrité (ou simulation) : le serveur en lit
            // l'état pour autoriser/bloquer et prévenir le client (GRX-SEC).
            secure::start(sim);
            if from_task {
                println!("Host élevé lancé par la tâche planifiée « Contrôle total ».");
            }
            run_host(&dir, auth, opts, no_upnp, external_port, Duration::from_secs(upnp_lease), from_task)?
        }
    }
    Ok(())
}

/// Analyse la ligne de commande. Pour `run --from-task` (tâche planifiée), les options de l'hôte
/// élevé ne sont PAS sur la ligne de commande (figée, créée sous UAC) : on les relit dans
/// `host-options.json` (dossier de données par défaut) et on ré-analyse en les ajoutant. Les
/// arguments y ont déjà été validés par la liste blanche avant écriture, et sont revérifiés ici.
fn load_cli() -> Cli {
    let cli = Cli::parse();
    if let Cmd::Run { from_task: true, .. } = &cli.cmd {
        let dir = cli.data_dir.clone().unwrap_or_else(|| default_data_dir("host"));
        match elevation::read_task_options(&dir) {
            Ok(extra) if !extra.is_empty() => {
                let mut argv: Vec<String> = vec!["grx-host".into()];
                if let Some(d) = &cli.data_dir {
                    argv.push("--data-dir".into());
                    argv.push(d.to_string_lossy().into_owned());
                }
                argv.push("run".into());
                argv.push("--from-task".into());
                argv.extend(extra);
                match Cli::try_parse_from(&argv) {
                    Ok(c) => return c,
                    Err(e) => eprintln!("options de la tâche ignorées (ré-analyse impossible) : {e}"),
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("host-options.json ignoré : {e}"),
        }
    }
    cli
}

/// Sous-commandes `grx-host elevation …`.
fn run_elevation(cmd: ElevationCmd) -> Result<()> {
    match cmd {
        ElevationCmd::Enable { at_logon } => {
            let exe = std::env::current_exe()?;
            // Vérifie AVANT l'UAC (message clair, pas d'invite inutile).
            elevation::preflight(&exe)?;
            println!("Activation du Contrôle total : une autorisation Windows (UAC) va être demandée…");
            let mut args = vec!["elevation", "install-task"];
            if at_logon {
                args.push("--at-logon");
            }
            let code = elevation::run_elevated_self(&args)?;
            if code != 0 {
                bail!("l'opération élevée a échoué (code {code})");
            }
            let st = elevation::status();
            if st.task_present && st.task_valid {
                println!("Contrôle total activé : la tâche « {} » est installée.", grx_core::elevation::TASK_NAME);
                println!("grx-gui (ou `grx-host elevation run-task`) peut maintenant lancer le host élevé sans UAC.");
            } else {
                bail!("la tâche n'a pas été créée : relancez en acceptant l'invite UAC.");
            }
        }
        ElevationCmd::Disable => {
            println!("Désactivation du Contrôle total : une autorisation Windows (UAC) va être demandée…");
            let code = elevation::run_elevated_self(&["elevation", "remove-task"])?;
            if code != 0 {
                bail!("l'opération élevée a échoué (code {code})");
            }
            println!("Contrôle total désactivé : la tâche planifiée a été supprimée.");
        }
        ElevationCmd::Status => {
            let st = elevation::status();
            println!("Processus courant élevé        : {}", oui_non(st.current_elevated));
            println!("Dossier protégé (Program Files): {}", oui_non(st.protected_location));
            println!("Tâche planifiée présente       : {}", oui_non(st.task_present));
            if st.task_present {
                println!("Tâche valide (élevée, bon exe) : {}", oui_non(st.task_valid));
                println!("Lancement à l'ouverture session: {}", oui_non(st.at_logon));
                if let Some(c) = &st.task_command {
                    println!("Exécutable de la tâche         : {c}");
                }
            }
        }
        ElevationCmd::InstallTask { at_logon } => {
            if !elevation::is_elevated() {
                bail!("install-task doit être exécuté élevé (passez par `elevation enable`).");
            }
            elevation::create_task(at_logon)?;
        }
        ElevationCmd::RemoveTask => {
            if !elevation::is_elevated() {
                bail!("remove-task doit être exécuté élevé (passez par `elevation disable`).");
            }
            elevation::delete_task()?;
        }
        ElevationCmd::RunTask => {
            elevation::run_task()?;
            println!("Host élevé démarré via la tâche planifiée (aucune UAC).");
        }
        ElevationCmd::Stop { pid } => {
            if elevation::signal_stop(pid) {
                println!("Arrêt demandé au host élevé (pid {pid}).");
            } else {
                bail!("aucun host élevé joignable (pid {pid}) : déjà arrêté ?");
            }
        }
    }
    Ok(())
}

fn oui_non(b: bool) -> &'static str {
    if b {
        "oui"
    } else {
        "non"
    }
}

/// `run` : socket, status.json, redirection automatique et pare-feu en arrière-plan, puis la
/// boucle réseau ; à l'arrêt (Ctrl+C ou fin), suppression de la redirection et de status.json.
fn run_host(
    dir: &std::path::Path,
    auth: HostAuth,
    opts: RunOpts,
    no_upnp: bool,
    external_port: Option<u16>,
    lease: Duration,
    from_task: bool,
) -> Result<()> {
    use status::{FirewallStatus, HostStatus, StatusFile, UpnpStatus};
    let sock = server::bind_socket(opts.bind)?;
    let bound = sock.local_addr()?;
    let ipv6: Vec<String> = grx_portmap::global_ipv6().into_iter().map(|g| g.addr.to_string()).collect();
    let mapping_local = portmap_task::local_for_bind(bound);
    let upnp = match (&mapping_local, no_upnp) {
        (_, true) => UpnpStatus { state: "disabled".into(), ..Default::default() },
        (Err(why), _) => UpnpStatus { state: "disabled".into(), error: Some(why.clone()), ..Default::default() },
        (Ok(_), false) => UpnpStatus { state: "pending".into(), ..Default::default() },
    };
    let st = StatusFile::create(
        dir,
        HostStatus {
            pid: std::process::id(),
            bind: bound.to_string(),
            started_ms: now_ms(),
            upnp,
            ipv6: ipv6.clone(),
            firewall: FirewallStatus { state: "checking".into(), detail: None },
        },
    );

    let task = match (mapping_local, no_upnp) {
        (_, true) => {
            println!("[upnp] Désactivé (--no-upnp) : redirigez le port UDP {} à la main pour un accès Internet.", bound.port());
            None
        }
        (Err(why), _) => {
            println!("[upnp] {why}");
            None
        }
        (Ok(local), false) => {
            Some(portmap_task::PortmapTask::start(local, external_port.unwrap_or(bound.port()), lease, st.clone()))
        }
    };

    if let Some(v6) = ipv6.first() {
        if bound.is_ipv6() {
            println!("IPv6 : joignable aussi sur [{v6}]:{} (autorisez seulement le port UDP dans le pare-feu Windows)", bound.port());
        } else {
            println!("IPv6 globale disponible ({v6}) : lancez avec --bind [::]:{} pour l'accepter aussi.", bound.port());
        }
    }

    // Pare-feu (lecture seule) en arrière-plan : ne retarde pas le démarrage.
    {
        let st = st.clone();
        let port = bound.port();
        let _ = std::thread::Builder::new().name("grx-firewall".into()).spawn(move || {
            let Ok(exe) = std::env::current_exe() else { return };
            let v = firewall::check(&exe, port);
            match &v {
                firewall::Verdict::Allowed(_) => {}
                firewall::Verdict::Unknown(e) => eprintln!("[pare-feu] Vérification impossible ({e}) ; voir `grx-host firewall-help`."),
                _ => {
                    eprintln!("[pare-feu] Attention : {}. Les clients extérieurs seront bloqués.", v.detail());
                    eprintln!("[pare-feu] Commande à lancer en administrateur (détails : grx-host firewall-help --port {port}) :");
                    eprintln!(
                        "  netsh advfirewall firewall add rule name=\"{}\" dir=in action=allow protocol=UDP localport={port} program=\"{}\" profile=any",
                        firewall::rule_name(port),
                        exe.display()
                    );
                }
            }
            st.update(|s| s.firewall = FirewallStatus { state: v.state().into(), detail: Some(v.detail()) });
        });
    }

    // Host lancé élevé par la tâche « Contrôle total » : grx-gui (non élevé) ne peut pas lui
    // envoyer Ctrl+C, on ajoute donc un arrêt par événement nommé (ACL : utilisateur courant).
    // Ctrl+C reste géré (console) via `server::serve`, qui installe aussi le gestionnaire.
    let res = if from_task {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let quit = Arc::new(AtomicBool::new(false));
        {
            let q = quit.clone();
            let _ = ctrlc::set_handler(move || q.store(true, Ordering::SeqCst));
        }
        if let Some(ev) = elevation::create_stop_event() {
            let q = quit.clone();
            // Un seul fil bloqué sur l'événement ; au déclenchement, demande l'arrêt propre.
            let _ = std::thread::Builder::new().name("grx-stop".into()).spawn(move || {
                ev.wait();
                println!("Arrêt demandé par événement nommé (grx-gui).");
                q.store(true, Ordering::SeqCst);
            });
        } else {
            eprintln!("[sécurité] événement d'arrêt non créé : seul Ctrl+C arrêtera ce host.");
        }
        server::serve(sock, auth, opts, quit)
    } else {
        server::run(sock, auth, opts)
    };
    if let Some(t) = task {
        t.stop(Duration::from_secs(8));
    }
    st.remove();
    res
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_type_flag_parses() {
        for (arg, want) in [("auto", GamepadTypeArg::Auto), ("xbox", GamepadTypeArg::Xbox), ("ds4", GamepadTypeArg::Ds4)] {
            match Cli::try_parse_from(["grx-host", "run", "--gamepad-type", arg]).unwrap().cmd {
                Cmd::Run { gamepad_type, .. } => assert_eq!(gamepad_type, want),
                _ => panic!(),
            }
        }
        match Cli::try_parse_from(["grx-host", "run"]).unwrap().cmd {
            Cmd::Run { gamepad_type, .. } => assert_eq!(gamepad_type, GamepadTypeArg::Auto),
            _ => panic!(),
        }
        assert!(Cli::try_parse_from(["grx-host", "run", "--gamepad-type", "wii"]).is_err());
    }

    #[test]
    fn run_flags_parse() {
        let cli = Cli::try_parse_from([
            "grx-host", "run", "--no-keyboard", "--no-mouse", "--no-gamepad", "--block-win-key", "--no-input",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Run { no_keyboard, no_mouse, no_gamepad, block_win_key, no_input, .. } => {
                assert!(no_keyboard && no_mouse && no_gamepad && block_win_key && no_input);
            }
            _ => panic!("sous-commande run attendue"),
        }
        match Cli::try_parse_from(["grx-host", "run"]).unwrap().cmd {
            Cmd::Run { no_keyboard, no_mouse, no_gamepad, block_win_key, no_upnp, external_port, upnp_lease, .. } => {
                assert!(!no_keyboard && !no_mouse && !no_gamepad && !block_win_key);
                assert!(!no_upnp, "UPnP actif par défaut");
                assert_eq!((external_port, upnp_lease), (None, 3600));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn upnp_and_invite_flags_parse() {
        match Cli::try_parse_from(["grx-host", "run", "--no-upnp", "--external-port", "7790", "--bind", "[::]:7777"]).unwrap().cmd {
            Cmd::Run { no_upnp, external_port, bind, .. } => {
                assert!(no_upnp);
                assert_eq!(external_port, Some(7790));
                assert!(bind.is_ipv6());
            }
            _ => panic!(),
        }
        match Cli::try_parse_from(["grx-host", "invite", "--addr", "auto"]).unwrap().cmd {
            Cmd::Invite { addr, port, .. } => assert_eq!((addr.as_str(), port), ("auto", 7777)),
            _ => panic!(),
        }
        match Cli::try_parse_from(["grx-host", "firewall-help", "--port", "9000"]).unwrap().cmd {
            Cmd::FirewallHelp { port } => assert_eq!(port, 9000),
            _ => panic!(),
        }
    }

    #[test]
    fn set_role_parses_both_roles_and_rejects_others() {
        for (arg, view_only) in [("view-only", true), ("full", false)] {
            match Cli::try_parse_from(["grx-host", "set-role", "ABCD-EFGH", arg]).unwrap().cmd {
                Cmd::SetRole { fingerprint, role } => {
                    assert_eq!(fingerprint, "ABCD-EFGH");
                    assert_eq!(matches!(role, RoleArg::ViewOnly), view_only);
                }
                _ => panic!("set-role attendu"),
            }
        }
        assert!(Cli::try_parse_from(["grx-host", "set-role", "ABCD", "admin"]).is_err());
    }
}

/// Processus « per-monitor DPI aware v2 » : sans ça, Windows virtualise les coordonnées du bureau
/// (DXGI, curseur) quand la mise à l'échelle n'est pas de 100 %.
fn set_dpi_aware() {
    #[cfg(windows)]
    // SAFETY : appel sans pointeur ; échoue sans effet si déjà fixé (manifeste, appel antérieur).
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
}

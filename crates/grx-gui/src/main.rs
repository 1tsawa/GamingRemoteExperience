//! grx-gui : interface graphique d'appairage (onglets Host et Client).
//!
//! Voir `docs/ARCHITECTURE.md` (jalon GUI). Aucun appel réseau externe ; tout passe par
//! `grx-core` (TrustStore, ProfileStore) et par les exécutables `grx-host` / `grx-client`
//! installés à côté de `grx-gui.exe`.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod backend;
mod hoststatus;
mod logic;
mod qr;
mod settings;
mod theme;
mod update;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, ValueEnum)]
enum TabArg {
    Host,
    Client,
}

#[derive(Parser)]
#[command(name = "grx-gui", version, about = "Interface graphique d'appairage GamingRemoteExperience")]
struct Cli {
    /// Dossier de données du host (identité, trust.json). Défaut : %APPDATA%\GamingRemoteExperience\host
    #[arg(long, env = "GRX_DATA_DIR_HOST")]
    data_dir_host: Option<PathBuf>,
    /// Dossier de données du client (identité, hosts.json). Défaut : %APPDATA%\GamingRemoteExperience\client
    #[arg(long, env = "GRX_DATA_DIR_CLIENT")]
    data_dir_client: Option<PathBuf>,

    // Options de démonstration / test : préparent l'état initial sans cliquer.
    /// Onglet affiché au démarrage
    #[arg(long, value_enum, default_value = "host", hide = true)]
    tab: TabArg,
    /// Pré-remplit le champ « lien » de l'onglet Client
    #[arg(long, hide = true)]
    prefill_link: Option<String>,
    /// Pré-remplit l'adresse publique de l'onglet Host
    #[arg(long, hide = true)]
    prefill_addr: Option<String>,
    /// Crée une invitation dès le démarrage (test)
    #[arg(long, hide = true)]
    auto_invite: bool,
    /// Copie le lien de l'invitation créée dans le presse-papiers (test)
    #[arg(long, hide = true)]
    auto_copy_invite: bool,
    /// Démarre le host (options mémorisées) dès l'ouverture (test)
    #[arg(long, hide = true)]
    auto_start_host: bool,
    /// Ferme sans demander de confirmation (test) ; le host lancé d'ici est quand même arrêté proprement
    #[arg(long, hide = true)]
    quit_without_confirm: bool,
    /// Force --no-input --no-audio pour le host lancé depuis la GUI (test)
    #[arg(long, hide = true)]
    force_safe_host: bool,
    /// Ouvre la fenêtre de commande pare-feu dès que le host tourne (test)
    #[arg(long, hide = true)]
    auto_firewall_help: bool,
    /// Arrête proprement les grx-host / grx-client installés dans ce dossier puis quitte (installateur)
    #[arg(long, hide = true)]
    stop_grx_processes: bool,
    /// Recherche et installe une mise à jour sans fenêtre, puis quitte (tests ; code de sortie)
    #[arg(long, hide = true)]
    update_now: bool,
    /// Position initiale de la fenêtre (x,y)
    #[arg(long, hide = true, value_parser = parse_pos)]
    window_pos: Option<(f32, f32)>,
}

fn parse_pos(s: &str) -> Result<(f32, f32), String> {
    let (x, y) = s.split_once(',').ok_or("attendu x,y")?;
    Ok((x.trim().parse().map_err(|_| "x invalide")?, y.trim().parse().map_err(|_| "y invalide")?))
}

/// Option enregistrée par l'installateur pour le protocole `grx://`.
const OPEN_LINK_FLAG: &str = "--open-link";

/// `grx-gui.exe --open-link "grx://…"` (association de protocole `grx://`) : TOUT ce qui suit
/// `--open-link` est le lien (aucune autre option n'est interprétée, même si une page web fabrique
/// un lien contenant des guillemets). Le lien doit commencer par `grx://` ; il n'est ni journalisé
/// ni écrit sur disque : il pré-remplit seulement l'onglet Client, et rien n'est envoyé tant que
/// l'utilisateur n'a pas vérifié l'empreinte et cliqué « Appairer et connecter ».
fn link_from_args(args: &[String]) -> Option<Option<String>> {
    if args.get(1).map(String::as_str) != Some(OPEN_LINK_FLAG) {
        return None;
    }
    let link = args[2..].join(" ");
    let link = link.trim();
    let ok = link.len() <= 1024
        && link.get(..6).is_some_and(|p| p.eq_ignore_ascii_case("grx://"))
        && !link.chars().any(char::is_control);
    Some(ok.then(|| link.to_string()))
}

/// Sous-système Windows : pas de console. Pour `--version`, `--help` et `--update-now`, on
/// s'attache à la console du terminal parent (si elle existe) pour y écrire.
fn attach_parent_console() {
    #[cfg(windows)]
    // SAFETY : appel Win32 sans pointeur.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() -> eframe::Result {
    let raw: Vec<String> = std::env::args().collect();
    if let Some(link) = link_from_args(&raw) {
        let paths = backend::Paths::resolve(
            std::env::var_os("GRX_DATA_DIR_HOST").map(PathBuf::from),
            std::env::var_os("GRX_DATA_DIR_CLIENT").map(PathBuf::from),
        );
        let opts = app::StartOptions { tab: app::Tab::Client, prefill_link: link, opened_from_link: true, ..Default::default() };
        return run_window(paths, opts, None);
    }
    if raw.iter().skip(1).any(|a| matches!(a.as_str(), "--version" | "-V" | "--help" | "-h" | "--update-now")) {
        attach_parent_console();
    }
    let cli = Cli::parse();
    if cli.stop_grx_processes {
        std::process::exit(update::stop_processes_cli());
    }
    if cli.update_now {
        std::process::exit(update::update_now_cli());
    }
    let paths = backend::Paths::resolve(cli.data_dir_host, cli.data_dir_client);
    let opts = app::StartOptions {
        tab: match cli.tab {
            TabArg::Host => app::Tab::Host,
            TabArg::Client => app::Tab::Client,
        },
        prefill_link: cli.prefill_link,
        prefill_addr: cli.prefill_addr,
        auto_invite: cli.auto_invite,
        auto_copy_invite: cli.auto_copy_invite,
        auto_start_host: cli.auto_start_host,
        quit_without_confirm: cli.quit_without_confirm,
        force_safe_host: cli.force_safe_host,
        auto_firewall_help: cli.auto_firewall_help,
        opened_from_link: false,
    };
    run_window(paths, opts, cli.window_pos)
}

fn run_window(paths: backend::Paths, opts: app::StartOptions, window_pos: Option<(f32, f32)>) -> eframe::Result {
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("GamingRemoteExperience — Appairage")
        .with_inner_size([1040.0, 860.0])
        .with_min_inner_size([760.0, 560.0]);
    if let Some((x, y)) = window_pos {
        viewport = viewport.with_position([x, y]);
    }
    let native = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("grx-gui", native, Box::new(move |cc| Ok(Box::new(app::App::new(cc, paths, opts)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn open_link_takes_everything_after_the_flag_and_requires_grx_scheme() {
        assert_eq!(link_from_args(&a(&["gui"])), None);
        assert_eq!(link_from_args(&a(&["gui", "--tab", "client"])), None);
        assert_eq!(link_from_args(&a(&["gui", "--open-link", "grx://1.2.3.4:7777/K/T"])), Some(Some("grx://1.2.3.4:7777/K/T".into())));
        assert_eq!(link_from_args(&a(&["gui", "--open-link", "GRX://h:1/K"])), Some(Some("GRX://h:1/K".into())));
        // Options glissées après le lien : jamais interprétées (elles font partie du « lien »).
        let r = link_from_args(&a(&["gui", "--open-link", "grx://x", "--auto-start-host"])).unwrap();
        assert_eq!(r.as_deref(), Some("grx://x --auto-start-host"));
        assert_eq!(link_from_args(&a(&["gui", "--open-link", "https://evil"])), Some(None));
        assert_eq!(link_from_args(&a(&["gui", "--open-link"])), Some(None));
        assert_eq!(link_from_args(&a(&["gui", "--open-link", "grx://a\u{7}"])), Some(None));
        let long = format!("grx://{}", "a".repeat(2000));
        assert_eq!(link_from_args(&a(&["gui", "--open-link", &long])), Some(None));
    }
}

//! Mode de l'installation actuelle et lancement du setup vérifié.

use std::path::{Path, PathBuf};

use crate::{UpdateError, VerifiedSetup};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMode {
    /// Installé pour l'utilisateur courant (%LOCALAPPDATA%\Programs, sans droits admin).
    CurrentUser,
    /// Installé pour toute la machine (Program Files, admin : l'UAC apparaîtra).
    AllUsers,
    /// Pas installé par le setup (ex. `target\release`) : setup lancé en mode interactif.
    NotInstalled,
}

impl InstallMode {
    pub fn label(self) -> &'static str {
        match self {
            InstallMode::CurrentUser => "installé pour l'utilisateur courant",
            InstallMode::AllUsers => "installé pour tous les utilisateurs (administrateur)",
            InstallMode::NotInstalled => "non installé (copie de développement)",
        }
    }
}

/// Dossier de l'exécutable courant.
pub fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
}

fn norm(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_lowercase();
    let s = s.strip_prefix("\\\\?\\").map(str::to_string).unwrap_or(s);
    s.trim_end_matches('\\').to_string()
}

/// `dir` est-il `root` ou un sous-dossier de `root` ? (insensible à la casse, séparateurs normalisés)
pub fn is_within(dir: &Path, root: &Path) -> bool {
    let (d, r) = (norm(dir), norm(root));
    !r.is_empty() && (d == r || d.starts_with(&(r + "\\")))
}

/// Détermine le mode d'après le dossier d'installation : présence du désinstalleur Inno Setup
/// (`unins000.exe`), puis emplacement (Program Files = machine, sinon utilisateur).
pub fn detect(install_dir: &Path, program_files: &[PathBuf]) -> InstallMode {
    if !install_dir.join("unins000.exe").is_file() {
        return InstallMode::NotInstalled;
    }
    if program_files.iter().any(|pf| is_within(install_dir, pf)) {
        InstallMode::AllUsers
    } else {
        InstallMode::CurrentUser
    }
}

/// Dossiers « Program Files » de la machine (variables d'environnement Windows).
pub fn program_files_dirs() -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|v| std::env::var_os(v))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .collect()
}

pub fn detect_current() -> InstallMode {
    current_exe_dir().map(|d| detect(&d, &program_files_dirs())).unwrap_or(InstallMode::NotInstalled)
}

/// Arguments du setup. `/RELAUNCH` (paramètre propre à grx.iss) relance grx-gui à la fin.
pub fn setup_args(mode: InstallMode, relaunch: bool) -> Vec<String> {
    let mut a: Vec<String> = match mode {
        InstallMode::NotInstalled => Vec::new(),
        InstallMode::CurrentUser => vec!["/SILENT".into(), "/SUPPRESSMSGBOXES".into(), "/NORESTART".into(), "/CURRENTUSER".into()],
        InstallMode::AllUsers => vec!["/SILENT".into(), "/SUPPRESSMSGBOXES".into(), "/NORESTART".into(), "/ALLUSERS".into()],
    };
    if relaunch && mode != InstallMode::NotInstalled {
        a.push("/RELAUNCH".into());
    }
    a
}

/// Revérifie le setup puis le lance (sans attendre). L'appelant doit quitter ensuite pour que le
/// setup puisse remplacer les fichiers.
pub fn launch(setup: &VerifiedSetup, mode: InstallMode, relaunch: bool) -> Result<u32, UpdateError> {
    setup.reverify()?;
    let child = std::process::Command::new(&setup.path)
        .args(setup_args(mode, relaunch))
        .current_dir(setup.path.parent().unwrap_or(Path::new(".")))
        .spawn()
        .map_err(|e| UpdateError::Launch(e.to_string()))?;
    Ok(child.id())
}

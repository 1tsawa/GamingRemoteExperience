//! Mode « Contrôle total » côté host : création/suppression d'une tâche planifiée qui lance
//! `grx-host run --from-task` ÉLEVÉ, sans invite UAC à chaque démarrage (une seule UAC à
//! l'activation). La logique pure (XML de la tâche, liste blanche des options, noms) est dans
//! `grx_core::elevation` ; ce module ne contient que les appels système et l'orchestration.
//!
//! Modèle de menace (voir docs/ARCHITECTURE.md § Contrôle total) :
//! * La tâche lance un exécutable ÉLEVÉ à la demande, sans ré-authentification. Il faut donc que
//!   `grx-host.exe` soit dans un dossier qu'un utilisateur standard NE PEUT PAS modifier (sinon il
//!   pourrait remplacer l'exe et obtenir une élévation silencieuse). D'où le refus d'activer si
//!   l'exe n'est pas sous `C:\Program Files` (installation « pour tous »), vérifié en plus par un
//!   test d'écriture réel du dossier.
//! * Les options de lancement sont lues dans `host-options.json` (dossier de données de
//!   l'utilisateur) et filtrées par `validate_run_args` : pas de `--data-dir`, pas de chemin, pas
//!   d'option cachée. Au pire, l'utilisateur ne change que SES propres paramètres de capture.
//! * Arrêt propre depuis grx-gui (non élevé) : un événement nommé par PID, dont l'ACL n'autorise
//!   que l'utilisateur courant (personne d'autre ne peut le déclencher ni le détourner).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use grx_core::elevation as core;

/// Résultat de `elevation status`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// La tâche planifiée existe.
    pub task_present: bool,
    /// Elle pointe bien vers l'exe courant, en « HighestAvailable ».
    pub task_valid: bool,
    /// Elle se lance aussi à l'ouverture de session.
    pub at_logon: bool,
    /// Chemin de l'exe que la tâche lance (tel que lu).
    pub task_command: Option<String>,
    /// Le processus courant est élevé.
    pub current_elevated: bool,
    /// L'exe est dans un dossier protégé (installation machine).
    pub protected_location: bool,
}

/// Le dossier `dir` est-il non modifiable par un utilisateur standard ?
///
/// Deux conditions cumulées : (1) le chemin est sous un `Program Files` connu (défense en
/// profondeur, lisible par l'utilisateur) ; (2) un test d'écriture réel échoue quand on n'est pas
/// élevé (preuve que l'ACL protège vraiment, même pour un dossier renommé).
pub fn protected_dir(dir: &Path) -> bool {
    program_files_dirs().iter().any(|pf| core::is_within_ci(&dir.to_string_lossy(), &pf.to_string_lossy()))
}

/// Teste si on peut écrire dans `dir` (création puis suppression d'un fichier témoin).
pub fn can_write(dir: &Path) -> bool {
    let probe = dir.join(format!(".grx-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Dossiers « Program Files » et « Program Files (x86) ».
///
/// Lus via l'API des dossiers connus de Windows, et NON via les variables d'environnement
/// (`%ProgramFiles%`…) : un programme de l'utilisateur peut modifier son environnement, qui est
/// hérité par le processus élevé, et ferait alors passer un dossier inscriptible pour protégé.
pub fn program_files_dirs() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Com::CoTaskMemFree;
        use windows_sys::Win32::UI::Shell::{FOLDERID_ProgramFiles, FOLDERID_ProgramFilesX86, SHGetKnownFolderPath};
        let mut v = Vec::new();
        for id in [&FOLDERID_ProgramFiles, &FOLDERID_ProgramFilesX86] {
            let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
            // SAFETY: id pointe vers un GUID statique ; p reçoit une chaîne allouée par le shell,
            // lue jusqu'au zéro terminal puis libérée par CoTaskMemFree (même en cas d'échec).
            let hr = unsafe { SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p) };
            if hr >= 0 && !p.is_null() {
                let s = unsafe {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
                };
                if !s.is_empty() {
                    v.push(PathBuf::from(s));
                }
            }
            unsafe { CoTaskMemFree(p as *const std::ffi::c_void) };
        }
        if !v.is_empty() {
            return v;
        }
    }
    vec![PathBuf::from(r"C:\Program Files")]
}

/// Vérifie que l'activation est sûre : exe absolu, dans un dossier protégé, non modifiable sans
/// élévation. Retourne un message d'erreur explicite si refusé.
pub fn preflight(exe: &Path) -> Result<()> {
    if !exe.is_absolute() {
        bail!("chemin de grx-host.exe non absolu");
    }
    let dir = exe.parent().context("dossier de grx-host.exe introuvable")?;
    if !protected_dir(dir) {
        bail!(
            "Contrôle total refusé : grx-host.exe est dans « {} », un dossier modifiable par un utilisateur standard.\n\
             Un host lancé élevé à la demande doit être installé « pour tous les utilisateurs » dans Program Files,\n\
             sinon quiconque pourrait remplacer l'exécutable et obtenir une élévation silencieuse.\n\
             Réinstallez avec l'option « pour tous les utilisateurs » (installation machine, une UAC).",
            dir.display()
        );
    }
    // Défense en profondeur : si on n'est PAS élevé et qu'on peut quand même écrire dans le
    // dossier, c'est que l'ACL ne protège pas → refus.
    if !is_elevated() && can_write(dir) {
        bail!(
            "Contrôle total refusé : le dossier « {} » est inscriptible sans élévation (ACL non protégée).",
            dir.display()
        );
    }
    Ok(())
}

/// Le processus courant est-il élevé (intégrité ≥ élevée) ?
pub fn is_elevated() -> bool {
    crate::secure::own_integrity().is_some_and(|r| r >= crate::secure::HIGH_RID)
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenEventW, OpenProcessToken, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE, SYNCHRONIZATION_SYNCHRONIZE,
    };
    use windows_sys::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// SID de l'utilisateur courant (`S-1-5-21-…`), pour le principal de la tâche.
    pub fn current_user_sid() -> Result<String> {
        // SAFETY : handles fermés, tampon de taille annoncée, pointeur libéré par LocalFree.
        unsafe {
            let mut tok: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
                bail!("OpenProcessToken a échoué");
            }
            let mut len = 0u32;
            GetTokenInformation(tok, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len.max(8) as usize];
            let ok = GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) != 0;
            CloseHandle(tok);
            if !ok {
                bail!("GetTokenInformation(TokenUser) a échoué");
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut str_sid: windows_sys::core::PWSTR = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut str_sid) == 0 || str_sid.is_null() {
                bail!("ConvertSidToStringSidW a échoué");
            }
            let mut n = 0;
            while *str_sid.add(n) != 0 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(str_sid, n));
            windows_sys::Win32::Foundation::LocalFree(str_sid.cast());
            if !core::valid_sid(&s) {
                bail!("SID inattendu : {s}");
            }
            Ok(s)
        }
    }

    /// Lance `grx-host <args>` ÉLEVÉ via ShellExecute « runas » (invite UAC unique) et attend la
    /// fin. Retourne le code de sortie. Utilisé pour `elevation enable/disable` : la sous-commande
    /// élevée crée/supprime la tâche.
    pub fn run_elevated_self(args: &[&str]) -> Result<i32> {
        let exe = std::env::current_exe()?;
        let params = args.iter().map(|a| format!("\"{}\"", a.replace('"', "\\\""))).collect::<Vec<_>>().join(" ");
        let verb = wide("runas");
        let file = wide(&exe.to_string_lossy());
        let par = wide(&params);
        // SAFETY : structure initialisée à zéro, pointeurs valides le temps de l'appel ; handle de
        // processus fermé après attente.
        unsafe {
            let mut info: SHELLEXECUTEINFOW = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
            info.fMask = SEE_MASK_NOCLOSEPROCESS;
            info.lpVerb = verb.as_ptr();
            info.lpFile = file.as_ptr();
            info.lpParameters = par.as_ptr();
            info.nShow = 0; // SW_HIDE
            if ShellExecuteExW(&mut info) == 0 {
                bail!("élévation annulée ou impossible (UAC refusé ?)");
            }
            if info.hProcess.is_null() {
                return Ok(0);
            }
            WaitForSingleObject(info.hProcess, INFINITE);
            let mut code = 0u32;
            windows_sys::Win32::System::Threading::GetExitCodeProcess(info.hProcess, &mut code);
            CloseHandle(info.hProcess);
            Ok(code as i32)
        }
    }

    /// Attributs de sécurité « utilisateur courant + SYSTEM uniquement » (D:P(A;;...;;;<sid>)…),
    /// pour que l'événement d'arrêt ne soit déclenchable que par l'utilisateur (et le host élevé).
    fn user_only_sa(sid: &str) -> Result<(SECURITY_ATTRIBUTES, windows_sys::Win32::Security::PSECURITY_DESCRIPTOR)> {
        // EVENT_ALL_ACCESS (0x1F0003) à l'utilisateur et à SYSTEM ; personne d'autre.
        let sddl = format!("D:P(A;;0x1F0003;;;{sid})(A;;0x1F0003;;;SY)");
        let w = wide(&sddl);
        // SAFETY : descripteur alloué par l'API, référencé par le SA jusqu'à LocalFree (par l'appelant).
        unsafe {
            let mut psd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut psd, std::ptr::null_mut()) == 0 {
                bail!("descripteur de sécurité invalide");
            }
            let mut sa: SECURITY_ATTRIBUTES = std::mem::zeroed();
            sa.nLength = std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32;
            sa.lpSecurityDescriptor = psd;
            sa.bInheritHandle = 0;
            Ok((sa, psd))
        }
    }

    /// Crée l'événement d'arrêt du host courant (manuel reset), ACL = utilisateur courant seul.
    /// Renvoie un garde qui ferme le handle à la destruction. `None` hors Windows ou si échec.
    pub fn create_stop_event() -> Option<StopEvent> {
        let name = core::stop_event_name(std::process::id());
        let sid = current_user_sid().ok()?;
        let (sa, psd) = user_only_sa(&sid).ok()?;
        let wname = wide(&name);
        // SAFETY : SA valide le temps de l'appel ; handle conservé dans le garde.
        unsafe {
            let h = CreateEventW(&sa, 1, 0, wname.as_ptr());
            windows_sys::Win32::Foundation::LocalFree(psd.cast());
            if h.is_null() {
                return None;
            }
            Some(StopEvent { handle: h })
        }
    }

    /// Événement d'arrêt nommé ; `wait_thread` bloque jusqu'à `SetEvent`.
    pub struct StopEvent {
        handle: HANDLE,
    }

    // SAFETY : un HANDLE d'événement est utilisable depuis un autre thread.
    unsafe impl Send for StopEvent {}

    impl StopEvent {
        /// Bloque jusqu'à ce que l'événement soit déclenché (arrêt demandé).
        pub fn wait(&self) {
            // SAFETY : handle valide tant que `self` vit.
            unsafe {
                WaitForSingleObject(self.handle, INFINITE);
            }
        }
    }

    impl Drop for StopEvent {
        fn drop(&mut self) {
            // SAFETY : handle créé par CreateEventW, fermé une seule fois.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }

    /// Déclenche l'arrêt propre du host `pid` (ouvre son événement nommé et SetEvent).
    /// Retourne faux si l'événement n'existe pas (host déjà arrêté ou non élevé).
    pub fn signal_stop(pid: u32) -> bool {
        let name = core::stop_event_name(pid);
        let wname = wide(&name);
        // SAFETY : handle fermé avant retour.
        unsafe {
            let h = OpenEventW(EVENT_MODIFY_STATE | SYNCHRONIZATION_SYNCHRONIZE, 0, wname.as_ptr());
            if h.is_null() {
                return false;
            }
            let ok = SetEvent(h) != 0;
            CloseHandle(h);
            ok
        }
    }
}

// ───────────────────────────── schtasks (via la CLI, sans dépendance COM) ─────────────────────

/// Crée (ou remplace) la tâche planifiée. À appeler DÉJÀ ÉLEVÉ (sous-commande interne).
pub fn create_task(at_logon: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    preflight(&exe)?;
    let sid = current_user_sid_or_err()?;
    let spec = core::TaskSpec { exe: exe.to_string_lossy().into_owned(), user_sid: sid, at_logon };
    let xml = core::task_xml(&spec).map_err(anyhow::Error::msg)?;
    // XML en UTF-16LE avec BOM (schtasks l'exige).
    let tmp = std::env::temp_dir().join(format!("grx-task-{}.xml", std::process::id()));
    write_utf16(&tmp, &xml)?;
    let r = run_schtasks(&["/Create", "/TN", core::TASK_NAME, "/XML", &tmp.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&tmp);
    let out = r?;
    if !out.0 {
        bail!("schtasks /Create a échoué : {}", out.1);
    }
    Ok(())
}

/// Supprime la tâche planifiée. À appeler DÉJÀ ÉLEVÉ.
pub fn delete_task() -> Result<()> {
    let (ok, msg) = run_schtasks(&["/Delete", "/TN", core::TASK_NAME, "/F"])?;
    if !ok && !msg.contains("ne peut pas") && !msg.to_lowercase().contains("cannot find") && !msg.contains("introuvable") {
        bail!("schtasks /Delete a échoué : {msg}");
    }
    Ok(())
}

/// Lance la tâche (host élevé) à la demande, sans UAC. Appelable NON élevé (c'est tout l'intérêt).
pub fn run_task() -> Result<()> {
    let (ok, msg) = run_schtasks(&["/Run", "/TN", core::TASK_NAME])?;
    if !ok {
        bail!("schtasks /Run a échoué (la tâche existe-t-elle ? Activez d'abord le Contrôle total) : {msg}");
    }
    Ok(())
}

/// État de la tâche et du processus courant.
pub fn status() -> Status {
    let exe = std::env::current_exe().ok();
    let protected = exe.as_deref().and_then(Path::parent).map(protected_dir).unwrap_or(false);
    let mut st = Status { current_elevated: is_elevated(), protected_location: protected, ..Default::default() };
    if let Ok((true, xml)) = run_schtasks(&["/Query", "/TN", core::TASK_NAME, "/XML"]) {
        let info = core::parse_task_xml(&core::decode_tool_output(xml.as_bytes()));
        st.task_present = true;
        st.at_logon = info.at_logon;
        st.task_command = info.command.clone();
        let cmd_ok = match (&info.command, &exe) {
            (Some(c), Some(e)) => core::is_within_ci(c, &e.to_string_lossy()) || c.eq_ignore_ascii_case(&e.to_string_lossy()),
            _ => false,
        };
        st.task_valid = info.highest && info.arguments.as_deref() == Some(core::TASK_ARGS) && cmd_ok;
    }
    st
}

fn current_user_sid_or_err() -> Result<String> {
    #[cfg(windows)]
    {
        current_user_sid()
    }
    #[cfg(not(windows))]
    {
        bail!("tâche planifiée : Windows uniquement")
    }
}

/// Exécute `schtasks.exe` ; retourne (succès, sortie fusionnée).
fn run_schtasks(args: &[&str]) -> Result<(bool, String)> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("schtasks.exe")
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .context("lancement de schtasks.exe")?;
        let text = format!("{}{}", core::decode_tool_output(&out.stdout), core::decode_tool_output(&out.stderr));
        if args.first() == Some(&"/Query") {
            return Ok((out.status.success(), core::decode_tool_output(&out.stdout)));
        }
        Ok((out.status.success(), text.trim().to_string()))
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        bail!("schtasks : Windows uniquement")
    }
}

fn write_utf16(path: &Path, s: &str) -> Result<()> {
    let mut bytes = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(path, bytes).with_context(|| format!("écriture {}", path.display()))
}

#[cfg(not(windows))]
pub fn current_user_sid() -> Result<String> {
    bail!("Windows uniquement")
}
#[cfg(not(windows))]
pub struct StopEvent;
#[cfg(not(windows))]
impl StopEvent {
    pub fn wait(&self) {}
}
#[cfg(not(windows))]
pub fn create_stop_event() -> Option<StopEvent> {
    None
}
#[cfg(not(windows))]
pub fn signal_stop(_pid: u32) -> bool {
    false
}
#[cfg(not(windows))]
pub fn run_elevated_self(_args: &[&str]) -> Result<i32> {
    bail!("Windows uniquement")
}

/// Options de lancement (`host-options.json`) lues par le host élevé, puis traduites en arguments
/// validés. Erreur = aucune option appliquée (le host élevé démarre avec ses valeurs par défaut).
pub fn read_task_options(dir: &Path) -> Result<Vec<String>> {
    let p = dir.join(core::OPTIONS_FILE);
    let bytes = std::fs::read(&p).with_context(|| format!("lecture {}", p.display()))?;
    let o = core::TaskOptions::parse(&bytes).map_err(anyhow::Error::msg)?;
    Ok(o.args)
}

/// Écrit `host-options.json` (surtout pour les tests ; grx-gui écrit via `grx_core::elevation`).
#[allow(dead_code)]
pub fn write_task_options(dir: &Path, args: &[String]) -> Result<()> {
    core::validate_run_args(args).map_err(anyhow::Error::msg)?;
    let o = core::TaskOptions { args: args.to_vec() };
    let json = serde_json::to_vec_pretty(&o)?;
    std::fs::create_dir_all(dir).ok();
    std::fs::write(dir.join(core::OPTIONS_FILE), json).context("écriture host-options.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_dir_matches_program_files() {
        // Un chemin sous Program Files est protégé ; ailleurs non.
        let pf = program_files_dirs();
        let p = pf[0].join("GamingRemoteExperience");
        assert!(protected_dir(&p));
        assert!(!protected_dir(Path::new(r"C:\Users\x\Downloads\grx")));
        assert!(!protected_dir(Path::new(r"C:\Temp")));
    }

    #[test]
    fn preflight_rejects_user_writable_location() {
        let tmp = std::env::temp_dir().join(format!("grx-pf-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let exe = tmp.join("grx-host.exe");
        let err = preflight(&exe).unwrap_err().to_string();
        assert!(err.contains("dossier modifiable") || err.contains("inscriptible"), "{err}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn preflight_rejects_relative_path() {
        assert!(preflight(Path::new("grx-host.exe")).is_err());
    }

    #[test]
    fn task_options_roundtrip_on_disk() {
        let tmp = std::env::temp_dir().join(format!("grx-opt-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let args = vec!["--bind".to_string(), "0.0.0.0:7777".into(), "--fps".into(), "60".into(), "--fill".into()];
        write_task_options(&tmp, &args).unwrap();
        assert_eq!(read_task_options(&tmp).unwrap(), args);
        // Option interdite refusée à l'écriture.
        assert!(write_task_options(&tmp, &["--data-dir".to_string(), "x".into()]).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

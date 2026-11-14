//! Mode « Contrôle total » : host lancé ÉLEVÉ par une tâche planifiée de l'utilisateur (partie
//! pure, testée sans Windows ; les appels système sont dans `grx-host/src/elevation.rs`).
//!
//! * La tâche [`TASK_NAME`] lance `"<Program Files>\…\grx-host.exe" run --from-task`
//!   (« exécuter avec les autorisations maximales », session interactive de l'utilisateur).
//! * Les options de lancement ne passent PAS par la ligne de commande de la tâche (figée à la
//!   création, sous UAC) : `grx-gui` les écrit dans [`OPTIONS_FILE`] (dossier de données du host),
//!   et le host élevé ne les accepte qu'après [`validate_run_args`] (liste blanche stricte : pas de
//!   `--data-dir`, pas de chemin, pas d'option cachée de test).
//! * Arrêt propre d'un host (élevé ou non) depuis un processus non élevé : événement nommé
//!   [`stop_event_name`] (ACL : utilisateur courant seulement, voir `grx-host`).

/// Nom de la tâche planifiée (dossier racine du Planificateur de tâches).
pub const TASK_NAME: &str = "GamingRemoteExperience Host (élevé)";
/// Options de `grx-host run` pour le host élevé, écrites par grx-gui (JSON [`TaskOptions`]).
pub const OPTIONS_FILE: &str = "host-options.json";
/// Arguments fixes de l'action de la tâche.
pub const TASK_ARGS: &str = "run --from-task";
/// Taille maximale acceptée pour [`OPTIONS_FILE`].
pub const MAX_OPTIONS_BYTES: usize = 4096;

/// Événement nommé d'arrêt propre du host `pid` (espace `Local\` = session courante).
pub fn stop_event_name(pid: u32) -> String {
    format!("Local\\GamingRemoteExperience-Host-Stop-{pid}")
}

/// Contenu de [`OPTIONS_FILE`].
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TaskOptions {
    /// Arguments de `grx-host run` (sans `run`), validés par [`validate_run_args`].
    pub args: Vec<String>,
}

impl TaskOptions {
    /// Lecture stricte : taille bornée, JSON valide, arguments dans la liste blanche.
    pub fn parse(bytes: &[u8]) -> Result<TaskOptions, String> {
        if bytes.len() > MAX_OPTIONS_BYTES {
            return Err(format!("{OPTIONS_FILE} trop gros ({} octets)", bytes.len()));
        }
        let o: TaskOptions = serde_json::from_slice(bytes).map_err(|e| format!("{OPTIONS_FILE} illisible : {e}"))?;
        validate_run_args(&o.args)?;
        Ok(o)
    }
}

/// Options de `grx-host run` acceptées avec une valeur.
const VALUE_FLAGS: &[&str] = &[
    "--bind",
    "--external-port",
    "--upnp-lease",
    "--codec",
    "--fps",
    "--max-bitrate",
    "--start-bitrate",
    "--gamepad-type",
    "--idle-timeout",
    "--display",
];
/// Options de `grx-host run` acceptées sans valeur.
const BOOL_FLAGS: &[&str] = &[
    "--no-upnp",
    "--fill",
    "--no-input",
    "--no-keyboard",
    "--no-mouse",
    "--no-gamepad",
    "--block-win-key",
    "--no-audio",
];
/// Longueur maximale d'une valeur (adresse IPv6 + port tient largement).
const MAX_VALUE_LEN: usize = 64;
const MAX_ARGS: usize = 64;

/// Liste blanche des arguments du host élevé. Refuse tout le reste : `--data-dir` (le host élevé
/// n'écrit que dans le dossier par défaut de l'utilisateur), toute option inconnue ou cachée, la
/// forme `--opt=valeur`, les arguments positionnels, les valeurs vides, trop longues, contenant des
/// caractères de contrôle ou commençant par `-` (pas d'option déguisée en valeur).
pub fn validate_run_args(args: &[String]) -> Result<(), String> {
    if args.len() > MAX_ARGS {
        return Err("trop d'arguments".into());
    }
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if BOOL_FLAGS.contains(&a.as_str()) {
            continue;
        }
        if VALUE_FLAGS.contains(&a.as_str()) {
            let v = it.next().ok_or_else(|| format!("{a} : valeur manquante"))?;
            if v.is_empty() || v.len() > MAX_VALUE_LEN || v.starts_with('-') || v.chars().any(char::is_control) {
                return Err(format!("{a} : valeur refusée"));
            }
            continue;
        }
        return Err(format!("argument refusé pour le host élevé : {}", sanitize(a)));
    }
    Ok(())
}

fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(40).collect()
}

/// Échappement XML (texte et attributs).
pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Paramètres de la tâche.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    /// Chemin ABSOLU de grx-host.exe (dossier protégé, vérifié par l'appelant).
    pub exe: String,
    /// SID de l'utilisateur (principal ET déclencheur d'ouverture de session).
    pub user_sid: String,
    /// Lancer aussi à l'ouverture de session de cet utilisateur.
    pub at_logon: bool,
}

/// Définition XML (schéma 1.2) de la tâche, pour `schtasks /Create /XML`.
///
/// Principal = l'utilisateur, `InteractiveToken` (sa session, sans mot de passe stocké),
/// `HighestAvailable` (élevé), une seule instance, pas de limite de durée, priorité normale (4 ;
/// le défaut 7 du Planificateur est « inférieure à la normale », mauvais pour l'encodage).
pub fn task_xml(t: &TaskSpec) -> Result<String, String> {
    let exe = std::path::Path::new(&t.exe);
    if !exe.is_absolute() || !t.exe.contains(':') && !t.exe.starts_with("\\\\") {
        return Err("chemin de grx-host.exe non absolu".into());
    }
    if !valid_sid(&t.user_sid) {
        return Err("SID utilisateur invalide".into());
    }
    let dir = exe.parent().map(|p| p.to_string_lossy().into_owned()).ok_or("dossier de grx-host.exe introuvable")?;
    let sid = xml_escape(&t.user_sid);
    let trigger = if t.at_logon {
        format!(
            "  <Triggers>\n    <LogonTrigger>\n      <Enabled>true</Enabled>\n      <UserId>{sid}</UserId>\n      <Delay>PT15S</Delay>\n    </LogonTrigger>\n  </Triggers>\n"
        )
    } else {
        String::new()
    };
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n    <Author>GamingRemoteExperience</Author>\n    <Description>Lance grx-host élevé (mode Contrôle total) sans invite UAC. Créée et supprimée par grx-host elevation enable/disable.</Description>\n  </RegistrationInfo>\n\
{trigger}\
  <Principals>\n    <Principal id=\"Author\">\n      <UserId>{sid}</UserId>\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>HighestAvailable</RunLevel>\n    </Principal>\n  </Principals>\n\
  <Settings>\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n    <AllowHardTerminate>true</AllowHardTerminate>\n    <StartWhenAvailable>false</StartWhenAvailable>\n    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n    <IdleSettings>\n      <StopOnIdleEnd>false</StopOnIdleEnd>\n      <RestartOnIdle>false</RestartOnIdle>\n    </IdleSettings>\n    <AllowStartOnDemand>true</AllowStartOnDemand>\n    <Enabled>true</Enabled>\n    <Hidden>false</Hidden>\n    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n    <WakeToRun>false</WakeToRun>\n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n    <Priority>4</Priority>\n  </Settings>\n\
  <Actions Context=\"Author\">\n    <Exec>\n      <Command>{}</Command>\n      <Arguments>{}</Arguments>\n      <WorkingDirectory>{}</WorkingDirectory>\n    </Exec>\n  </Actions>\n\
</Task>\n",
        xml_escape(&t.exe),
        xml_escape(TASK_ARGS),
        xml_escape(&dir),
    ))
}

/// `S-1-5-21-…` : chiffres et tirets seulement.
pub fn valid_sid(s: &str) -> bool {
    s.len() <= 184 && s.starts_with("S-1-") && s[4..].split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Ce qu'on relit d'une tâche existante (`schtasks /Query /XML`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskInfo {
    pub command: Option<String>,
    pub arguments: Option<String>,
    pub at_logon: bool,
    pub highest: bool,
    pub user_id: Option<String>,
}

fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    Some(xml[start..end].trim())
}

/// Lecture tolérante de la définition XML d'une tâche (sous-ensemble utile au statut).
pub fn parse_task_xml(xml: &str) -> TaskInfo {
    TaskInfo {
        command: tag(xml, "Command").map(xml_unescape),
        arguments: tag(xml, "Arguments").map(xml_unescape),
        at_logon: xml.contains("<LogonTrigger"),
        highest: tag(xml, "RunLevel") == Some("HighestAvailable"),
        user_id: tag(xml, "Principals").and_then(|p| tag(p, "UserId")).map(xml_unescape),
    }
}

/// Sortie de `schtasks /XML` : UTF-16LE (avec ou sans BOM) ou UTF-8.
pub fn decode_tool_output(b: &[u8]) -> String {
    let utf16 = b.starts_with(&[0xFF, 0xFE]) || (b.len() >= 4 && b[1] == 0 && b[3] == 0);
    if utf16 {
        let start = if b.starts_with(&[0xFF, 0xFE]) { 2 } else { 0 };
        let u: Vec<u16> = b[start..].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u)
    } else {
        String::from_utf8_lossy(b.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(b)).into_owned()
    }
}

/// `path` est-il dans `root` (comparaison par composants, insensible à la casse, `\\?\` ignoré) ?
pub fn is_within_ci(path: &str, root: &str) -> bool {
    fn comps(s: &str) -> Vec<String> {
        let s = s.strip_prefix(r"\\?\").unwrap_or(s);
        s.split(['\\', '/']).filter(|c| !c.is_empty() && *c != ".").map(|c| c.to_lowercase()).collect()
    }
    let (p, r) = (comps(path), comps(root));
    if p.iter().any(|c| c == "..") || r.is_empty() {
        return false;
    }
    p.len() > r.len() && p[..r.len()] == r[..]
}

#[cfg(test)]
#[path = "elevation_tests.rs"]
mod tests;

//! Pare-feu Windows : vérification EN LECTURE SEULE (objet COM HNetCfg.FwPolicy2 via PowerShell)
//! d'une règle autorisant grx-host en entrée UDP, et commande exacte à lancer en administrateur.
//! grx-host ne modifie jamais le pare-feu lui-même.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FwRule {
    pub name: String,
    pub app: String,
    /// 17 = UDP, 256 = tous.
    pub protocol: i32,
    pub local_ports: String,
    /// 1 = autoriser, 0 = bloquer.
    pub action: i32,
    /// Masque : 1 domaine, 2 privé, 4 public (0x7FFFFFFF = tous).
    pub profiles: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allowed(String),
    Blocked(Vec<String>),
    WrongProfile(Vec<String>),
    Missing,
    Unknown(String),
}

impl Verdict {
    pub fn state(&self) -> &'static str {
        match self {
            Verdict::Allowed(_) => "ok",
            Verdict::Blocked(_) => "blocked",
            Verdict::WrongProfile(_) => "wrong_profile",
            Verdict::Missing => "missing",
            Verdict::Unknown(_) => "unknown",
        }
    }

    pub fn detail(&self) -> String {
        match self {
            Verdict::Allowed(why) => why.clone(),
            Verdict::Blocked(r) => format!("règle(s) de BLOCAGE pour grx-host : {}", r.join(", ")),
            Verdict::WrongProfile(r) => {
                format!("règle(s) {} présente(s) mais pas pour le profil réseau actuel (public/privé)", r.join(", "))
            }
            Verdict::Missing => "aucune règle n'autorise grx-host en entrée UDP".into(),
            Verdict::Unknown(e) => format!("vérification impossible : {e}"),
        }
    }
}

fn port_matches(spec: &str, port: u16) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec == "*" {
        return true;
    }
    spec.split(',').any(|p| {
        let p = p.trim();
        match p.split_once('-') {
            Some((a, b)) => match (a.trim().parse::<u16>(), b.trim().parse::<u16>()) {
                (Ok(a), Ok(b)) => (a..=b).contains(&port),
                _ => false,
            },
            None => p.parse::<u16>() == Ok(port),
        }
    })
}

/// Analyse pure : `current` = profils actifs (masque), `fw_on` = pare-feu actif sur ces profils.
pub fn analyze(rules: &[FwRule], current: i32, fw_on: bool, exe: &Path, port: u16) -> Verdict {
    if !fw_on {
        return Verdict::Allowed("pare-feu Windows désactivé pour le réseau actuel".into());
    }
    let exe = exe.to_string_lossy().to_lowercase();
    let relevant: Vec<&FwRule> = rules
        .iter()
        .filter(|r| (r.protocol == 17 || r.protocol == 256) && port_matches(&r.local_ports, port))
        .filter(|r| {
            let app = r.app.trim().to_lowercase();
            if app.is_empty() {
                // Règle par port (sans programme) : seulement si elle cite explicitement le port.
                r.local_ports.trim() != "*" && !r.local_ports.trim().is_empty()
            } else {
                app == exe
            }
        })
        .collect();
    let covers = |r: &FwRule| current != 0 && (r.profiles & current) == current;
    let blocked: Vec<String> = relevant.iter().filter(|r| r.action == 0 && (r.profiles & current) != 0).map(|r| r.name.clone()).collect();
    if !blocked.is_empty() {
        return Verdict::Blocked(blocked);
    }
    if let Some(r) = relevant.iter().find(|r| r.action == 1 && covers(r)) {
        return Verdict::Allowed(format!("règle « {} »", r.name));
    }
    let partial: Vec<String> = relevant.iter().filter(|r| r.action == 1).map(|r| r.name.clone()).collect();
    if !partial.is_empty() {
        return Verdict::WrongProfile(partial);
    }
    Verdict::Missing
}

/// Sortie du script : `PROFILES\t<masque>`, `FWON\t<0|1>`, `RULE\t<nom>\t<app>\t<proto>\t<ports>\t<action>\t<profils>`.
pub fn parse_output(out: &str) -> (Vec<FwRule>, i32, bool) {
    let mut rules = Vec::new();
    let mut current = 0;
    let mut on = false;
    for line in out.lines() {
        let f: Vec<&str> = line.trim_end_matches('\r').split('\t').collect();
        match f.first().copied() {
            Some("PROFILES") => current = f.get(1).and_then(|v| v.trim().parse().ok()).unwrap_or(0),
            Some("FWON") => on |= f.get(1).map(|v| v.trim() == "1").unwrap_or(false),
            Some("RULE") if f.len() >= 7 => rules.push(FwRule {
                name: f[1].to_string(),
                app: f[2].to_string(),
                protocol: f[3].trim().parse().unwrap_or(-1),
                local_ports: f[4].to_string(),
                action: f[5].trim().parse().unwrap_or(-1),
                profiles: f[6].trim().parse().unwrap_or(0),
            }),
            _ => {}
        }
    }
    (rules, current, on)
}

const SCRIPT: &str = r#"
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$p = New-Object -ComObject HNetCfg.FwPolicy2
$cp = [int]$p.CurrentProfileTypes
"PROFILES`t$cp"
foreach ($t in 1,2,4) { if ($cp -band $t) { if ($p.FirewallEnabled($t)) { "FWON`t1" } else { "FWON`t0" } } }
foreach ($r in $p.Rules) {
  if ($r.Direction -eq 1 -and $r.Enabled -and ($r.Protocol -eq 17 -or $r.Protocol -eq 256)) {
    $app = [Environment]::ExpandEnvironmentVariables([string]$r.ApplicationName)
    "RULE`t$($r.Name)`t$app`t$($r.Protocol)`t$($r.LocalPorts)`t$($r.Action)`t$($r.Profiles)"
  }
}
"#;

/// Vérifie le pare-feu (lecture seule, ~0,5-2 s). À appeler hors de la boucle réseau.
#[cfg(windows)]
pub fn check(exe: &Path, port: u16) -> Verdict {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", SCRIPT])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let (rules, current, on) = parse_output(&String::from_utf8_lossy(&o.stdout));
            if current == 0 {
                return Verdict::Unknown("profil réseau inconnu".into());
            }
            analyze(&rules, current, on, exe, port)
        }
        Ok(o) => Verdict::Unknown(String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("échec de PowerShell").to_string()),
        Err(e) => Verdict::Unknown(format!("PowerShell : {e}")),
    }
}

#[cfg(not(windows))]
pub fn check(_exe: &Path, _port: u16) -> Verdict {
    Verdict::Unknown("vérification disponible sous Windows uniquement".into())
}

pub fn rule_name(port: u16) -> String {
    format!("GamingRemoteExperience host (UDP {port})")
}

/// Commandes à lancer EN ADMINISTRATEUR pour autoriser grx-host (UDP `port`) en entrée.
pub fn help_text(exe: &Path, port: u16) -> String {
    let exe = exe.display();
    let name = rule_name(port);
    format!(
        "Autoriser grx-host en entrée UDP {port} dans le pare-feu Windows (grx-host ne le fait jamais lui-même).\n\
         Dans un PowerShell lancé EN ADMINISTRATEUR :\n\n  \
         New-NetFirewallRule -DisplayName \"{name}\" -Direction Inbound -Action Allow -Protocol UDP -LocalPort {port} -Program \"{exe}\" -Profile Any\n\n\
         ou dans une invite de commandes ADMINISTRATEUR :\n\n  \
         netsh advfirewall firewall add rule name=\"{name}\" dir=in action=allow protocol=UDP localport={port} program=\"{exe}\" profile=any\n\n\
         Si une règle BLOQUE grx-host (créée en refusant la fenêtre « Autoriser l'accès ») :\n\n  \
         Get-NetFirewallApplicationFilter -Program \"{exe}\" | Get-NetFirewallRule | Where-Object Action -eq Block | Remove-NetFirewallRule\n\n\
         Pour retirer la règle plus tard :  Remove-NetFirewallRule -DisplayName \"{name}\"\n\
         (Valable en IPv4 et en IPv6 ; si l'exécutable est déplacé, recréez la règle.)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule(name: &str, app: &str, proto: i32, ports: &str, action: i32, profiles: i32) -> FwRule {
        FwRule { name: name.into(), app: app.into(), protocol: proto, local_ports: ports.into(), action, profiles }
    }

    #[test]
    fn verdicts() {
        let exe = PathBuf::from(r"C:\grx\grx-host.exe");
        let app = r"c:\GRX\grx-host.exe"; // casse différente
        assert_eq!(analyze(&[], 2, true, &exe, 7777), Verdict::Missing);
        assert_eq!(analyze(&[], 2, false, &exe, 7777).state(), "ok");
        // Règle créée par la fenêtre Windows : programme, UDP, tous ports, profil privé.
        let popup = rule("grx-host.exe", app, 17, "*", 1, 2);
        assert_eq!(analyze(std::slice::from_ref(&popup), 2, true, &exe, 7777).state(), "ok");
        assert_eq!(analyze(std::slice::from_ref(&popup), 4, true, &exe, 7777).state(), "wrong_profile");
        // Notre règle conseillée.
        let ours = rule("GRX", app, 17, "7777", 1, 0x7FFF_FFFF);
        assert_eq!(analyze(std::slice::from_ref(&ours), 4, true, &exe, 7777).state(), "ok");
        assert_eq!(analyze(std::slice::from_ref(&ours), 4, true, &exe, 7778), Verdict::Missing);
        // Un blocage l'emporte.
        let block = rule("grx-host.exe", app, 17, "*", 0, 4);
        assert_eq!(analyze(&[ours.clone(), block], 4, true, &exe, 7777).state(), "blocked");
        // Règle par port sans programme ; une règle « tous ports, tous programmes » ne compte pas.
        assert_eq!(analyze(&[rule("p", "", 256, "7000-8000", 1, 7)], 2, true, &exe, 7777).state(), "ok");
        assert_eq!(analyze(&[rule("any", "", 17, "*", 1, 7)], 2, true, &exe, 7777), Verdict::Missing);
        // Autre programme, TCP : ignorés.
        assert_eq!(analyze(&[rule("x", r"C:\autre.exe", 17, "*", 1, 7), rule("t", app, 6, "*", 1, 7)], 2, true, &exe, 7777), Verdict::Missing);
    }

    #[test]
    fn ports_and_parsing() {
        assert!(port_matches("*", 1));
        assert!(port_matches("80,7777", 7777));
        assert!(port_matches("7000-8000", 7777));
        assert!(!port_matches("RPC", 7777));
        let out = "PROFILES\t2\r\nFWON\t1\r\nRULE\tgrx\tC:\\g\\grx-host.exe\t17\t*\t1\t2\r\nRULE\tbad\n";
        let (rules, cp, on) = parse_output(out);
        assert_eq!((cp, on, rules.len()), (2, true, 1));
        assert_eq!(rules[0].app, r"C:\g\grx-host.exe");
        assert!(help_text(Path::new(r"C:\g\grx-host.exe"), 7777).contains("localport=7777 program=\"C:\\g\\grx-host.exe\""));
    }
}

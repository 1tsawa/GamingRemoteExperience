//! Politique « bureau sécurisé / fenêtre administrateur » du host (pure, testée sans Windows).
//!
//! Le host observe (thread `grx-secure`, `grx-host/src/secure.rs`, lecture seule) :
//! * le **bureau d'entrée** : `Default` (normal) ou autre (`Winlogon` = invite UAC,
//!   Ctrl+Alt+Suppr, ouverture de session ; inaccessible = considéré sécurisé) ;
//! * l'**intégrité de la fenêtre au premier plan**, comparée à celle du host : un processus
//!   d'intégrité supérieure (administrateur, alors que le host ne l'est pas) ne reçoit pas les
//!   entrées injectées (UIPI), la souris du client semblerait « figée ».
//!
//! Il décide, pour le client de la session, ce qui est permis ([`access`]), puis le lui signale
//! ([`notice`] → message `GRX-SEC`, [`crate::secure_notice`]) pour que l'overlay l'explique.
//!
//! Le bureau sécurisé n'est **jamais** capturé ni piloté, pour aucun rôle : un host non-SYSTEM
//! ne le peut pas, et c'est voulu (une invite UAC se valide sur place).

pub use crate::secure_notice::{BlockReason, HostScreen, SecureNotice};
use crate::trust::Role;

/// Ce que le host observe.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct HostSituation {
    /// Bureau d'entrée autre que `Default` (UAC, Ctrl+Alt+Suppr…) ou inaccessible.
    pub secure_desktop: bool,
    /// Fenêtre au premier plan d'intégrité « élevée » ou plus (processus administrateur), ou
    /// d'intégrité illisible (prudence).
    pub elevated_foreground: bool,
    /// Le host lui-même tourne élevé (mode « Contrôle total », lancé par sa tâche planifiée).
    pub host_elevated: bool,
    /// La fenêtre au premier plan a une intégrité STRICTEMENT supérieure à celle du host :
    /// Windows (UIPI) rejetterait les entrées injectées, quel que soit le rôle.
    pub foreground_above_host: bool,
}

/// Ce que le client de la session peut faire maintenant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// Recevoir les images capturées.
    pub video: bool,
    /// Injecter clavier et souris.
    pub keyboard_mouse: bool,
    /// Manettes virtuelles (ViGEm : pilote, non concerné par l'UIPI).
    pub gamepad: bool,
}

/// Droits du client selon son rôle et la situation du host.
///
/// * Bureau sécurisé : rien (ni image, ni entrée, ni manette), pour tout le monde.
/// * Fenêtre élevée au premier plan : image permise, manette permise ; clavier/souris seulement
///   pour le rôle `Total` ET si le host peut l'atteindre (host élevé, intégrité ≥ fenêtre).
/// * Sinon : comportement historique (`ViewOnly` voit, `Full`/`Total` injectent).
pub fn access(role: Role, s: &HostSituation) -> Access {
    if s.secure_desktop {
        return Access { video: false, keyboard_mouse: false, gamepad: false };
    }
    let inject = role.can_inject();
    let km = inject && !s.foreground_above_host && (!s.elevated_foreground || role == Role::Total);
    Access { video: true, keyboard_mouse: km, gamepad: inject }
}

/// Message à envoyer au client (`screen == Normal` : rien de particulier).
pub fn notice(role: Role, s: &HostSituation) -> SecureNotice {
    let a = access(role, s);
    let screen = if s.secure_desktop {
        HostScreen::SecureDesktop
    } else if s.elevated_foreground || s.foreground_above_host {
        HostScreen::ElevatedWindow
    } else {
        HostScreen::Normal
    };
    // Un spectateur n'injecte jamais : on ne lui signale pas un blocage d'entrées.
    let input_blocked = role.can_inject() && !a.keyboard_mouse;
    let video_blocked = !a.video;
    let reason = if !(input_blocked || video_blocked) {
        BlockReason::None
    } else if s.secure_desktop || s.foreground_above_host {
        BlockReason::HostMode
    } else {
        BlockReason::Role
    };
    SecureNotice { screen, video_blocked, input_blocked, host_elevated: s.host_elevated, reason }
}

/// Texte du bandeau de l'overlay du client, `None` = rien à afficher (situation normale, ou
/// rien n'est bloqué pour ce client).
pub fn notice_text(n: &SecureNotice) -> Option<String> {
    if n.screen == HostScreen::Normal || !(n.video_blocked || n.input_blocked) {
        return None;
    }
    Some(match (n.screen, n.reason) {
        (HostScreen::SecureDesktop, _) => {
            "Écran sécurisé Windows (UAC / verrouillage) sur le host — à valider sur place".to_string()
        }
        (_, BlockReason::Role) => {
            "Fenêtre administrateur au premier plan sur le host : rôle « Contrôle total » requis (clavier/souris bloqués)"
                .to_string()
        }
        _ if n.host_elevated => {
            "Fenêtre système au premier plan sur le host : clavier/souris inopérants".to_string()
        }
        _ => "Fenêtre administrateur au premier plan sur le host : clavier/souris inopérants (activez « Contrôle total » sur le host)"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROLES: [Role; 3] = [Role::ViewOnly, Role::Full, Role::Total];

    /// Situation cohérente : `elevated` = fenêtre administrateur au premier plan.
    fn sit(secure: bool, elevated: bool, host_elevated: bool) -> HostSituation {
        HostSituation {
            secure_desktop: secure,
            elevated_foreground: elevated,
            host_elevated,
            foreground_above_host: elevated && !host_elevated,
        }
    }

    #[test]
    fn normal_desktop_keeps_historic_behaviour() {
        for he in [false, true] {
            let s = sit(false, false, he);
            assert_eq!(access(Role::ViewOnly, &s), Access { video: true, keyboard_mouse: false, gamepad: false });
            assert_eq!(access(Role::Full, &s), Access { video: true, keyboard_mouse: true, gamepad: true });
            assert_eq!(access(Role::Total, &s), Access { video: true, keyboard_mouse: true, gamepad: true });
            for r in ROLES {
                assert_eq!(notice(r, &s), SecureNotice { host_elevated: he, ..Default::default() });
                assert!(notice_text(&notice(r, &s)).is_none());
            }
        }
    }

    #[test]
    fn secure_desktop_is_never_reachable() {
        for elevated in [false, true] {
            for he in [false, true] {
                for r in ROLES {
                    let s = sit(true, elevated, he);
                    assert_eq!(access(r, &s), Access { video: false, keyboard_mouse: false, gamepad: false });
                    let n = notice(r, &s);
                    assert_eq!(n.screen, HostScreen::SecureDesktop);
                    assert!(n.video_blocked);
                    assert_eq!(n.input_blocked, r.can_inject());
                    assert_eq!(n.reason, BlockReason::HostMode);
                    assert!(notice_text(&n).unwrap().contains("à valider sur place"));
                }
            }
        }
    }

    #[test]
    fn elevated_window_needs_total_role_and_elevated_host() {
        // Host non élevé : personne ne pilote la fenêtre administrateur (UIPI).
        let s = sit(false, true, false);
        for r in ROLES {
            assert!(!access(r, &s).keyboard_mouse, "{r:?}");
            assert!(access(r, &s).video);
        }
        assert!(access(Role::Full, &s).gamepad && access(Role::Total, &s).gamepad);
        // Host élevé : seul « Contrôle total ».
        let s = sit(false, true, true);
        assert!(access(Role::Total, &s).keyboard_mouse);
        assert!(!access(Role::Full, &s).keyboard_mouse && access(Role::Full, &s).gamepad);
        assert!(!access(Role::ViewOnly, &s).keyboard_mouse && !access(Role::ViewOnly, &s).gamepad);
    }

    #[test]
    fn window_above_an_elevated_host_blocks_everyone() {
        // Ex. fenêtre d'un processus SYSTEM au premier plan alors que le host est « seulement » élevé.
        let s = HostSituation { secure_desktop: false, elevated_foreground: true, host_elevated: true, foreground_above_host: true };
        for r in ROLES {
            assert!(!access(r, &s).keyboard_mouse);
        }
        let n = notice(Role::Total, &s);
        assert_eq!((n.input_blocked, n.reason), (true, BlockReason::HostMode));
        assert!(notice_text(&n).unwrap().contains("système"));
    }

    #[test]
    fn notices_explain_the_reason() {
        // Host non élevé : il faut activer le mode sur le host.
        let n = notice(Role::Total, &sit(false, true, false));
        assert_eq!((n.screen, n.video_blocked, n.input_blocked, n.reason), (HostScreen::ElevatedWindow, false, true, BlockReason::HostMode));
        assert!(notice_text(&n).unwrap().contains("activez « Contrôle total »"));
        // Host élevé, client « Complet » : rôle insuffisant.
        let n = notice(Role::Full, &sit(false, true, true));
        assert_eq!((n.screen, n.video_blocked, n.input_blocked, n.reason), (HostScreen::ElevatedWindow, false, true, BlockReason::Role));
        assert!(notice_text(&n).unwrap().contains("rôle « Contrôle total » requis"));
        // Contrôle total effectif : rien à afficher.
        let n = notice(Role::Total, &sit(false, true, true));
        assert!(!n.video_blocked && !n.input_blocked && n.reason == BlockReason::None && n.host_elevated);
        assert!(notice_text(&n).is_none());
        // Spectateur devant une fenêtre élevée : il voit, rien n'est « bloqué » pour lui.
        let n = notice(Role::ViewOnly, &sit(false, true, false));
        assert!(!n.input_blocked && !n.video_blocked && n.reason == BlockReason::None);
        assert!(notice_text(&n).is_none());
    }

    #[test]
    fn every_notice_roundtrips_on_the_wire() {
        for r in ROLES {
            for bits in 0..16u8 {
                let s = HostSituation {
                    secure_desktop: bits & 1 != 0,
                    elevated_foreground: bits & 2 != 0,
                    host_elevated: bits & 4 != 0,
                    foreground_above_host: bits & 8 != 0,
                };
                let n = notice(r, &s);
                assert_eq!(SecureNotice::decode(&n.encode()), Some(n));
                // Texte présent ⇔ quelque chose est bloqué hors situation normale.
                assert_eq!(notice_text(&n).is_some(), n.screen != HostScreen::Normal && (n.video_blocked || n.input_blocked));
            }
        }
    }
}

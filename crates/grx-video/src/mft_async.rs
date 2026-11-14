//! Modèle événementiel des MFT asynchrones (encodeurs matériels NVIDIA/AMD/Intel, décodeurs
//! Intel Quick Sync) : le MFT annonce `METransformNeedInput` (il accepte une entrée) et
//! `METransformHaveOutput` (une sortie est prête) ; l'appelant ne doit appeler `ProcessInput`
//! qu'après un `NeedInput` et `ProcessOutput` qu'après un `HaveOutput`.
//!
//! [`pump`] fait tourner cette machine en mode synchrone pour l'appelant (attente active courte
//! puis sommeils de 1 ms) jusqu'à une condition ; la logique est séparée de COM pour être testée
//! avec un faux MFT ([`AsyncMft`]).

use std::time::{Duration, Instant};

/// Événement d'un MFT asynchrone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MftEvent {
    NeedInput,
    HaveOutput,
    /// `MEError` avec son HRESULT.
    Error(i32),
    /// Autre événement (ignoré).
    Other,
}

/// Ce que [`pump`] sait faire d'un MFT asynchrone.
pub trait AsyncMft {
    /// Prochain événement sans attendre (`Ok(None)` : file vide).
    fn next_event(&mut self) -> Result<Option<MftEvent>, String>;
    /// Un `NeedInput` est arrivé (le compteur d'entrées acceptées augmente).
    fn on_need_input(&mut self);
    /// Un `HaveOutput` est arrivé : récupérer la sortie (`ProcessOutput`).
    fn on_have_output(&mut self) -> Result<(), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpError {
    Timeout,
    /// `MEError` reçu.
    MftError(i32),
    /// Échec de `GetEvent` / `ProcessOutput`.
    Failed(String),
}

impl std::fmt::Display for PumpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PumpError::Timeout => write!(f, "délai dépassé en attendant le MFT"),
            PumpError::MftError(hr) => write!(f, "erreur du MFT (MEError {hr:#010x})"),
            PumpError::Failed(e) => write!(f, "{e}"),
        }
    }
}

/// Traite les événements de `t` jusqu'à ce que `cond(t)` soit vraie (vérifiée avant chaque
/// événement) ou que `timeout` expire.
pub fn pump<T: AsyncMft>(t: &mut T, timeout: Duration, cond: impl Fn(&T) -> bool) -> Result<(), PumpError> {
    let t0 = Instant::now();
    loop {
        if cond(t) {
            return Ok(());
        }
        match t.next_event().map_err(PumpError::Failed)? {
            Some(MftEvent::NeedInput) => t.on_need_input(),
            Some(MftEvent::HaveOutput) => t.on_have_output().map_err(PumpError::Failed)?,
            Some(MftEvent::Error(hr)) => return Err(PumpError::MftError(hr)),
            Some(MftEvent::Other) => {}
            None => {
                let el = t0.elapsed();
                if el > timeout {
                    return Err(PumpError::Timeout);
                }
                // Attente active courte (latence minimale) puis 1 ms (timeBeginPeriod(1)).
                if el < Duration::from_millis(3) {
                    std::hint::spin_loop();
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "mft_async_tests.rs"]
mod tests;

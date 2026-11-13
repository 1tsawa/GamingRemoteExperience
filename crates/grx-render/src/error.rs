use grx_video::Codec;

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("Windows API error in {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("no hardware-capable decoder found for {0:?} (see crate docs: install the HEVC Video Extensions for H.265)")]
    NoDecoder(Codec),
    #[error("decoder produced no frame within the timeout")]
    Timeout,
    #[error("window/event loop error: {0}")]
    Window(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RenderError>;

/// Ajoute un contexte lisible à une erreur `windows::core::Error`.
pub(crate) trait WinCtx<T> {
    fn ctx(self, context: &'static str) -> Result<T>;
}

impl<T> WinCtx<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T> {
        self.map_err(|source| RenderError::Win { context, source })
    }
}

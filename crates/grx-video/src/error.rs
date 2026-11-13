use crate::Codec;

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("Windows API error in {context}: {source}")]
    Win {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
    #[error("no hardware encoder found for {0:?}")]
    NoEncoder(Codec),
    #[error("no suitable display adapter/output found")]
    NoOutput,
    #[error("timeout: {0}")]
    Timeout(&'static str),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, VideoError>;

/// Ajoute un contexte lisible à une erreur `windows::core::Error`.
pub(crate) trait WinCtx<T> {
    fn ctx(self, context: &'static str) -> Result<T>;
}

impl<T> WinCtx<T> for windows::core::Result<T> {
    fn ctx(self, context: &'static str) -> Result<T> {
        self.map_err(|source| VideoError::Win { context, source })
    }
}

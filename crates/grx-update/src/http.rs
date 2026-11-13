//! Accès HTTP(S) : trait [`Fetcher`] (remplaçable dans les tests) et implémentation `ureq`.

use std::io::Read;
use std::time::Duration;

use crate::config::USER_AGENT;
use crate::UpdateError;

pub trait Fetcher: Send + Sync {
    /// Télécharge entièrement une ressource de taille bornée.
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, UpdateError>;
    /// Ouvre un flux (gros fichier) ; la borne de taille est appliquée par l'appelant.
    fn open(&self, url: &str) -> Result<Box<dyn Read + Send>, UpdateError>;
}

/// `https://…` toujours ; `http://` seulement vers la boucle locale et si `allow_loopback_http`.
pub fn check_url(url: &str, allow_loopback_http: bool) -> Result<(), UpdateError> {
    let insecure = || UpdateError::InsecureUrl(url.chars().take(120).collect());
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(insecure());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        return if rest.is_empty() || rest.starts_with('/') { Err(insecure()) } else { Ok(()) };
    }
    if let Some(rest) = url.strip_prefix("http://") {
        if !allow_loopback_http {
            return Err(insecure());
        }
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return Err(insecure());
        }
        let host = if let Some(h) = authority.strip_prefix('[') {
            h.split(']').next().unwrap_or("")
        } else {
            authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
        };
        return if matches!(host, "127.0.0.1" | "localhost" | "::1") { Ok(()) } else { Err(insecure()) };
    }
    Err(insecure())
}

pub struct UreqFetcher {
    agent: ureq::Agent,
    allow_loopback_http: bool,
}

impl UreqFetcher {
    pub fn new(allow_loopback_http: bool) -> Self {
        let agent = ureq::AgentBuilder::new()
            .user_agent(USER_AGENT)
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .redirects(5)
            .https_only(!allow_loopback_http)
            .build();
        Self { agent, allow_loopback_http }
    }

    fn call(&self, url: &str) -> Result<ureq::Response, UpdateError> {
        check_url(url, self.allow_loopback_http)?;
        let req = self.agent.get(url).set("Accept", "application/vnd.github+json, application/octet-stream;q=0.9");
        match req.call() {
            Ok(r) => Ok(r),
            Err(ureq::Error::Status(code, _)) => Err(UpdateError::HttpStatus(code)),
            // Le détail ureq peut contenir l'URL (pas de secret ici), on le garde court.
            Err(e) => Err(UpdateError::Network(e.kind().to_string())),
        }
    }
}

impl Fetcher for UreqFetcher {
    fn get(&self, url: &str, max_bytes: u64) -> Result<Vec<u8>, UpdateError> {
        let resp = self.call(url)?;
        let mut out = Vec::new();
        resp.into_reader()
            .take(max_bytes + 1)
            .read_to_end(&mut out)
            .map_err(|e| UpdateError::Network(e.kind().to_string()))?;
        if out.len() as u64 > max_bytes {
            return Err(UpdateError::TooLarge(max_bytes));
        }
        Ok(out)
    }

    fn open(&self, url: &str) -> Result<Box<dyn Read + Send>, UpdateError> {
        Ok(self.call(url)?.into_reader())
    }
}

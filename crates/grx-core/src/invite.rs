use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::identity::PublicKey;
use crate::util::{b32_decode, b32_encode};
use crate::{Error, Result};

/// Jeton secret à usage unique, 128 bits.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(pub [u8; 16]);

impl Drop for Token {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

impl Token {
    pub fn random() -> Self {
        let mut t = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut t);
        Token(t)
    }

    /// Seul le hash est stocké côté host : une fuite du fichier ne révèle pas de jeton valide.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }

    pub fn to_b32(&self) -> String {
        b32_encode(&self.0)
    }

    pub fn from_b32(s: &str) -> Option<Self> {
        b32_decode(s)?.try_into().ok().map(Token)
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

/// `grx://<ip:port>/<clé publique du host>[/<jeton>]`
///
/// La clé du host est "épinglée" dans le lien : c'est ce qui élimine l'attaque
/// de l'homme du milieu sans aucune autorité centrale.
#[derive(Debug, Clone)]
pub struct Invite {
    pub addr: String,
    pub host_key: PublicKey,
    pub token: Option<Token>,
}

impl Invite {
    pub fn to_uri(&self) -> String {
        let mut s = format!("grx://{}/{}", self.addr, b32_encode(&self.host_key));
        if let Some(t) = &self.token {
            s.push('/');
            s.push_str(&t.to_b32());
        }
        s
    }

    pub fn parse(uri: &str) -> Result<Self> {
        let bad = |m: &str| Error::BadInvite(m.to_string());
        let rest = uri.trim().strip_prefix("grx://").ok_or_else(|| bad("préfixe grx:// manquant"))?;
        let mut parts = rest.split('/');
        let addr = parts.next().filter(|a| !a.is_empty()).ok_or_else(|| bad("adresse manquante"))?;
        let host_key = parts
            .next()
            .and_then(b32_decode)
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| bad("clé du host invalide"))?;
        let token = match parts.next() {
            None | Some("") => None,
            Some(t) => Some(Token::from_b32(t).ok_or_else(|| bad("jeton invalide"))?),
        };
        if parts.next().is_some() {
            return Err(bad("segments en trop"));
        }
        Ok(Invite { addr: addr.to_string(), host_key, token })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, ToSocketAddrs};

    /// Lien IPv6 `grx://[2a01:…]:7777/…` : l'adresse entre crochets passe telle quelle et se résout.
    #[test]
    fn ipv6_invite_roundtrip() {
        for addr in ["[2a01:e0a:1:2::1234]:7777", "203.0.113.5:7777", "monpc.duckdns.org:7777"] {
            let inv = Invite { addr: addr.into(), host_key: [9u8; 32], token: Some(Token([3u8; 16])) };
            let uri = inv.to_uri();
            let back = Invite::parse(&uri).unwrap();
            assert_eq!(back.addr, addr);
            assert_eq!(back.host_key, [9u8; 32]);
            assert_eq!(back.token, Some(Token([3u8; 16])));
        }
        let back = Invite::parse("grx://[2a01:e0a::1]:7777/".to_string().as_str());
        assert!(back.is_err(), "clé manquante");
        let sa: SocketAddr = "[2a01:e0a:1:2::1234]:7777".to_socket_addrs().unwrap().next().unwrap();
        assert!(sa.is_ipv6());
        assert_eq!(sa.port(), 7777);
    }
}

//! AES-256-GCM sealing for host-managed credentials bound to profile, name, kind, and token endpoint.

use super::{CREDENTIAL_KEY_NAME, CREDENTIAL_KEY_PROFILE};
use bot_core::{
    auth::{AuthError, CredentialKind},
    id::{ProfileId, SecretName},
    port::SecretStore,
};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use std::fmt;

/// 32-byte AES-256-GCM key loaded from the reserved host secret (`host` / `credential-key`).
pub struct MasterKey([u8; 32]);

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey([redacted])")
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl MasterKey {
    pub fn parse_hex(raw: &str) -> Result<Self, AuthError> {
        let trimmed = raw.trim();
        if trimmed.len() != 64 || !trimmed.is_ascii() {
            return Err(AuthError::Invalid);
        }
        let mut key = [0u8; 32];
        for (i, slot) in key.iter_mut().enumerate() {
            let pair = &trimmed[i * 2..i * 2 + 2];
            *slot = u8::from_str_radix(pair, 16).map_err(|_| AuthError::Invalid)?;
        }
        Ok(Self(key))
    }

    pub async fn load(secrets: &dyn SecretStore) -> Result<Self, AuthError> {
        let handle = secrets
            .for_profile(ProfileId(CREDENTIAL_KEY_PROFILE.to_owned()))
            .await
            .map_err(AuthError::from)?;
        let name = SecretName::parse(CREDENTIAL_KEY_NAME).ok_or(AuthError::Invalid)?;
        let value = handle.resolve(name).await.map_err(AuthError::from)?;
        let text = std::str::from_utf8(&value.0).map_err(|_| AuthError::Invalid)?;
        Self::parse_hex(text)
    }

    /// Encrypts `plaintext` with a fresh 96-bit random nonce and returns `(nonce, ciphertext)`.
    pub fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), AuthError> {
        let rng = SystemRandom::new();
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rng.fill(&mut nonce_bytes)
            .map_err(|_| AuthError::Unavailable)?;
        let unbound = UnboundKey::new(&AES_256_GCM, &self.0).map_err(|_| AuthError::Invalid)?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.to_vec();
        key.seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out)
            .map_err(|_| AuthError::Unavailable)?;
        Ok((nonce_bytes.to_vec(), in_out))
    }

    /// Decrypts `ciphertext` authenticated against `aad` and `nonce`.
    pub fn open(&self, aad: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, AuthError> {
        let nonce_arr: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| AuthError::Invalid)?;
        let unbound = UnboundKey::new(&AES_256_GCM, &self.0).map_err(|_| AuthError::Invalid)?;
        let key = LessSafeKey::new(unbound);
        let mut buf = ciphertext.to_vec();
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_arr),
                Aad::from(aad),
                &mut buf,
            )
            .map_err(|_| AuthError::Invalid)?;
        Ok(plain.to_vec())
    }
}

/// Canonical AAD binding ciphertext to its profile, name, kind, and token endpoint `host:port`.
pub fn aad_bytes(
    profile: &ProfileId,
    name: &SecretName,
    kind: CredentialKind,
    token_endpoint_host: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + profile.0.len() + name.as_str().len());
    out.extend_from_slice(b"bot-cred-v1\0");
    out.extend_from_slice(profile.0.as_bytes());
    out.push(0);
    out.extend_from_slice(name.as_str().as_bytes());
    out.push(0);
    out.extend_from_slice(kind.as_str().as_bytes());
    out.push(0);
    out.extend_from_slice(token_endpoint_host.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> MasterKey {
        MasterKey::parse_hex(&format!("{byte:02x}").repeat(32)).unwrap()
    }

    #[test]
    fn seal_and_open_round_trip_and_reject_any_aad_or_key_tampering() {
        let k1 = key(0xab);
        let k2 = key(0xcd);
        let p = ProfileId("personal".into());
        let n = SecretName::parse("mcp-oauth").unwrap();
        let aad = aad_bytes(&p, &n, CredentialKind::OAuth, "auth.example:443");
        let (nonce, ct) = k1.seal(&aad, b"{\"access_token\":\"tok-1\"}").unwrap();
        assert_eq!(nonce.len(), NONCE_LEN);
        assert_eq!(
            k1.open(&aad, &nonce, &ct).unwrap(),
            b"{\"access_token\":\"tok-1\"}"
        );
        // Wrong key.
        assert_eq!(k2.open(&aad, &nonce, &ct), Err(AuthError::Invalid));
        // Wrong profile, name, kind, or host:port.
        for wrong in [
            aad_bytes(
                &ProfileId("other".into()),
                &n,
                CredentialKind::OAuth,
                "auth.example:443",
            ),
            aad_bytes(
                &p,
                &SecretName::parse("other").unwrap(),
                CredentialKind::OAuth,
                "auth.example:443",
            ),
            aad_bytes(&p, &n, CredentialKind::ServiceAccount, "auth.example:443"),
            aad_bytes(&p, &n, CredentialKind::OAuth, "auth.example:8443"),
            aad_bytes(&p, &n, CredentialKind::OAuth, "evil.example:443"),
        ] {
            assert_eq!(k1.open(&wrong, &nonce, &ct), Err(AuthError::Invalid));
        }
        // Invalid key hex rejected.
        for bad in ["", "abcd", &"zz".repeat(32), &"aa".repeat(31)] {
            assert_eq!(MasterKey::parse_hex(bad).unwrap_err(), AuthError::Invalid);
        }
        assert!(MasterKey::parse_hex(&format!("  {}\n", "0f".repeat(32))).is_ok());
    }
}

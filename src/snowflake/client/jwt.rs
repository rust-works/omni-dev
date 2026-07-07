//! Snowflake key-pair (RS256) JWT generation for non-interactive login.
//!
//! Snowflake authenticates key-pair logins with a short-lived JWT the client
//! signs locally with its RSA private key; the server verifies it against the
//! public key registered via `ALTER USER … SET RSA_PUBLIC_KEY`. The JWT's
//! `iss`/`sub` use Snowflake's normalized identifiers, and `iss` carries the
//! public-key fingerprint `SHA256:<base64(sha256(DER SubjectPublicKeyInfo))>`.
//!
//! Signing uses `aws-lc-rs` (already in the tree via rustls) — deliberately not
//! the `rsa` crate. Only **unencrypted** PKCS#8 keys are supported; an encrypted
//! key (`-----BEGIN ENCRYPTED PRIVATE KEY-----`) is rejected with an actionable
//! error.

use std::time::{SystemTime, UNIX_EPOCH};

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeyPair;
use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::error::{Error, Result};

/// JWT lifetime. Snowflake caps key-pair JWTs at one hour; stay just under so a
/// small clock skew can't push `exp` past the limit.
const JWT_LIFETIME_SECS: u64 = 3540; // 59 minutes

/// Builds a signed RS256 JWT authenticating `user` on `account` with the RSA
/// private key in `private_key_pem` (unencrypted PKCS#8).
///
/// # Errors
///
/// [`Error::Auth`] if the key is encrypted, not valid unencrypted PKCS#8, or
/// signing fails.
pub(crate) fn build_jwt(account: &str, user: &str, private_key_pem: &str) -> Result<String> {
    let key_pair = load_key_pair(private_key_pem)?;
    let fingerprint = public_key_fingerprint(&key_pair)?;

    let qualified = format!(
        "{}.{}",
        normalize_account(account),
        user.trim().to_uppercase()
    );
    let iss = format!("{qualified}.{fingerprint}");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Auth("system clock is before the Unix epoch".into()))?
        .as_secs();

    let header = encode_part(&json!({ "alg": "RS256", "typ": "JWT" }))?;
    let claims = encode_part(&json!({
        "iss": iss,
        "sub": qualified,
        "iat": now,
        "exp": now + JWT_LIFETIME_SECS,
    }))?;

    let signing_input = format!("{header}.{claims}");
    let signature = sign(&key_pair, signing_input.as_bytes())?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

/// Snowflake's account normalization for JWT identifiers, matching the official
/// connectors: a `.global` (organization / global-URL) account keeps the segment
/// before the first `-`; every other form keeps the segment before the first `.`
/// (region/cloud stripped). The result is uppercased. Note this differs from
/// [`SnowflakeClientConfig::api_host`](super::config::SnowflakeClientConfig::api_host),
/// which lowercases and preserves the region.
fn normalize_account(account: &str) -> String {
    let account = account.trim();
    let base = if account.contains(".global") {
        account.split('-').next()
    } else {
        account.split('.').next()
    };
    base.unwrap_or_default().to_uppercase()
}

/// Parses `pem` (unencrypted PKCS#8) into a signing key pair.
fn load_key_pair(pem: &str) -> Result<KeyPair> {
    let der = pem_to_der(pem)?;
    KeyPair::from_pkcs8(&der).map_err(|e| {
        Error::Auth(format!(
            "invalid RSA private key (expected unencrypted PKCS#8): {e}"
        ))
    })
}

/// Extracts the DER payload from a PEM private key, rejecting encrypted keys.
fn pem_to_der(pem: &str) -> Result<Vec<u8>> {
    if pem.contains("ENCRYPTED PRIVATE KEY") {
        return Err(Error::Auth(
            "encrypted private keys are not yet supported; decrypt with \
             `openssl pkcs8 -in key.p8 -out key_unencrypted.p8`"
                .into(),
        ));
    }
    let body: String = pem
        .lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .collect();
    if body.trim().is_empty() {
        return Err(Error::Auth(
            "private key PEM is empty or missing its base64 body".into(),
        ));
    }
    STANDARD
        .decode(body.trim())
        .map_err(|e| Error::Auth(format!("private key PEM body is not valid base64: {e}")))
}

/// `SHA256:<base64(sha256(DER SubjectPublicKeyInfo))>` — the fingerprint format
/// Snowflake stores as `RSA_PUBLIC_KEY_FP`.
fn public_key_fingerprint(key_pair: &KeyPair) -> Result<String> {
    let spki = key_pair
        .public_key()
        .as_der()
        .map_err(|_| Error::Auth("failed to export the RSA public key".into()))?;
    Ok(format!(
        "SHA256:{}",
        STANDARD.encode(Sha256::digest(spki.as_ref()))
    ))
}

/// RS256-signs `msg`, returning the raw signature bytes.
fn sign(key_pair: &KeyPair, msg: &[u8]) -> Result<Vec<u8>> {
    let mut signature = vec![0u8; key_pair.public_modulus_len()];
    key_pair
        .sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), msg, &mut signature)
        .map_err(|_| Error::Auth("failed to sign the key-pair JWT".into()))?;
    Ok(signature)
}

/// base64url-no-pad encoding of a JSON value (a JWT header or claims segment).
fn encode_part(value: &serde_json::Value) -> Result<String> {
    let bytes =
        serde_json::to_vec(value).map_err(|e| Error::Auth(format!("serializing JWT: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};

    /// Generates a fresh RSA-2048 key and returns its PEM plus the key pair, so
    /// tests never embed a private key in source.
    fn generate_key_pem() -> (String, KeyPair) {
        let key_pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = key_pair.as_der().unwrap();
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(der.as_ref())
        );
        (pem, key_pair)
    }

    #[test]
    fn build_jwt_produces_verifiable_claims_and_signature() {
        let (pem, key_pair) = generate_key_pem();
        let jwt = build_jwt("myacct.us-east-1.aws", "svc_user", &pem).unwrap();

        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "compact JWS has three segments");

        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        // Account normalized: region stripped, everything uppercased.
        assert_eq!(claims["sub"], "MYACCT.SVC_USER");
        let iss = claims["iss"].as_str().unwrap();
        assert!(iss.starts_with("MYACCT.SVC_USER.SHA256:"));
        assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());

        // The fingerprint equals sha256(SPKI), computed independently here.
        let spki = key_pair.public_key().as_der().unwrap();
        let expected_fp = format!("SHA256:{}", STANDARD.encode(Sha256::digest(spki.as_ref())));
        assert!(iss.ends_with(&expected_fp));

        // The signature verifies against the public key.
        let signing_input = format!("{}.{}", parts[0], parts[1]);
        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, spki.as_ref())
            .verify(signing_input.as_bytes(), &signature)
            .expect("signature must verify against the registered public key");
    }

    #[test]
    fn build_jwt_rejects_encrypted_keys() {
        let pem =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----";
        let err = build_jwt("acct", "user", pem).unwrap_err();
        assert!(err.to_string().contains("encrypted"));
    }

    #[test]
    fn build_jwt_rejects_garbage_keys() {
        assert!(build_jwt("acct", "user", "not a pem at all").is_err());
        assert!(build_jwt("acct", "user", "").is_err());
    }

    #[test]
    fn normalize_account_strips_region_and_uppercases() {
        assert_eq!(normalize_account("xy12345.us-east-1.aws"), "XY12345");
        // A plain org-account form (no `.global`) partitions on `.`, so the
        // dash-joined name is kept whole.
        assert_eq!(normalize_account("myorg-acct"), "MYORG-ACCT");
        assert_eq!(normalize_account("  Lower  "), "LOWER");
    }

    #[test]
    fn normalize_account_partitions_global_urls_on_the_dash() {
        // The `.global` special case matches the official connectors: partition
        // on the first `-`, not the first `.`.
        assert_eq!(normalize_account("myaccount-myorg.global"), "MYACCOUNT");
        assert_eq!(
            normalize_account("acct123-org456.global.snowflakecomputing.com"),
            "ACCT123"
        );
    }
}

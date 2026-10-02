//! GitHub.com App credentials for the shared synchronous gh boundary.

use std::{io, path::PathBuf, process::Command, sync::Mutex, time::Duration};

use aws_lc_rs::{rand::SystemRandom, rsa::KeyPair, signature::RSA_PKCS1_SHA256};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine as _,
};
use chrono::{DateTime, Utc};
use serde_json::json;

use crate::utils::{
    env::{non_empty_var, EnvSource},
    secret::Secret,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    app: u64,
    installation: u64,
    key_path: PathBuf,
}

fn error(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn resolve(env: &impl EnvSource) -> io::Result<Option<Config>> {
    match non_empty_var(env, "OMNI_DEV_GITHUB_AUTH")
        .as_deref()
        .unwrap_or("pat")
    {
        "pat" => return Ok(None),
        "app" => {}
        _ => return Err(error("OMNI_DEV_GITHUB_AUTH must be pat or app")),
    }
    if non_empty_var(env, "GH_HOST").is_some_and(|host| host != "github.com") {
        return Err(error("GitHub App auth supports GH_HOST=github.com only"));
    }
    let id = |name| {
        non_empty_var(env, name)
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .ok_or_else(|| error(format!("{name} must be a positive numeric ID in App mode")))
    };
    Ok(Some(Config {
        app: id("OMNI_DEV_GITHUB_APP_ID")?,
        installation: id("OMNI_DEV_GITHUB_APP_INSTALLATION_ID")?,
        key_path: non_empty_var(env, "OMNI_DEV_GITHUB_APP_PRIVATE_KEY_PATH")
            .map(PathBuf::from)
            .ok_or_else(|| error("OMNI_DEV_GITHUB_APP_PRIVATE_KEY_PATH is required in App mode"))?,
    }))
}

#[derive(Debug)]
struct Token {
    value: Secret,
    expires: DateTime<Utc>,
}

#[derive(Default)]
struct Cache(Option<(Config, Token)>);

impl Cache {
    fn token(
        &mut self,
        config: &Config,
        now: DateTime<Utc>,
        mint: impl FnOnce() -> io::Result<Token>,
    ) -> io::Result<Secret> {
        if let Some((identity, token)) = &self.0 {
            if identity == config && token.expires > now + chrono::Duration::minutes(5) {
                return Ok(token.value.clone());
            }
        }
        let token = mint()?;
        if token.expires <= Utc::now() + chrono::Duration::minutes(5)
            || token.value.expose_secret().is_empty()
        {
            return Err(error(
                "GitHub returned an empty or near-expired installation token",
            ));
        }
        let value = token.value.clone();
        self.0 = Some((config.clone(), token));
        Ok(value)
    }
}

static CACHE: Mutex<Cache> = Mutex::new(Cache(None));

pub(super) fn configure(
    cmd: &mut Command,
    args: &[String],
    env: &impl EnvSource,
) -> io::Result<()> {
    // Local capability probes must work without credentials or network access.
    if args
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "--help")
    {
        return Ok(());
    }
    let Some(config) = resolve(env)? else {
        return Ok(());
    };
    let value = CACHE
        .lock()
        .map_err(|_| error("GitHub App token cache lock poisoned"))?
        .token(&config, Utc::now(), || mint(&config))?;
    inject(cmd, &value);
    Ok(())
}

fn inject(cmd: &mut Command, token: &Secret) {
    cmd.env("GH_TOKEN", token.expose_secret())
        .env("GITHUB_TOKEN", token.expose_secret());
}

fn jwt(app: u64, pem: &Secret, now: i64) -> io::Result<Secret> {
    let pem = pem.expose_secret().trim();
    let (begin, end, pkcs8) = if pem.starts_with("-----BEGIN RSA PRIVATE KEY-----") {
        (
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----END RSA PRIVATE KEY-----",
            false,
        )
    } else {
        (
            "-----BEGIN PRIVATE KEY-----",
            "-----END PRIVATE KEY-----",
            true,
        )
    };
    let body = pem
        .strip_prefix(begin)
        .and_then(|s| s.strip_suffix(end))
        .ok_or_else(|| error("App key must be unencrypted RSA PKCS#1 or PKCS#8 PEM"))?;
    let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_| error("invalid App private key PEM"))?;
    let key = if pkcs8 {
        KeyPair::from_pkcs8(&der)
    } else {
        KeyPair::from_der(&der)
    }
    .map_err(|_| error("invalid App RSA private key"))?;
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = URL_SAFE_NO_PAD
        .encode(json!({"iss": app.to_string(), "iat": now - 60, "exp": now + 300}).to_string());
    let input = format!("{header}.{claims}");
    let mut signature = vec![0; key.public_modulus_len()];
    key.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        input.as_bytes(),
        &mut signature,
    )
    .map_err(|_| error("failed to sign App JWT"))?;
    Ok(Secret::new(format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    )))
}

fn mint(config: &Config) -> io::Result<Token> {
    let pem = Secret::new(
        std::fs::read_to_string(&config.key_path)
            .map_err(|_| error("cannot read OMNI_DEV_GITHUB_APP_PRIVATE_KEY_PATH"))?,
    );
    let jwt = jwt(config.app, &pem, Utc::now().timestamp())?;
    exchange(
        &format!(
            "https://api.github.com/app/installations/{}/access_tokens",
            config.installation
        ),
        &jwt,
    )
}

fn exchange(url: &str, jwt: &Secret) -> io::Result<Token> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .build()
        .new_agent();
    let mut response = agent.post(url)
        .header("Authorization", format!("Bearer {}", jwt.expose_secret()))
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "omni-dev")
        .header("Content-Type", "application/json")
        .send("{}")
        .map_err(|e| match e {
            ureq::Error::StatusCode(code) => error(format!("GitHub App token exchange failed (HTTP {code}); check App installation and permissions")),
            _ => error("GitHub App token exchange failed (transport error)"),
        })?;
    // Never propagate decoder errors, which could contain credential material.
    let body = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .map_err(|_| error("cannot read GitHub App token response"))?;
    let body: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| error("invalid GitHub App token response"))?;
    let token = body["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| error("missing GitHub App installation token"))?;
    let expires = body["expires_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .ok_or_else(|| error("invalid GitHub App token expiry"))?
        .with_timezone(&Utc);
    Ok(Token {
        value: Secret::new(token),
        expires,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;
    use aws_lc_rs::{
        encoding::AsDer,
        rsa::KeySize,
        signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256},
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    fn env() -> MapEnv {
        MapEnv::new()
            .with("OMNI_DEV_GITHUB_AUTH", "app")
            .with("OMNI_DEV_GITHUB_APP_ID", "123")
            .with("OMNI_DEV_GITHUB_APP_INSTALLATION_ID", "456")
            .with("OMNI_DEV_GITHUB_APP_PRIVATE_KEY_PATH", "/unused/key.pem")
    }

    fn token(value: &str, minutes: i64) -> Token {
        Token {
            value: value.into(),
            expires: Utc::now() + chrono::Duration::minutes(minutes),
        }
    }

    #[test]
    fn config_defaults_to_pat_and_validates_app_fields() {
        assert_eq!(resolve(&MapEnv::new()).unwrap(), None);
        assert_eq!(
            resolve(&env().with("OMNI_DEV_GITHUB_AUTH", "pat")).unwrap(),
            None
        );
        assert_eq!(resolve(&env()).unwrap().unwrap().installation, 456);
        for (name, value) in [
            ("OMNI_DEV_GITHUB_AUTH", "typo"),
            ("OMNI_DEV_GITHUB_APP_ID", "0"),
            ("OMNI_DEV_GITHUB_APP_ID", ""),
            ("OMNI_DEV_GITHUB_APP_INSTALLATION_ID", "../other"),
            ("OMNI_DEV_GITHUB_APP_PRIVATE_KEY_PATH", ""),
            ("GH_HOST", "enterprise.example"),
        ] {
            assert!(resolve(&env().with(name, value)).is_err(), "{name}");
        }
    }

    #[test]
    fn jwt_has_github_claims_and_verifiable_signature() {
        let key = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let pem = Secret::new(format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
            STANDARD.encode(key.as_der().unwrap().as_ref())
        ));
        let jwt = jwt(123, &pem, 1_700_000_000).unwrap();
        let parts: Vec<_> = jwt.expose_secret().split('.').collect();
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(
            claims,
            json!({"iss":"123", "iat":1_699_999_940, "exp":1_700_000_300})
        );
        let header: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "RS256");
        UnparsedPublicKey::new(
            &RSA_PKCS1_2048_8192_SHA256,
            key.public_key().as_der().unwrap().as_ref(),
        )
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(parts[2]).unwrap(),
        )
        .unwrap();
        assert_eq!(format!("{jwt:?}"), "<redacted>");
    }

    #[test]
    fn keys_fail_without_exposing_material() {
        for pem in [
            "SECRET INVALID KEY",
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nSECRET\n-----END ENCRYPTED PRIVATE KEY-----",
        ] {
            let err = jwt(1, &pem.into(), 1_700_000_000).unwrap_err().to_string();
            assert!(!err.contains("SECRET"));
        }
    }

    #[test]
    fn accepts_github_pkcs1_download_format() {
        // Generated exclusively for this test; never registered with an App.
        let pem = include_str!("../../tests/fixtures/github-app/test-key.pem");
        assert!(jwt(123, &pem.into(), 1_700_000_000).is_ok());
    }

    #[test]
    fn cache_reuses_refreshes_and_separates_identities() {
        let config = resolve(&env()).unwrap().unwrap();
        let mut cache = Cache::default();
        let now = Utc::now();
        assert_eq!(
            cache
                .token(&config, now, || Ok(token("first", 60)))
                .unwrap()
                .expose_secret(),
            "first"
        );
        assert_eq!(
            cache
                .token(&config, now, || panic!("must reuse"))
                .unwrap()
                .expose_secret(),
            "first"
        );
        assert_eq!(
            cache
                .token(&config, now + chrono::Duration::minutes(56), || Ok(token(
                    "second", 60
                )))
                .unwrap()
                .expose_secret(),
            "second"
        );
        let other = Config {
            installation: 999,
            ..config.clone()
        };
        assert!(cache
            .token(&other, now, || Err(error("refresh failed")))
            .is_err());
        assert_eq!(
            cache
                .token(&other, now, || Ok(token("other", 60)))
                .unwrap()
                .expose_secret(),
            "other"
        );
        assert!(cache.token(&config, now, || Ok(token("", 60))).is_err());
        assert!(cache.token(&config, now, || Ok(token("short", 2))).is_err());
        assert!(cache
            .token(&other, now + chrono::Duration::hours(2), || Err(error(
                "offline"
            )))
            .is_err());
    }

    #[test]
    fn concurrent_callers_mint_once() {
        let cache = Arc::new(Mutex::new(Cache::default()));
        let calls = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let cache = Arc::clone(&cache);
                let calls = Arc::clone(&calls);
                scope.spawn(move || {
                    cache
                        .lock()
                        .unwrap()
                        .token(&resolve(&env()).unwrap().unwrap(), Utc::now(), || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(token("shared", 60))
                        })
                        .unwrap();
                });
            }
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn child_environment_overrides_both_tokens_and_pat_is_untouched() {
        let mut cmd = Command::new("unused");
        cmd.env("GH_TOKEN", "personal")
            .env("GITHUB_TOKEN", "personal");
        configure(&mut cmd, &["api".into()], &MapEnv::new()).unwrap();
        assert!(cmd
            .get_envs()
            .all(|(_, value)| value.unwrap() == "personal"));
        inject(&mut cmd, &"installation".into());
        assert!(cmd
            .get_envs()
            .all(|(_, value)| value.unwrap() == "installation"));
        configure(
            &mut cmd,
            &["--version".into()],
            &env().with("OMNI_DEV_GITHUB_APP_ID", "bad"),
        )
        .unwrap();
    }

    fn mock(status: u16, body: String) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/app/installations/456/access_tokens",
            listener.local_addr().unwrap()
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 1024];
            loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn exchange_posts_jwt_and_parses_expiry() {
        let expires = Utc::now() + chrono::Duration::hours(1);
        let (url, server) = mock(
            201,
            json!({"token":"installation", "expires_at":expires.to_rfc3339()}).to_string(),
        );
        let token = exchange(&url, &"signed-jwt".into()).unwrap();
        assert_eq!(token.value.expose_secret(), "installation");
        assert_eq!(token.expires, expires);
        let request = server.join().unwrap().to_lowercase();
        assert!(request.starts_with("post /app/installations/456/access_tokens"));
        assert!(request.contains("authorization: bearer signed-jwt"));
        assert!(request.contains("x-github-api-version: 2022-11-28"));
    }

    #[test]
    fn exchange_errors_do_not_expose_response_or_jwt() {
        for (status, body) in [
            (403, "SECRET BODY"),
            (201, "SECRET INVALID JSON"),
            (201, "{\"token\":\"SECRET\",\"expires_at\":\"SECRET\"}"),
        ] {
            let (url, server) = mock(status, body.into());
            let err = exchange(&url, &"SECRET JWT".into())
                .unwrap_err()
                .to_string();
            server.join().unwrap();
            assert!(!err.contains("SECRET"));
        }
    }
}

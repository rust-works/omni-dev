//! The one resolver for secret environment variables and their `_FILE`
//! companions (issue #2006, [ADR-0089](../../docs/adrs/adr-0089.md),
//! [STYLE-0030](../../docs/STYLE_GUIDE.md)).
//!
//! Every secret omni-dev reads from the environment (or the settings.json
//! `env` fallback) is registered in [`SECRET_ENV_VARS`] and read only through
//! [`secret_var`] / [`secret_var_any`]. Each accepts a `<NAME>_FILE` companion
//! naming a file whose contents are the secret — the Docker/Kubernetes secrets
//! convention — so a secret can stay out of `env` listings,
//! `/proc/<pid>/environ`, shell history, child processes, and settings.json.
//!
//! The rules, each pinned by a test below:
//!
//! - Empty values (`NAME=`, `NAME_FILE=`) count as unset, so an exported
//!   variable can still be neutralised for one invocation.
//! - `NAME` and `NAME_FILE` both set **in the same layer** is an error naming
//!   both. Layers (process env, then the active profile's or base settings
//!   `env`) are resolved as pairs through [`EnvSource::var_pair`], so a
//!   `NAME_FILE` in the process env overrides a `NAME` in settings.json.
//! - Aliases keep their existing precedence: the first name whose pair is set
//!   wins, and a conflict is only ever within one name's pair.
//! - The path must be absolute; symlinks are followed and the **target** must
//!   be a regular file. On Unix it must be either owned by the effective uid
//!   and owner-only (`mode & 0o077 == 0`), or owned by root and not writable
//!   by group or other (`mode & 0o022 == 0`) — the shape of Kubernetes and
//!   Docker Swarm secrets. Windows has no check.
//! - Exactly one trailing `\n` or `\r\n` is trimmed; an empty result is an
//!   error, not "unset".
//! - Errors name variables, paths, modes and uids — never the file's bytes.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::utils::env::EnvSource;
use crate::utils::secret::Secret;

/// Suffix of the companion variable naming a file that holds the secret.
pub const FILE_SUFFIX: &str = "_FILE";

/// Every secret environment variable omni-dev reads. Each is read only
/// through this module and so always accepts a `<NAME>_FILE` companion.
///
/// The grep guard in this module's tests fails the build if a secret-shaped
/// variable is read anywhere else, or if a new one is neither listed here nor
/// in [`EXEMPT_SECRET_ENV_VARS`].
pub const SECRET_ENV_VARS: &[&str] = &[
    // AI backends
    "ANTHROPIC_API_KEY",
    "CLAUDE_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "OPENAI_AUTH_TOKEN",
    // Jev
    "TYPESAFE_API_KEY",
    "OMNI_DEV_JEV_API_KEY",
    // Atlassian
    "ATLASSIAN_API_TOKEN",
    // Datadog
    "DATADOG_API_KEY",
    "DATADOG_APP_KEY",
    // Gmail (the client secret is exempt — see EXEMPT_SECRET_ENV_VARS)
    "GMAIL_REFRESH_TOKEN",
    // Drive
    "DRIVE_CLIENT_SECRET",
    "DRIVE_REFRESH_TOKEN",
    // Snowflake
    "SNOWFLAKE_TOKEN",
    "SNOWFLAKE_PRIVATE_KEY",
    // Browser bridge
    "OMNI_BRIDGE_TOKEN",
];

/// Secret-shaped names that are deliberately **not** read through this
/// resolver, each with the reason. Keep this list short: every entry is an
/// exception to STYLE-0030.
pub const EXEMPT_SECRET_ENV_VARS: &[(&str, &str)] = &[
    (
        "GMAIL_CLIENT_SECRET",
        "GMAIL_CLIENT_SECRET_FILE already names a Google client_secret.json for \
         `gmail auth import`; an installed-app OAuth client secret is not \
         confidential, and that command already reads it from a file (ADR-0089)",
    ),
    (
        "SNOWFLAKE_PRIVATE_KEY_PASSPHRASE",
        "only read to reject encrypted keys, which are unsupported; include it \
         once encrypted keys are supported (#2006)",
    ),
    (
        "PROGRAMMATIC_ACCESS_TOKEN",
        "a Snowflake authenticator wire value, not an environment variable",
    ),
];

/// Why a secret could not be resolved. Messages name variables, paths, modes
/// and uids only — never the secret's bytes.
#[derive(Debug, Error)]
pub enum SecretEnvError {
    /// `NAME` and `NAME_FILE` are both set in the same layer.
    #[error("both {name} and {file_var} are set; set only one of them")]
    Conflict {
        /// The variable holding the secret directly.
        name: String,
        /// Its `_FILE` companion.
        file_var: String,
    },

    /// The `_FILE` value is not an absolute path.
    #[error("{file_var} must be an absolute path, got '{}'", path.display())]
    RelativePath {
        /// The variable naming the file.
        file_var: String,
        /// The offending path.
        path: PathBuf,
    },

    /// The file could not be opened, inspected, or read.
    #[error("cannot read the secret file {} named by {file_var}: {source}", path.display())]
    Unreadable {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The file could not be written.
    #[error("cannot write the secret file {} named by {file_var}: {source}", path.display())]
    Unwritable {
        /// The key naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The path (after following symlinks) is not a regular file.
    #[error("the secret file {} named by {file_var} is not a regular file", path.display())]
    NotAFile {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
    },

    /// The file is empty (after trimming one trailing newline).
    #[error("the secret file {} named by {file_var} is empty", path.display())]
    Empty {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
    },

    /// The file's contents are not valid UTF-8.
    #[error("the secret file {} named by {file_var} is not valid UTF-8", path.display())]
    NotUtf8 {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
    },

    /// A file owned by the current user grants group or other permission
    /// bits.
    #[error(
        "the secret file {path} named by {file_var} has mode {mode}, which grants group or \
         other access; restrict it with `chmod 600 {path}`",
        path = path.display(),
        mode = OctalMode(*mode)
    )]
    LoosePermissions {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
        /// The permission bits found (`st_mode & 0o777`).
        mode: u32,
    },

    /// A root-owned file that group or other users can write.
    #[error(
        "the secret file {path} named by {file_var} is owned by root but has mode {mode}, \
         which lets group or other users write it; remove that with `chmod go-w {path}`",
        path = path.display(),
        mode = OctalMode(*mode)
    )]
    WritableByOthers {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
        /// The permission bits found (`st_mode & 0o777`).
        mode: u32,
    },

    /// The file is owned by neither the current user nor root.
    #[error(
        "the secret file {} named by {file_var} is owned by uid {owner}, which is neither \
         the current user (uid {expected}) nor root; take ownership of it (or copy it) and \
         `chmod 600` it",
        path.display()
    )]
    WrongOwner {
        /// The variable naming the file.
        file_var: String,
        /// The file's path.
        path: PathBuf,
        /// The file's owner uid.
        owner: u32,
        /// The effective uid of this process.
        expected: u32,
    },
}

/// Formats permission bits as `0644`.
struct OctalMode(u32);

impl fmt::Display for OctalMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04o}", self.0)
    }
}

/// Returns `<name>_FILE`.
#[must_use]
pub fn file_var_name(name: &str) -> String {
    format!("{name}{FILE_SUFFIX}")
}

/// Resolves the secret `name`, from `name` itself or from the file its
/// `name_FILE` companion points at. `Ok(None)` means neither is set.
///
/// # Errors
///
/// See [`SecretEnvError`]: both set in one layer, or a `_FILE` that is
/// relative, unreadable, not a regular file, empty, not UTF-8, or neither
/// the current user's and owner-only nor root's and read-only to others.
pub fn secret_var(env: &impl EnvSource, name: &str) -> Result<Option<Secret>, SecretEnvError> {
    debug_assert!(
        SECRET_ENV_VARS.contains(&name),
        "{name} must be registered in SECRET_ENV_VARS"
    );
    let file_var = file_var_name(name);
    let (value, file) = env.var_pair(name, &file_var);
    resolve_secret_pair(name, value.as_deref(), &file_var, file.as_deref())
}

/// Resolves one already-read secret pair: the secret held directly
/// (`value`, labelled `name`) or a path to a file holding it (`file`,
/// labelled `file_var`). `Ok(None)` means neither is set.
///
/// [`secret_var`]'s rules, for secrets that are not environment variables:
/// the named-account `client_secret`/`client_secret_file` and
/// `refresh_token`/`refresh_token_file` fields in settings.json (#2008). The
/// labels only name the pair in errors, e.g.
/// `drive.accounts.work.client_secret`.
///
/// # Errors
///
/// As [`secret_var`].
pub fn resolve_secret_pair(
    name: &str,
    value: Option<&str>,
    file_var: &str,
    file: Option<&str>,
) -> Result<Option<Secret>, SecretEnvError> {
    let value = value.filter(|v| !v.is_empty());
    let file = file.filter(|v| !v.is_empty());
    match (value, file) {
        (Some(_), Some(_)) => Err(SecretEnvError::Conflict {
            name: name.to_string(),
            file_var: file_var.to_string(),
        }),
        (Some(value), None) => Ok(Some(Secret::new(value))),
        (None, Some(path)) => read_secret_file(file_var, Path::new(path)).map(Some),
        (None, None) => Ok(None),
    }
}

/// [`secret_var`] over aliases in precedence order: the first name whose
/// pair is set wins. A conflict is only ever within one name's pair.
///
/// # Errors
///
/// As [`secret_var`], for the first name whose pair is set.
pub fn secret_var_any(
    env: &impl EnvSource,
    names: &[&str],
) -> Result<Option<Secret>, SecretEnvError> {
    for name in names {
        if let Some(secret) = secret_var(env, name)? {
            return Ok(Some(secret));
        }
    }
    Ok(None)
}

/// Whether `name` or its `_FILE` companion is set (non-empty), without
/// reading the file. For presence-only reports such as `auth status`.
#[must_use]
pub fn secret_var_is_set(env: &impl EnvSource, name: &str) -> bool {
    debug_assert!(
        SECRET_ENV_VARS.contains(&name),
        "{name} must be registered in SECRET_ENV_VARS"
    );
    let (value, file) = env.var_pair(name, &file_var_name(name));
    value.is_some_and(|v| !v.is_empty()) || file.is_some_and(|v| !v.is_empty())
}

/// Reads a secret from `path`, which the variable `file_var` named.
///
/// Checks the path is absolute, follows symlinks, requires a regular file
/// that is either the current user's and owner-only or root's and not
/// writable by others (Unix), and trims exactly one trailing newline.
///
/// # Errors
///
/// See [`SecretEnvError`].
pub fn read_secret_file(file_var: &str, path: &Path) -> Result<Secret, SecretEnvError> {
    if !path.is_absolute() {
        return Err(SecretEnvError::RelativePath {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
        });
    }
    let unreadable = |source| SecretEnvError::Unreadable {
        file_var: file_var.to_string(),
        path: path.to_path_buf(),
        source,
    };
    // Stat before opening so a FIFO or device never blocks the open, then
    // re-check the opened handle so the checks apply to what is actually read.
    check_metadata(
        file_var,
        path,
        &std::fs::metadata(path).map_err(unreadable)?,
    )?;
    let mut file = std::fs::File::open(path).map_err(unreadable)?;
    check_metadata(file_var, path, &file.metadata().map_err(unreadable)?)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(unreadable)?;
    let mut text = String::from_utf8(bytes).map_err(|_| SecretEnvError::NotUtf8 {
        file_var: file_var.to_string(),
        path: path.to_path_buf(),
    })?;
    trim_one_newline(&mut text);
    if text.is_empty() {
        return Err(SecretEnvError::Empty {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
        });
    }
    Ok(Secret::new(text))
}

/// Stores `secret` in the file at `path`, which the key `file_var` named,
/// so the next [`read_secret_file`] returns it. Returns whether anything was
/// written.
///
/// Used when a login or import has a new value for a settings.json secret
/// field whose `_file` companion is set (#2008): the value goes where the
/// user said it lives, never back into settings.json.
///
/// - If the file already holds `secret`, nothing is written. That keeps a
///   file shared by several accounts untouched, and keeps a root-owned
///   read-only mount usable while the secret is unchanged.
/// - Otherwise the write is atomic: symlinks are followed to the target (as
///   [`read_secret_file`] does), a `0600` temp file is created beside it,
///   synced, and renamed over it. The parent directory must already exist.
///
/// # Errors
///
/// [`SecretEnvError::RelativePath`] for a relative path, and
/// [`SecretEnvError::Unwritable`] when the file can't be written.
pub fn write_secret_file(
    file_var: &str,
    path: &Path,
    secret: &Secret,
) -> Result<bool, SecretEnvError> {
    if !path.is_absolute() {
        return Err(SecretEnvError::RelativePath {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
        });
    }
    if read_secret_file(file_var, path)
        .is_ok_and(|current| current.expose_secret() == secret.expose_secret())
    {
        return Ok(false);
    }
    let unwritable = |source| SecretEnvError::Unwritable {
        file_var: file_var.to_string(),
        path: path.to_path_buf(),
        source,
    };
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.to_path_buf(),
        Err(e) => return Err(unwritable(e)),
    };
    let (Some(dir), Some(file_name)) = (target.parent(), target.file_name()) else {
        return Err(unwritable(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path has no parent directory",
        )));
    };
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(file_name);
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = dir.join(temp_name);
    let result = write_new_0600(&temp, secret.expose_secret())
        .and_then(|()| std::fs::rename(&temp, &target));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&temp);
        return Err(unwritable(e));
    }
    Ok(true)
}

/// Creates `path` (which must not exist) owner-only and writes `contents`
/// plus a trailing newline, synced to disk.
fn write_new_0600(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}

/// Removes exactly one trailing `\n` or `\r\n`, and nothing else: secrets may
/// legitimately contain other whitespace.
fn trim_one_newline(text: &mut String) {
    if text.ends_with("\r\n") {
        text.truncate(text.len() - 2);
    } else if text.ends_with('\n') {
        text.truncate(text.len() - 1);
    }
}

/// Requires a regular file and, on Unix, passes [`check_unix_security`].
fn check_metadata(
    file_var: &str,
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), SecretEnvError> {
    if !metadata.is_file() {
        return Err(SecretEnvError::NotAFile {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        check_unix_security(
            file_var,
            path,
            metadata.mode(),
            metadata.uid(),
            nix::unistd::geteuid().as_raw(),
        )?;
    }
    Ok(())
}

/// The uid of `root`, whose read-only files are accepted as secrets.
#[cfg(unix)]
const ROOT_UID: u32 = 0;

/// The pure half of [`check_metadata`]'s Unix check, so the root-owned and
/// wrong-owner cases are testable without root.
///
/// - A **root-owned** file is accepted when no group or other user can write
///   it (`mode & 0o022 == 0`). Group and other *read* is allowed, because
///   that is how Kubernetes (`fsGroup`, default `0644`) and Docker Swarm
///   (`0444`) project secrets. Only root could have put it there, so a
///   same-user process can't have planted or swapped it.
/// - A file **owned by the effective uid** must be owner-only
///   (`mode & 0o077 == 0`): on a workstation there is no reason for anyone
///   else to see it.
/// - Any other owner is refused.
#[cfg(unix)]
fn check_unix_security(
    file_var: &str,
    path: &Path,
    mode: u32,
    owner: u32,
    euid: u32,
) -> Result<(), SecretEnvError> {
    let mode = mode & 0o777;
    if owner == ROOT_UID {
        if mode & 0o022 != 0 {
            return Err(SecretEnvError::WritableByOthers {
                file_var: file_var.to_string(),
                path: path.to_path_buf(),
                mode,
            });
        }
        return Ok(());
    }
    if owner != euid {
        return Err(SecretEnvError::WrongOwner {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
            owner,
            expected: euid,
        });
    }
    if mode & 0o077 != 0 {
        return Err(SecretEnvError::LoosePermissions {
            file_var: file_var.to_string(),
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::test_support::env::MapEnv;

    const NAME: &str = "DATADOG_API_KEY";
    const FILE_VAR: &str = "DATADOG_API_KEY_FILE";
    const SECRET_BYTES: &str = "sekret-bytes-never-logged";

    /// Writes `contents` to a fresh file with `mode` in its own temp dir.
    fn secret_file(contents: &[u8], mode: u32) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::fs::write(&path, contents).unwrap();
        set_mode(&path, mode);
        (dir, path)
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) {}

    fn env_with_file(path: &Path) -> MapEnv {
        MapEnv::new().with(FILE_VAR, path.to_str().unwrap())
    }

    fn resolve(env: &MapEnv) -> Result<Option<String>, SecretEnvError> {
        secret_var(env, NAME).map(|s| s.map(|s| s.expose_secret().to_string()))
    }

    #[test]
    fn only_the_variable_is_used_verbatim() {
        let env = MapEnv::new().with(NAME, " value with spaces \n");
        assert_eq!(
            resolve(&env).unwrap().as_deref(),
            Some(" value with spaces \n")
        );
    }

    #[test]
    fn only_the_file_is_read() {
        let (_dir, path) = secret_file(b"from-file\n", 0o600);
        assert_eq!(
            resolve(&env_with_file(&path)).unwrap().as_deref(),
            Some("from-file")
        );
    }

    #[test]
    fn neither_set_is_none() {
        assert!(resolve(&MapEnv::new()).unwrap().is_none());
    }

    #[test]
    fn empty_values_count_as_unset() {
        let env = MapEnv::new().with(NAME, "").with(FILE_VAR, "");
        assert!(resolve(&env).unwrap().is_none());
        assert!(!secret_var_is_set(&env, NAME));
        // An empty NAME does not conflict with a set NAME_FILE.
        let (_dir, path) = secret_file(b"f", 0o600);
        let env = env_with_file(&path).with(NAME, "");
        assert_eq!(resolve(&env).unwrap().as_deref(), Some("f"));
    }

    #[test]
    fn both_set_is_a_conflict_naming_both() {
        let (_dir, path) = secret_file(b"f", 0o600);
        let env = env_with_file(&path).with(NAME, "v");
        let err = resolve(&env).unwrap_err();
        assert!(matches!(err, SecretEnvError::Conflict { .. }));
        let msg = err.to_string();
        assert!(msg.contains(NAME) && msg.contains(FILE_VAR), "{msg}");
    }

    #[test]
    fn empty_file_is_an_error_not_unset() {
        for contents in [&b""[..], b"\n", b"\r\n"] {
            let (_dir, path) = secret_file(contents, 0o600);
            let err = resolve(&env_with_file(&path)).unwrap_err();
            assert!(matches!(err, SecretEnvError::Empty { .. }), "{contents:?}");
        }
    }

    #[test]
    fn missing_file_is_unreadable_and_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        let err = resolve(&env_with_file(&path)).unwrap_err();
        assert!(matches!(err, SecretEnvError::Unreadable { .. }));
        assert!(err.to_string().contains(path.to_str().unwrap()));
        assert!(err.to_string().contains(FILE_VAR));
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let err = resolve(&env_with_file(dir.path())).unwrap_err();
        assert!(matches!(err, SecretEnvError::NotAFile { .. }));
    }

    #[test]
    fn relative_path_is_rejected() {
        let env = MapEnv::new().with(FILE_VAR, "secrets/key");
        let err = resolve(&env).unwrap_err();
        assert!(matches!(err, SecretEnvError::RelativePath { .. }));
        assert!(err.to_string().contains("absolute"));
    }

    #[test]
    fn trims_exactly_one_trailing_crlf() {
        let (_dir, path) = secret_file(b"crlf\r\n", 0o600);
        assert_eq!(
            resolve(&env_with_file(&path)).unwrap().as_deref(),
            Some("crlf")
        );
    }

    #[test]
    fn trims_only_one_newline_and_keeps_other_whitespace() {
        let (_dir, path) = secret_file(b"  in ner \t\n\n", 0o600);
        assert_eq!(
            resolve(&env_with_file(&path)).unwrap().as_deref(),
            Some("  in ner \t\n")
        );
    }

    #[test]
    fn non_utf8_is_rejected() {
        let (_dir, path) = secret_file(&[0xff, 0xfe, b'x'], 0o600);
        let err = resolve(&env_with_file(&path)).unwrap_err();
        assert!(matches!(err, SecretEnvError::NotUtf8 { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn mode_0600_and_0400_are_accepted() {
        for mode in [0o600, 0o400] {
            let (_dir, path) = secret_file(b"ok", mode);
            assert_eq!(
                resolve(&env_with_file(&path)).unwrap().as_deref(),
                Some("ok")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn mode_0644_is_rejected_with_the_mode_and_the_fix() {
        let (_dir, path) = secret_file(SECRET_BYTES.as_bytes(), 0o644);
        let err = resolve(&env_with_file(&path)).unwrap_err();
        assert!(matches!(
            err,
            SecretEnvError::LoosePermissions { mode: 0o644, .. }
        ));
        let msg = err.to_string();
        assert!(msg.contains("0644"), "{msg}");
        assert!(
            msg.contains(&format!("chmod 600 {}", path.display())),
            "{msg}"
        );
        assert!(!msg.contains(SECRET_BYTES), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn any_group_or_other_bit_is_rejected() {
        for mode in [0o640, 0o604, 0o610, 0o601] {
            let (_dir, path) = secret_file(b"x", mode);
            // omni-dev: coverage ignore reason="the loop above runs this 4 times and every mode fails the same way; verified locally that llvm-cov still reports 0 hits on the matches!( line — a region-attribution artifact on the nested assert!/matches! macro call, not an untested path"
            assert!(
                matches!(
                    resolve(&env_with_file(&path)).unwrap_err(),
                    SecretEnvError::LoosePermissions { .. }
                ),
                "{mode:o}"
            );
            // omni-dev: coverage end
        }
    }

    #[cfg(unix)]
    #[test]
    fn another_users_file_is_rejected_by_the_pure_check() {
        let path = Path::new("/home/other/key");
        let err = check_unix_security(FILE_VAR, path, 0o100_600, 502, 501).unwrap_err();
        assert!(matches!(
            err,
            SecretEnvError::WrongOwner {
                owner: 502,
                expected: 501,
                ..
            }
        ));
        let msg = err.to_string();
        assert!(msg.contains("uid 502") && msg.contains("uid 501"), "{msg}");
        // The file type bits are masked off before the mode check.
        assert!(check_unix_security(FILE_VAR, path, 0o100_600, 501, 501).is_ok());
    }

    /// Container secrets are root-owned and often group/world-readable:
    /// Kubernetes projects `0644` (or `0440` with `fsGroup`), Docker Swarm
    /// `0444`. They are accepted whether the process runs as root or not.
    #[cfg(unix)]
    #[test]
    fn root_owned_read_only_secrets_are_accepted() {
        let path = Path::new("/run/secrets/key");
        for mode in [0o100_400, 0o100_440, 0o100_444, 0o100_600, 0o100_644] {
            for euid in [0, 501] {
                assert!(
                    check_unix_security(FILE_VAR, path, mode, ROOT_UID, euid).is_ok(),
                    "mode {mode:o} euid {euid}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_owned_secrets_writable_by_others_are_rejected() {
        let path = Path::new("/run/secrets/key");
        for mode in [0o100_664, 0o100_646, 0o100_666, 0o100_620] {
            let err = check_unix_security(FILE_VAR, path, mode, ROOT_UID, 501).unwrap_err();
            assert!(
                matches!(err, SecretEnvError::WritableByOthers { .. }),
                "mode {mode:o}"
            );
            let msg = err.to_string();
            assert!(msg.contains("chmod go-w /run/secrets/key"), "{msg}");
        }
    }

    /// Running as root doesn't loosen the rule for root's own files beyond
    /// the container shape: still no group/other write.
    #[cfg(unix)]
    #[test]
    fn root_process_still_rejects_group_writable_root_files() {
        let path = Path::new("/run/secrets/key");
        assert!(matches!(
            check_unix_security(FILE_VAR, path, 0o100_660, ROOT_UID, ROOT_UID).unwrap_err(),
            SecretEnvError::WritableByOthers { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_followed_and_the_target_is_checked() {
        let (dir, target) = secret_file(b"via-link\n", 0o600);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            resolve(&env_with_file(&link)).unwrap().as_deref(),
            Some("via-link")
        );

        set_mode(&target, 0o644);
        assert!(matches!(
            resolve(&env_with_file(&link)).unwrap_err(),
            SecretEnvError::LoosePermissions { mode: 0o644, .. }
        ));
    }

    #[test]
    fn no_error_message_contains_the_secret_bytes() {
        let (_dir, path) = secret_file(SECRET_BYTES.as_bytes(), 0o600);
        let env = env_with_file(&path).with(NAME, SECRET_BYTES);
        let err = resolve(&env).unwrap_err();
        assert!(!err.to_string().contains(SECRET_BYTES));
        assert!(!format!("{err:?}").contains(SECRET_BYTES));
    }

    #[test]
    fn aliases_keep_precedence_and_only_conflict_within_a_pair() {
        let (_dir, path) = secret_file(b"from-file", 0o600);
        let names = ["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"];

        // A_FILE with B set: A wins, no conflict.
        let env = MapEnv::new()
            .with("OPENAI_API_KEY_FILE", path.to_str().unwrap())
            .with("OPENAI_AUTH_TOKEN", "b");
        let got = secret_var_any(&env, &names).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "from-file");

        // Only the lower-precedence alias's file is set.
        let env = MapEnv::new().with("OPENAI_AUTH_TOKEN_FILE", path.to_str().unwrap());
        let got = secret_var_any(&env, &names).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "from-file");

        // A conflict within the winning pair is still an error.
        let env = MapEnv::new()
            .with("OPENAI_API_KEY", "a")
            .with("OPENAI_API_KEY_FILE", path.to_str().unwrap());
        assert!(matches!(
            secret_var_any(&env, &names).unwrap_err(),
            SecretEnvError::Conflict { .. }
        ));

        assert!(secret_var_any(&MapEnv::new(), &names).unwrap().is_none());
    }

    #[test]
    fn is_set_does_not_read_the_file() {
        let env = MapEnv::new().with(FILE_VAR, "/definitely/not/there");
        assert!(secret_var_is_set(&env, NAME));
        assert!(secret_var_is_set(&MapEnv::new().with(NAME, "v"), NAME));
        assert!(!secret_var_is_set(&MapEnv::new(), NAME));
    }

    #[test]
    fn registry_and_exemptions_are_disjoint_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for name in SECRET_ENV_VARS
            .iter()
            .chain(EXEMPT_SECRET_ENV_VARS.iter().map(|(n, _)| n))
        {
            assert!(seen.insert(*name), "{name} is listed twice");
            assert!(!name.ends_with(FILE_SUFFIX), "{name}");
        }
    }

    // ── Grep guards (STYLE-0030) ──

    /// One production source file: its path relative to `src/` and its text
    /// with inline `#[cfg(test)] mod … { … }` blocks and comment lines removed.
    struct Source {
        rel: String,
        code: String,
    }

    fn production_sources() -> Vec<Source> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src, &mut files);
        files
            .into_iter()
            .filter_map(|path| {
                let rel = path
                    .strip_prefix(&src)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let name = path.file_name().unwrap().to_string_lossy();
                let is_test_file = rel == "test_support.rs"
                    || name == "tests.rs"
                    || name.ends_with("_tests.rs")
                    || rel.contains("/tests/");
                if is_test_file {
                    return None;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                let code = strip_test_modules(&rel, &text)
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("//"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(Source { rel, code })
            })
            .collect()
    }

    /// Removes each `#[cfg(test)]`-gated `mod name { … }` block, matching
    /// braces with [`matching_brace`] so braces inside strings, chars and
    /// comments don't count. Panics on a block that never closes, so a lexer
    /// gap fails loudly instead of silently hiding the rest of the file.
    fn strip_test_modules(rel: &str, text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("#[cfg(test)]") {
            out.push_str(&rest[..at]);
            let after = &rest[at..];
            let brace = after.find('{');
            let semi = after.find(';');
            // Skip any further attribute lines, then look for `mod …`.
            let mut item = after["#[cfg(test)]".len()..].trim_start();
            while item.starts_with("#[") {
                item = item.find('\n').map_or("", |nl| item[nl..].trim_start());
            }
            let is_mod_block = (item.starts_with("mod ") || item.starts_with("pub(crate) mod "))
                && brace.is_some_and(|b| semi.is_none_or(|s| b < s));
            if !is_mod_block {
                out.push_str("#[cfg(test)]");
                rest = &after["#[cfg(test)]".len()..];
                continue;
            }
            let start = brace.unwrap();
            let end = matching_brace(&after[start..])
                .unwrap_or_else(|| panic!("{rel}: unbalanced #[cfg(test)] module"));
            rest = &after[start + end + 1..];
        }
        out.push_str(rest);
        out
    }

    /// Byte offset of the `}` closing the `{` that `text` starts with,
    /// skipping string, raw-string, char and byte literals and comments.
    /// Lifetimes (`'a`) are told apart from char literals by the closing `'`.
    fn matching_brace(text: &str) -> Option<usize> {
        let b = text.as_bytes();
        let mut depth = 0usize;
        let mut i = 0;
        while i < b.len() {
            match b[i] {
                b'/' if b.get(i + 1) == Some(&b'/') => {
                    i = text[i..].find('\n').map_or(b.len(), |n| i + n);
                    continue;
                }
                b'/' if b.get(i + 1) == Some(&b'*') => {
                    i = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
                    continue;
                }
                b'r' if !is_ident_byte(b, i) && matches!(b.get(i + 1), Some(b'"' | b'#')) => {
                    let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                    if b.get(i + 1 + hashes) == Some(&b'"') {
                        let close = format!("\"{}", "#".repeat(hashes));
                        let body = i + 2 + hashes;
                        i = text[body..]
                            .find(&close)
                            .map_or(b.len(), |n| body + n + close.len());
                        continue;
                    }
                }
                b'"' => {
                    i += 1;
                    while i < b.len() && b[i] != b'"' {
                        i += if b[i] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\'' => {
                    // A char literal: '\x…' or a single (possibly multi-byte)
                    // char followed by a closing quote. Otherwise a lifetime.
                    if b.get(i + 1) == Some(&b'\\') {
                        if let Some(n) = text[i + 2..].find('\'') {
                            i += 2 + n;
                        }
                    } else if let Some(c) = text[i + 1..].chars().next() {
                        if text[i + 1 + c.len_utf8()..].starts_with('\'') {
                            i += 1 + c.len_utf8();
                        }
                    } // omni-dev: coverage ignore-line reason="exercised by strip_test_modules_ignores_braces_in_literals_and_comments's '{' char literal; verified locally that llvm-cov still reports 0 hits — a region-attribution artifact on this closing brace, not an untested path"
                }
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Whether the byte before `i` continues an identifier (so an `r` there
    /// is not a raw-string prefix).
    fn is_ident_byte(b: &[u8], i: usize) -> bool {
        i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
    }

    /// Every `"UPPER_SNAKE"` string literal in `code`.
    fn env_like_literals(code: &str) -> Vec<String> {
        let re = regex::Regex::new(r#""([A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+)""#).unwrap();
        re.captures_iter(code).map(|c| c[1].to_string()).collect()
    }

    fn is_secret_shaped(name: &str) -> bool {
        [
            "_API_KEY",
            "_APP_KEY",
            "_TOKEN",
            "_SECRET",
            "_PRIVATE_KEY",
            "_PASSWORD",
            "_PASSPHRASE",
            "_CREDENTIALS",
        ]
        .iter()
        .any(|s| name.ends_with(s))
    }

    fn is_known(name: &str) -> bool {
        SECRET_ENV_VARS.contains(&name) || EXEMPT_SECRET_ENV_VARS.iter().any(|(n, _)| *n == name)
    }

    /// (a) A new secret-shaped variable must be registered (and so read
    /// through the resolver) or exempted with a reason.
    #[test]
    fn every_secret_shaped_literal_is_registered_or_exempt() {
        let mut unknown = Vec::new();
        for source in production_sources() {
            for name in env_like_literals(&source.code) {
                if is_secret_shaped(&name) && !is_known(&name) {
                    unknown.push(format!("{}: {name}", source.rel)); // omni-dev: coverage ignore-line reason="only runs if an unregistered secret-shaped literal exists; the assert below that unknown is empty is this test's whole point, so a passing run never takes this branch"
                }
            }
        }
        assert!(
            unknown.is_empty(),
            "secret-shaped variables must be added to SECRET_ENV_VARS (and read via \
             secret_var) or to EXEMPT_SECRET_ENV_VARS with a reason (STYLE-0030): \
             {unknown:#?}"
        );
    }

    /// (b) No registered secret is read through a plain env accessor outside
    /// this module — neither by literal nor through a `const` bound to one.
    #[test]
    fn registered_secrets_are_never_read_through_plain_accessors() {
        let sources = production_sources();
        let const_re =
            regex::Regex::new(r#"const ([A-Z][A-Z0-9_]*): &str = "([A-Z0-9_]+)";"#).unwrap();
        // Every spelling that denotes a registered secret: the literal, and
        // every const ident bound to it anywhere in the crate.
        let mut spellings: Vec<(String, &str)> = SECRET_ENV_VARS
            .iter()
            .map(|n| (format!("\"{n}\""), *n))
            .collect();
        for source in &sources {
            for c in const_re.captures_iter(&source.code) {
                if let Some(name) = SECRET_ENV_VARS.iter().find(|n| **n == &c[2]) {
                    spellings.push((c[1].to_string(), name));
                }
            }
        }
        let ident_re = |s: &str| regex::Regex::new(&format!(r"\b{}\b", regex::escape(s))).unwrap();
        let spelling_res: Vec<_> = spellings
            .iter()
            .map(|(s, n)| {
                let re = if s.starts_with('"') {
                    regex::Regex::new(&regex::escape(s)).unwrap()
                } else {
                    ident_re(s)
                };
                (re, *n)
            })
            .collect();
        let call_re = regex::Regex::new(
            r"(?:\.var|\bvar_any|\bget_env_var|\bnon_empty_var|\benv::var|\bvar_os|\btruthy_var)\s*\(",
        )
        .unwrap();

        let mut offenders = Vec::new();
        for source in &sources {
            if source.rel == "utils/secret_env.rs" {
                continue;
            }
            for m in call_re.find_iter(&source.code) {
                let args = balanced_args(&source.code[m.end()..]);
                for (re, name) in &spelling_res {
                    if re.is_match(args) {
                        // omni-dev: coverage ignore reason="only runs if a registered secret is read through a plain accessor outside this module; offenders.is_empty() below is this test's whole point"
                        offenders.push(format!("{}: `{}…` reads {name}", source.rel, m.as_str()));
                        // omni-dev: coverage end
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "registered secrets must be read through utils::secret_env (STYLE-0030): \
             {offenders:#?}"
        );
    }

    /// The text up to the `)` closing a call whose `(` was just consumed.
    fn balanced_args(after_paren: &str) -> &str {
        let mut depth = 1usize;
        for (i, c) in after_paren.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &after_paren[..i];
                    }
                }
                _ => {}
            }
        }
        after_paren
    }

    /// (c) No registered `<NAME>_FILE` collides with a variable that already
    /// means something else: the companion names must not appear as literals
    /// anywhere in production code (they are only ever built by
    /// [`file_var_name`]).
    #[test]
    fn no_registered_file_companion_collides_with_an_existing_variable() {
        let companions: Vec<String> = SECRET_ENV_VARS.iter().map(|n| file_var_name(n)).collect();
        let mut collisions = Vec::new();
        for source in production_sources() {
            for name in env_like_literals(&source.code) {
                if companions.contains(&name) {
                    collisions.push(format!("{}: {name}", source.rel)); // omni-dev: coverage ignore-line reason="only runs if a _FILE companion collides with an existing variable; collisions.is_empty() below is this test's whole point"
                }
            }
        }
        assert!(
            collisions.is_empty(),
            "a registered secret's _FILE companion is already a variable with another \
             meaning; rename it or exempt the secret (ADR-0089): {collisions:#?}"
        );
        // The known collision is why GMAIL_CLIENT_SECRET is exempt.
        assert!(!SECRET_ENV_VARS.contains(&"GMAIL_CLIENT_SECRET"));
    }

    /// Self-check for the guards' source stripping: test modules go, the
    /// production code around them stays.
    #[test]
    fn strip_test_modules_removes_only_test_blocks() {
        let text = "fn a() {}\n#[cfg(test)]\nfn helper() {}\n#[cfg(test)]\n\
                    #[allow(x)]\nmod tests {\n fn t() { if x { } }\n}\nfn b() {}\n";
        let stripped = strip_test_modules("t.rs", text);
        assert!(stripped.contains("fn a()"));
        assert!(stripped.contains("fn helper()"));
        assert!(stripped.contains("fn b()"));
        assert!(!stripped.contains("fn t()"));
    }

    /// Self-check: braces inside strings, chars and comments inside a test
    /// module don't end it early or keep it open, so code after it is seen.
    #[test]
    fn strip_test_modules_ignores_braces_in_literals_and_comments() {
        let text = "#[cfg(test)]\nmod tests {\n let a = '{'; let b = \"}}\\\"{\";\n \
                    let c = r#\"{\"#; // }\n /* { */ fn f<'a>(x: &'a str) {}\n}\nfn after() {}\n";
        let stripped = strip_test_modules("t.rs", text);
        assert!(stripped.contains("fn after()"), "{stripped}");
        assert!(!stripped.contains("fn f<"), "{stripped}");
    }

    /// Self-check: an unclosed `{` (a malformed test module) has no matching
    /// brace, rather than looping forever or panicking itself — the panic on
    /// `None` belongs to `strip_test_modules`'s caller, not this scanner.
    #[test]
    fn matching_brace_returns_none_when_unclosed() {
        assert_eq!(matching_brace("{ fn f() { }"), None);
    }

    /// Self-check: nested parens are depth-counted rather than stopping at
    /// the first `)`, and text with no closing paren at all is returned
    /// unchanged (the caller's regex match is always balanced Rust source in
    /// practice, but the helper must still degrade rather than panic).
    #[test]
    fn balanced_args_counts_nested_parens_and_degrades_when_unclosed() {
        assert_eq!(balanced_args("f(x), y) rest"), "f(x), y");
        assert_eq!(balanced_args("no closing paren"), "no closing paren");
    }

    // ── resolve_secret_pair / write_secret_file (#2008) ─────────────────

    #[test]
    fn resolve_secret_pair_uses_the_given_labels_in_a_conflict() {
        let err = resolve_secret_pair(
            "drive.accounts.work.client_secret",
            Some("v"),
            "drive.accounts.work.client_secret_file",
            Some("/x"),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "both drive.accounts.work.client_secret and \
             drive.accounts.work.client_secret_file are set; set only one of them"
        );
    }

    #[test]
    fn resolve_secret_pair_treats_empty_members_as_unset() {
        assert!(resolve_secret_pair("a", Some(""), "a_file", Some(""))
            .unwrap()
            .is_none());
        let resolved = resolve_secret_pair("a", Some("v"), "a_file", Some("")).unwrap();
        assert_eq!(resolved.unwrap().expose_secret(), "v");
    }

    #[test]
    fn write_secret_file_creates_an_owner_only_file_the_reader_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        let written =
            write_secret_file("k_file", &path, &Secret::new(SECRET_BYTES.to_string())).unwrap();
        assert!(written);
        assert_eq!(
            read_secret_file("k_file", &path).unwrap().expose_secret(),
            SECRET_BYTES
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // No temp file is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn write_secret_file_replaces_a_different_value() {
        let (_dir, path) = secret_file(b"old\n", 0o600);
        assert!(write_secret_file("k_file", &path, &Secret::new("new".to_string())).unwrap());
        assert_eq!(
            read_secret_file("k_file", &path).unwrap().expose_secret(),
            "new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_leaves_an_unchanged_file_alone() {
        use std::os::unix::fs::MetadataExt;
        let (_dir, path) = secret_file(b"same\n", 0o600);
        let inode = std::fs::metadata(&path).unwrap().ino();
        assert!(!write_secret_file("k_file", &path, &Secret::new("same".to_string())).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_writes_through_a_symlink_to_its_target() {
        let (dir, target) = secret_file(b"old\n", 0o600);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_secret_file("k_file", &link, &Secret::new("new".to_string())).unwrap());
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            read_secret_file("k_file", &target).unwrap().expose_secret(),
            "new"
        );
    }

    #[test]
    fn write_secret_file_rejects_a_relative_path() {
        let err = write_secret_file(
            "k_file",
            Path::new("rel/token"),
            &Secret::new("v".to_string()),
        )
        .unwrap_err();
        assert!(matches!(err, SecretEnvError::RelativePath { .. }), "{err}");
    }

    #[test]
    fn write_secret_file_reports_a_missing_directory_without_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join("token");
        let err =
            write_secret_file("k_file", &path, &Secret::new(SECRET_BYTES.to_string())).unwrap_err();
        assert!(matches!(err, SecretEnvError::Unwritable { .. }), "{err}");
        let message = err.to_string();
        assert!(message.contains("k_file"), "{message}");
        assert!(!message.contains(SECRET_BYTES), "{message}");
    }

    /// Self-check: the guards see the real read sites (so an empty scan
    /// can't pass vacuously).
    #[test]
    fn guards_scan_the_real_read_sites() {
        let sources = production_sources();
        let all: Vec<String> = sources
            .iter()
            .flat_map(|s| env_like_literals(&s.code))
            .collect();
        for name in ["DATADOG_API_KEY", "OMNI_BRIDGE_TOKEN", "CLAUDE_API_KEY"] {
            assert!(all.iter().any(|n| n == name), "{name} not found");
        }
    }
}

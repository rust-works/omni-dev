//! CLI command for `omni-dev gmail account import-legacy`.

use anyhow::Result;
use clap::Parser;

use crate::gmail::account;
use crate::gmail::auth;
use crate::utils::settings::Settings;

/// Migrates today's legacy Gmail credentials (the base `env` map, or the
/// active `--profile`'s `env` map) into a named account.
///
/// Non-destructive by default: the legacy credentials are left in place
/// unless `--remove-legacy` is passed. This is how a named account gets
/// created from an existing single-account setup, and the only place the
/// zero-migration guarantee is deliberately broken — an explicit,
/// user-initiated migration rather than an automatic one.
#[derive(Parser)]
pub struct ImportLegacyCommand {
    /// Name to migrate the legacy credentials into.
    ///
    /// Deliberately `--name`, not `--account`: the global `--account` flag
    /// is inherited by every subcommand (clap rejects two args bound to
    /// the same long flag on one command), and it means something
    /// different here anyway — this names the account being *created*,
    /// not one being selected.
    #[arg(long, value_name = "NAME", default_value = "default")]
    pub name: String,
    /// Also remove the legacy credentials after a successful migration.
    #[arg(long)]
    pub remove_legacy: bool,
}

impl ImportLegacyCommand {
    /// Reads the legacy credentials (forcing the pre-#1500 resolution path
    /// regardless of any ambient `--account`), writes them to
    /// `gmail.accounts.<name>`, and optionally removes the legacy copy.
    pub fn execute(self) -> Result<()> {
        let legacy = auth::load_credentials_legacy()?;
        let settings = Settings::load().unwrap_or_default();
        let is_first_transition =
            account::is_first_legacy_to_named_transition(&settings.gmail, true);

        Settings::upsert_gmail_account(
            &Settings::get_settings_path()?,
            &self.name,
            &[
                (
                    "client_id",
                    serde_json::Value::String(legacy.client_id.clone()),
                ),
                (
                    "client_secret",
                    serde_json::Value::String(legacy.client_secret.expose_secret().to_string()),
                ),
                (
                    "refresh_token",
                    serde_json::Value::String(legacy.refresh_token.expose_secret().to_string()),
                ),
                (
                    "scope",
                    serde_json::Value::String(legacy.scope.as_str().to_string()),
                ),
            ],
        )?;

        if is_first_transition {
            eprintln!(
                "note: legacy Gmail credentials are now shadowed for invocations without \
                 --account. Run `gmail account import-legacy` again for any other legacy \
                 account, or `gmail auth logout` to remove the old credentials once every \
                 mailbox you use is migrated."
            );
        }

        if self.remove_legacy {
            auth::remove_credentials()?;
            println!(
                "Legacy Gmail credentials migrated to account '{}' and removed from their old \
                 location.",
                self.name
            );
        } else {
            println!(
                "Legacy Gmail credentials migrated to account '{}'. Legacy credentials left in \
                 place — pass --remove-legacy to delete them.",
                self.name
            );
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn seed_legacy_credentials(settings_path: &std::path::Path) {
        Settings::upsert_env_vars(
            settings_path,
            &[
                (auth::GMAIL_CLIENT_ID, "legacy-id"),
                (auth::GMAIL_CLIENT_SECRET, "legacy-secret"),
                (auth::GMAIL_REFRESH_TOKEN, "legacy-refresh"),
            ],
        )
        .unwrap();
    }

    #[test]
    fn import_legacy_errors_when_nothing_to_migrate() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let _dir = guard.clear_credentials();

        let err = ImportLegacyCommand {
            name: "default".to_string(),
            remove_legacy: false,
        }
        .execute()
        .unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[test]
    fn import_legacy_writes_named_account_and_leaves_legacy_by_default() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        seed_legacy_credentials(&settings_path);

        ImportLegacyCommand {
            name: "work".to_string(),
            remove_legacy: false,
        }
        .execute()
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["gmail"]["accounts"]["work"]["client_id"], "legacy-id");
        // Legacy credentials are untouched.
        assert_eq!(val["env"]["GMAIL_CLIENT_ID"], "legacy-id");
    }

    #[test]
    fn import_legacy_remove_flag_deletes_legacy_credentials() {
        let guard = crate::gmail::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        seed_legacy_credentials(&settings_path);

        ImportLegacyCommand {
            name: "work".to_string(),
            remove_legacy: true,
        }
        .execute()
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["gmail"]["accounts"]["work"]["client_id"], "legacy-id");
        assert!(val["env"].get("GMAIL_CLIENT_ID").is_none());
    }

    #[test]
    fn import_legacy_defaults_name_to_default() {
        assert_eq!(
            ImportLegacyCommand {
                name: "default".to_string(),
                remove_legacy: false,
            }
            .name,
            "default"
        );
    }
}

//! CLI command for `omni-dev drive account set-default`.

use anyhow::Result;
use clap::Parser;

use crate::drive::account;
use crate::utils::settings::Settings;

/// Sets the account resolved when `--account`/`OMNI_DEV_DRIVE_ACCOUNT` is
/// not given and more than one account is configured.
#[derive(Parser)]
pub struct SetDefaultCommand {
    /// The account name to make the default. Must already be configured.
    pub name: String,
}

impl SetDefaultCommand {
    /// Validates `name` against the configured accounts, then writes
    /// `drive.default_account`.
    pub fn execute(self) -> Result<()> {
        let settings = Settings::load().unwrap_or_default();
        account::validate_account(&settings.drive, &self.name)?;
        Settings::set_drive_default_account(&Settings::get_settings_path()?, Some(&self.name))?;
        println!("Default Drive account set to '{}'.", self.name);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn set_default_rejects_unknown_account() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        let err = SetDefaultCommand {
            name: "bogus".to_string(),
        }
        .execute()
        .unwrap_err();
        assert!(err.to_string().contains("unknown Drive account 'bogus'"));
    }

    #[test]
    fn set_default_writes_known_account() {
        let guard = crate::drive::test_support::EnvGuard::take();
        let dir = guard.clear_credentials();
        let settings_path = dir.path().join(".omni-dev").join("settings.json");
        Settings::upsert_drive_account(
            &settings_path,
            "work",
            &[("client_id", serde_json::Value::String("id".to_string()))],
        )
        .unwrap();

        SetDefaultCommand {
            name: "work".to_string(),
        }
        .execute()
        .unwrap();

        let val: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(val["drive"]["default_account"], "work");
    }
}

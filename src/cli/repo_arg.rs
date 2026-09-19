//! The `-C/--repo` flag, scoped to the commands that act on a local repository.
//!
//! Flattened as a subtree-`global = true` arg onto `git`, `coverage`, and
//! `config scopes` (every leaf below them reads it), and onto the
//! `worktrees rebase` / `worktrees push` leaves (the only `worktrees`
//! subcommands that touch the local repository) — per the placement rule in
//! [`crate::cli::ai_backend_args`]. Everywhere else clap rejects it rather
//! than silently ignoring it (#1778).

use std::path::{Path, PathBuf};

use clap::Args;

/// `-C/--repo <PATH>`.
#[derive(Args, Debug, Clone, Default)]
pub struct RepoArg {
    /// Run as if omni-dev was started in `<PATH>` instead of the current
    /// working directory.
    ///
    /// Resolved exactly once by the owning command and threaded explicitly to
    /// its leaves as a parameter; deliberately **not** propagated to an
    /// environment variable (so, unlike the other scoped flags, `RepoArg` has
    /// no `apply()`), so the repo location never becomes an ambient global.
    /// Mirrors `git -C`.
    #[arg(long = "repo", short = 'C', global = true, value_name = "PATH")]
    pub repo: Option<PathBuf>,
}

impl RepoArg {
    /// The repository location, `None` meaning the current working directory.
    pub fn path(&self) -> Option<&Path> {
        self.repo.as_deref()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::cli::{Cli, Commands};
    use clap::Parser;

    fn git_repo(argv: &[&str]) -> Option<std::path::PathBuf> {
        let cli = Cli::try_parse_from(argv).unwrap();
        let Commands::Git(git) = cli.command else {
            panic!("expected a git command");
        };
        git.repo.repo
    }

    #[test]
    fn parses_long_and_short_after_the_owning_subcommand() {
        let view = ["commit", "message", "view", "HEAD"];
        for flag in ["--repo", "-C"] {
            // Directly after the owner, and — being subtree-global — after the leaf.
            let early: Vec<&str> = ["omni-dev", "git", flag, "/tmp/r"]
                .into_iter()
                .chain(view)
                .collect();
            let late: Vec<&str> = ["omni-dev", "git"]
                .into_iter()
                .chain(view)
                .chain([flag, "/tmp/r"])
                .collect();
            for argv in [early, late] {
                assert_eq!(
                    git_repo(&argv).as_deref(),
                    Some(std::path::Path::new("/tmp/r")),
                    "{argv:?}"
                );
            }
        }
        assert!(git_repo(&["omni-dev", "git", "commit", "message", "view"]).is_none());
    }

    /// The pre-#1778 placement — before the subcommand — no longer parses.
    #[test]
    fn rejected_before_the_subcommand_and_on_non_readers() {
        for argv in [
            &[
                "omni-dev", "-C", "/tmp/r", "git", "commit", "message", "view",
            ][..],
            &["omni-dev", "--repo", "/tmp/r", "help-all"],
            &["omni-dev", "config", "models", "show", "-C", "/tmp/r"],
            &["omni-dev", "log", "-C", "/tmp/r"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
    }
}

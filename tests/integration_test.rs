#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use git2::{Repository, Signature};
use tempfile::TempDir;

use omni_dev::cli::git::AmendCommand;
use omni_dev::data::amendments::{Amendment, AmendmentFile};

/// Test setup that creates a temporary git repository with test commits
struct TestRepo {
    _temp_dir: TempDir,
    repo_path: PathBuf,
    repo: Repository,
    commits: Vec<git2::Oid>,
}

impl TestRepo {
    fn new() -> Result<Self> {
        // Use an absolute base so TempDir::path() (and therefore repo_path)
        // is absolute.  A relative "tmp" would make repo_path relative to
        // the process CWD at creation time; if another test changes CWD
        // concurrently, libgit2 can no longer locate the repository.
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        let temp_dir = {
            std::fs::create_dir_all(&tmp_root)?;
            tempfile::tempdir_in(&tmp_root)?
        };
        let repo_path = temp_dir.path().to_path_buf();

        // Initialize git repository
        let repo = Repository::init(&repo_path)?;

        // Configure git user for commits
        let mut config = repo.config()?;
        config.set_str("user.name", "Test User")?;
        config.set_str("user.email", "test@example.com")?;

        Ok(Self {
            _temp_dir: temp_dir,
            repo_path,
            repo,
            commits: Vec::new(),
        })
    }

    fn add_commit(&mut self, message: &str, content: &str) -> Result<git2::Oid> {
        // Create a test file
        let file_path = self.repo_path.join("test.txt");
        fs::write(&file_path, content)?;

        // Add file to index
        let mut index = self.repo.index()?;
        index.add_path(std::path::Path::new("test.txt"))?;
        index.write()?;

        // Create commit
        let signature = Signature::now("Test User", "test@example.com")?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;

        let parent_commit = if let Some(last_commit_id) = self.commits.last() {
            Some(self.repo.find_commit(*last_commit_id)?)
        } else {
            None
        };

        let parents: Vec<&git2::Commit> = if let Some(ref parent) = parent_commit {
            vec![parent]
        } else {
            vec![]
        };

        let commit_id = self.repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            message,
            &tree,
            &parents,
        )?;

        self.commits.push(commit_id);
        Ok(commit_id)
    }

    fn get_commit_hash(&self, index: usize) -> Option<String> {
        self.commits.get(index).map(git2::Oid::to_string)
    }

    fn create_amendment_file(&self, amendments: Vec<(usize, &str)>) -> Result<PathBuf> {
        let amendment_file = AmendmentFile {
            amendments: amendments
                .iter()
                .filter_map(|(index, message)| {
                    self.get_commit_hash(*index)
                        .map(|hash| Amendment::new(hash, (*message).to_string()))
                })
                .collect(),
        };

        // Create the amendments file outside the repository (so it doesn't show
        // up as untracked) under a name unique to this repo's tempdir, so tests
        // that share the parent `tmp/` directory can't overwrite each other's
        // file and race.
        let unique = self.repo_path.file_name().unwrap().to_string_lossy();
        let yaml_path = self
            .repo_path
            .parent()
            .unwrap()
            .join(format!("{unique}-amendments.yaml"));
        amendment_file.save_to_file(&yaml_path)?;
        Ok(yaml_path)
    }
}

#[test]
fn amend_command_with_temporary_repo() -> Result<()> {
    // Set up temporary repository with test commits
    let mut test_repo = TestRepo::new()?;

    // Add some test commits
    let _commit1 = test_repo.add_commit("Initial commit", "Hello, world!")?;
    let _commit2 = test_repo.add_commit("Add feature", "Hello, world!\nNew feature added.")?;
    let _commit3 =
        test_repo.add_commit("Fix bug", "Hello, world!\nNew feature added.\nBug fixed.")?;

    println!("Created test repository at: {:?}", test_repo.repo_path);
    println!("Commits created:");
    for (i, commit_id) in test_repo.commits.iter().enumerate() {
        println!("  {i}: {commit_id}");
    }

    // Create amendment file to modify HEAD commit (tested and working)
    let amendments = vec![(2, "Fix critical bug in the new feature")];

    let amendment_file_path = test_repo.create_amendment_file(amendments)?;
    println!("Created amendment file at: {amendment_file_path:?}");

    // The repository is injected explicitly via `--repo`, so the amend command
    // runs entirely against `test_repo.repo_path` — no process-CWD manipulation
    // and no shared mutex are needed.
    let amend_cmd = AmendCommand {
        yaml_file: amendment_file_path.to_string_lossy().to_string(),
    };
    amend_cmd
        .execute(Some(test_repo.repo_path.as_path()))
        .expect("Amend command should succeed");

    // Verify that amendments were actually made.  Use the absolute repo_path
    // directly so this does not depend on process CWD.
    let repo = Repository::open(&test_repo.repo_path)?;
    let head = repo.head()?.target().unwrap();
    let commit = repo.find_commit(head)?;
    let head_message = commit.message().unwrap_or("").trim();
    assert_eq!(
        head_message, "Fix critical bug in the new feature",
        "HEAD commit should have been amended with new message"
    );

    Ok(())
}

/// "No silent mix" guard: the clean-worktree preflight checks the INJECTED
/// repository, not the process CWD. Repo A has a dirty worktree, so amend must
/// bail citing A's uncommitted changes even though the process CWD (the
/// omni-dev checkout) is a different repository.
#[test]
fn amend_preflight_checks_injected_repo_worktree() -> Result<()> {
    let mut repo_a = TestRepo::new()?;
    repo_a.add_commit("a: initial", "content")?;
    let amendment_file = repo_a.create_amendment_file(vec![(0, "a: amended")])?;

    // Dirty the injected repo's worktree with an untracked file.
    fs::write(repo_a.repo_path.join("dirty.txt"), "uncommitted")?;

    let err = AmendCommand {
        yaml_file: amendment_file.to_string_lossy().to_string(),
    }
    .execute(Some(repo_a.repo_path.as_path()))
    .expect_err("amend must bail on the injected repo's dirty worktree");
    let msg = format!("{err:#}").to_lowercase();
    // Assert on the injected repo's specific untracked file: a regressed
    // CWD-anchored check would report a different (or no) file, so this can't
    // pass for the wrong reason.
    assert!(
        msg.contains("dirty.txt"),
        "expected dirty-worktree error naming the injected repo's file, got: {msg}"
    );

    Ok(())
}

#[test]
fn amendment_file_parsing() -> Result<()> {
    // Test that amendment file parsing works correctly
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root)?;
        tempfile::tempdir_in(&tmp_root)?
    };
    let yaml_path = temp_dir.path().join("test_amendments.yaml");

    // Create a test amendment file
    let test_yaml = r#"
amendments:
  - commit: "1234567890abcdef1234567890abcdef12345678"
    message: "Updated commit message 1"
  - commit: "abcdef1234567890abcdef1234567890abcdef12"
    message: "Updated commit message 2"
"#;

    fs::write(&yaml_path, test_yaml)?;

    // Test loading the amendment file
    let amendment_file = AmendmentFile::load_from_file(&yaml_path)?;
    assert_eq!(amendment_file.amendments.len(), 2);

    println!("✅ Amendment file parsing test passed");
    Ok(())
}

#[test]
fn amendment_validation() -> Result<()> {
    // Test amendment validation
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root)?;
        tempfile::tempdir_in(&tmp_root)?
    };
    let yaml_path = temp_dir.path().join("invalid_amendments.yaml");

    // Test with invalid commit hash (too short)
    let invalid_yaml = r#"
amendments:
  - commit: "12345"
    message: "Short hash should fail"
"#;

    fs::write(&yaml_path, invalid_yaml)?;

    // This should fail validation
    let result = AmendmentFile::load_from_file(&yaml_path);
    assert!(result.is_err());
    println!("✅ Amendment validation test passed - invalid hash rejected");

    Ok(())
}

#[test]
fn help_all_golden() -> Result<()> {
    // Capture the help-all output using the help generator directly
    use omni_dev::cli::help::HelpGenerator;

    let generator = HelpGenerator::new();
    let help_output = generator.generate_all_help()?;

    // Use insta for snapshot testing - this creates a .snap file with the expected output
    insta::assert_snapshot!("help_all_output", help_output);

    println!("✅ Golden test for help-all command completed");
    Ok(())
}

// ── CLI binary invocation tests ─────────────────────────────────

#[test]
fn binary_help_flag_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("--help")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("AI-powered git commit rewriter"));
}

#[test]
fn binary_version_flag_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("--version")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("omni-dev"));
}

#[test]
fn binary_unknown_command_fails() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("nonexistent")
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
}

#[test]
fn binary_config_models_show_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["config", "models", "show"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The models.yaml template should contain model definitions
    assert!(stdout.contains("claude"));
}

#[test]
fn binary_resources_show_jfm_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["resources", "show", "specs/jfm"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Byte-equality with the embedded const: catches any header drift or
    // accidental trailing newline added by `println!` vs `print!`.
    assert_eq!(stdout.as_ref(), omni_dev::resources::SPEC_JFM);
}

#[test]
fn binary_resources_show_accepts_omni_dev_uri_form() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["resources", "show", "omni-dev://specs/jfm"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.as_ref(), omni_dev::resources::SPEC_JFM);
}

#[test]
fn binary_resources_list_includes_specs_jfm() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["resources", "list"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.lines().any(|l| l == "specs/jfm"));
}

#[test]
fn binary_resources_show_unknown_id_fails() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["resources", "show", "specs/does-not-exist"])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown resource"), "stderr: {stderr}");
    assert!(stderr.contains("specs/jfm"), "stderr: {stderr}");
}

#[test]
fn binary_help_all_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .arg("help-all")
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("omni-dev git"));
    assert!(stdout.contains("omni-dev ai"));
}

#[test]
fn binary_completions_bash_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["completions", "bash"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("complete -F _omni-dev"),
        "missing bash completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_zsh_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["completions", "zsh"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#compdef omni-dev"),
        "missing zsh compdef marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_fish_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["completions", "fish"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("complete -c omni-dev"),
        "missing fish completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_powershell_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["completions", "powershell"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Register-ArgumentCompleter"),
        "missing PowerShell completion marker; stdout: {stdout}"
    );
}

#[test]
fn binary_completions_unknown_shell_fails() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["completions", "banana"])
        .output()
        .expect("failed to run binary");
    assert!(!output.status.success());
}

#[test]
fn binary_git_help_succeeds() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["git", "--help"])
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("commit"));
    assert!(stdout.contains("branch"));
}

#[test]
fn binary_commands_generate_in_temp_dir() {
    let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
    let temp_dir = {
        std::fs::create_dir_all(&tmp_root).ok();
        tempfile::tempdir_in(&tmp_root).unwrap()
    };
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_omni-dev"))
        .args(["commands", "generate", "all"])
        .current_dir(temp_dir.path())
        .output()
        .expect("failed to run binary");
    assert!(output.status.success());

    // Verify templates were written
    assert!(temp_dir
        .path()
        .join(".claude/commands/commit-twiddle.md")
        .exists());
    assert!(temp_dir
        .path()
        .join(".claude/commands/pr-create.md")
        .exists());
    assert!(temp_dir
        .path()
        .join(".claude/commands/pr-update.md")
        .exists());
}

// ── TestRepo builder coverage ───────────────────────────────────

#[test]
fn test_repo_multiple_commits() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.add_commit("first", "content1")?;
    repo.add_commit("second", "content2")?;
    repo.add_commit("third", "content3")?;

    assert_eq!(repo.commits.len(), 3);
    assert!(repo.get_commit_hash(0).is_some());
    assert!(repo.get_commit_hash(1).is_some());
    assert!(repo.get_commit_hash(2).is_some());
    assert!(repo.get_commit_hash(3).is_none());

    // Hashes should be 40-character hex
    let hash = repo.get_commit_hash(0).unwrap();
    assert_eq!(hash.len(), 40);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));

    Ok(())
}

#[test]
fn test_repo_create_amendment_file_roundtrip() -> Result<()> {
    let mut repo = TestRepo::new()?;
    repo.add_commit("initial", "hello")?;
    repo.add_commit("second", "world")?;

    // Verify commits were actually created before relying on them
    assert_eq!(repo.commits.len(), 2, "should have 2 commits");
    let hash0 = repo
        .get_commit_hash(0)
        .expect("commit 0 must exist after add_commit");
    let hash1 = repo
        .get_commit_hash(1)
        .expect("commit 1 must exist after add_commit");

    // Build the AmendmentFile directly (avoid filter_map silently dropping items)
    let amendment_file = AmendmentFile {
        amendments: vec![
            Amendment::new(hash0, "improved initial".to_string()),
            Amendment::new(hash1, "improved second".to_string()),
        ],
    };

    // Use a unique filename to avoid collisions with other tests
    let yaml_path = repo
        .repo_path
        .parent()
        .unwrap()
        .join("roundtrip_amendments.yaml");
    amendment_file.save_to_file(&yaml_path)?;

    let loaded = AmendmentFile::load_from_file(&yaml_path)?;
    assert_eq!(loaded.amendments.len(), 2);
    assert_eq!(loaded.amendments[0].message, "improved initial");
    assert_eq!(loaded.amendments[1].message, "improved second");

    Ok(())
}

// ── Async dispatch coverage ──────────────────────────────────────
//
// These tests exercise the async execute() dispatch chain introduced in #222.
// They run in the omni-dev repo itself (a valid git repository), so commands
// that require a git repo succeed without needing a temporary repo setup.

#[tokio::test]
async fn cli_execute_dispatches_git_commit_message_view() {
    use omni_dev::cli::git::{
        CommitCommand, CommitSubcommands, GitCommand, GitSubcommands, MessageCommand,
        MessageSubcommands, ViewCommand,
    };
    use omni_dev::cli::{Cli, Commands};

    let cli = Cli {
        ai_backend: None,
        claude_cli_allow_tools: false,
        claude_cli_allow_mcp: false,
        claude_cli_max_budget_usd: None,
        models_yaml: None,
        repo: None,
        command: Commands::Git(GitCommand {
            command: GitSubcommands::Commit(CommitCommand {
                command: CommitSubcommands::Message(MessageCommand {
                    command: MessageSubcommands::View(ViewCommand {
                        commit_range: Some("HEAD".to_string()),
                    }),
                }),
            }),
        }),
    };
    let _ = cli.execute().await;
}

#[tokio::test]
async fn cli_execute_dispatches_git_branch_info() {
    use omni_dev::cli::git::{
        BranchCommand, BranchSubcommands, GitCommand, GitSubcommands, InfoCommand,
    };
    use omni_dev::cli::{Cli, Commands};

    let cli = Cli {
        ai_backend: None,
        claude_cli_allow_tools: false,
        claude_cli_allow_mcp: false,
        claude_cli_max_budget_usd: None,
        models_yaml: None,
        repo: None,
        command: Commands::Git(GitCommand {
            command: GitSubcommands::Branch(BranchCommand {
                command: BranchSubcommands::Info(InfoCommand { base_branch: None }),
            }),
        }),
    };
    let _ = cli.execute().await;
}

#[tokio::test]
async fn cli_execute_dispatches_ai_chat() {
    use omni_dev::cli::ai::{AiCommand, AiSubcommands, ChatCommand};
    use omni_dev::cli::{Cli, Commands};

    let cli = Cli {
        ai_backend: None,
        claude_cli_allow_tools: false,
        claude_cli_allow_mcp: false,
        claude_cli_max_budget_usd: None,
        models_yaml: None,
        repo: None,
        command: Commands::Ai(AiCommand {
            command: AiSubcommands::Chat(ChatCommand { model: None }),
        }),
    };
    // Without API credentials this returns Err at the preflight check;
    // with credentials it returns Err in a non-TTY environment.
    // Either way the async dispatch chain is exercised.
    let _ = cli.execute().await;
}

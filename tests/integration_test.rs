#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::env;
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
        // Create temporary directory
        let temp_dir = {
            std::fs::create_dir_all("tmp")?;
            tempfile::tempdir_in("tmp")?
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

        // Create the amendments file outside the repository to avoid it showing up as untracked
        let yaml_path = self.repo_path.parent().unwrap().join("amendments.yaml");
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

    // Change to the test repository directory
    let original_dir = env::current_dir()?;
    env::set_current_dir(&test_repo.repo_path)?;

    // Test the amend command
    let result = std::panic::catch_unwind(|| {
        let amend_cmd = AmendCommand {
            yaml_file: amendment_file_path.to_string_lossy().to_string(),
        };

        println!("Testing amend command...");
        let result = amend_cmd.execute();
        println!("Amend command result: {result:?}");
        result
    });

    // Restore original directory
    env::set_current_dir(&original_dir)?;

    match result {
        Ok(cmd_result) => {
            println!("Amend command completed: {cmd_result:?}");

            // The implementation should now actually work
            assert!(cmd_result.is_ok(), "Amend command should succeed");

            // Verify that amendments were actually made
            env::set_current_dir(&test_repo.repo_path)?;
            let repo = Repository::open(".")?;
            let head = repo.head()?.target().unwrap();
            let commit = repo.find_commit(head)?;
            println!(
                "Current HEAD commit message after amendment: {}",
                commit.message().unwrap_or("")
            );

            // Restore directory again
            env::set_current_dir(&original_dir)?;

            // The HEAD commit message should have been amended
            let head_message = commit.message().unwrap_or("").trim();
            assert_eq!(
                head_message, "Fix critical bug in the new feature",
                "HEAD commit should have been amended with new message"
            );

            println!("✅ Test passed: Amend command successfully amended the commit message");
        }
        Err(e) => {
            println!("❌ Amend command panicked: {e:?}");
            panic!("Amend command should not panic");
        }
    }

    Ok(())
}

#[test]
fn amendment_file_parsing() -> Result<()> {
    // Test that amendment file parsing works correctly
    let temp_dir = {
        std::fs::create_dir_all("tmp")?;
        tempfile::tempdir_in("tmp")?
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
    let temp_dir = {
        std::fs::create_dir_all("tmp")?;
        tempfile::tempdir_in("tmp")?
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
    assert!(stdout.contains("comprehensive development toolkit"));
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
    let temp_dir = {
        std::fs::create_dir_all("tmp").ok();
        tempfile::tempdir_in("tmp").unwrap()
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
    use omni_dev::cli::ai::{AiCommand, AiSubcommand, ChatCommand};
    use omni_dev::cli::{Cli, Commands};

    let cli = Cli {
        command: Commands::Ai(AiCommand {
            command: AiSubcommand::Chat(ChatCommand { model: None }),
        }),
    };
    // Without API credentials this returns Err at the preflight check;
    // with credentials it returns Err in a non-TTY environment.
    // Either way the async dispatch chain is exercised.
    let _ = cli.execute().await;
}

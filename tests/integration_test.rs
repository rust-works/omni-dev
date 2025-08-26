use anyhow::Result;
use git2::{Repository, Signature};
use omni_dev::cli::git::AmendCommand;
use omni_dev::data::amendments::{Amendment, AmendmentFile};
use std::env;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

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
        let temp_dir = tempfile::tempdir()?;
        let repo_path = temp_dir.path().to_path_buf();

        // Initialize git repository
        let repo = Repository::init(&repo_path)?;

        // Configure git user for commits
        let mut config = repo.config()?;
        config.set_str("user.name", "Test User")?;
        config.set_str("user.email", "test@example.com")?;

        Ok(TestRepo {
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
        self.commits.get(index).map(|oid| oid.to_string())
    }

    fn create_amendment_file(&self, amendments: Vec<(usize, &str)>) -> Result<PathBuf> {
        let amendment_file = AmendmentFile {
            amendments: amendments
                .iter()
                .filter_map(|(index, message)| {
                    self.get_commit_hash(*index)
                        .map(|hash| Amendment::new(hash, message.to_string()))
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
fn test_amend_command_with_temporary_repo() -> Result<()> {
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
        println!("  {}: {}", i, commit_id);
    }

    // Create amendment file to modify HEAD commit (tested and working)
    let amendments = vec![(2, "Fix critical bug in the new feature")];

    let amendment_file_path = test_repo.create_amendment_file(amendments)?;
    println!("Created amendment file at: {:?}", amendment_file_path);

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
        println!("Amend command result: {:?}", result);
        result
    });

    // Restore original directory
    env::set_current_dir(&original_dir)?;

    match result {
        Ok(cmd_result) => {
            println!("Amend command completed: {:?}", cmd_result);

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
            println!("❌ Amend command panicked: {:?}", e);
            panic!("Amend command should not panic");
        }
    }

    Ok(())
}

#[test]
fn test_amendment_file_parsing() -> Result<()> {
    // Test that amendment file parsing works correctly
    let temp_dir = tempfile::tempdir()?;
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
fn test_amendment_validation() -> Result<()> {
    // Test amendment validation
    let temp_dir = tempfile::tempdir()?;
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
fn test_help_all_golden() -> Result<()> {
    // Capture the help-all output using the help generator directly
    use omni_dev::cli::help::HelpGenerator;

    let generator = HelpGenerator::new();
    let help_output = generator.generate_all_help()?;

    // Use insta for snapshot testing - this creates a .snap file with the expected output
    insta::assert_snapshot!("help_all_output", help_output);

    println!("✅ Golden test for help-all command completed");
    Ok(())
}

//! Project context discovery system.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tracing::debug;

use crate::data::context::{
    Ecosystem, FeatureContext, ProjectContext, ProjectConventions, ScopeDefinition,
    ScopeRequirements,
};

/// Resolves configuration file path with local override support and home fallback.
///
/// Priority:
/// 1. {dir}/local/{filename} (local override)
/// 2. {dir}/{filename} (shared project config)
/// 3. $HOME/.omni-dev/{filename} (global user config)
pub fn resolve_config_file(dir: &Path, filename: &str) -> PathBuf {
    let local_path = dir.join("local").join(filename);
    if local_path.exists() {
        return local_path;
    }

    let project_path = dir.join(filename);
    if project_path.exists() {
        return project_path;
    }

    // Check home directory fallback
    if let Ok(home_dir) = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory")) {
        let home_path = home_dir.join(".omni-dev").join(filename);
        if home_path.exists() {
            return home_path;
        }
    }

    // Return project path as default (even if it doesn't exist)
    project_path
}

/// Loads project scopes from config files, merging ecosystem defaults.
///
/// Resolves `scopes.yaml` via the standard config priority (local → project → home),
/// then detects the project ecosystem and merges default scopes for that ecosystem.
pub fn load_project_scopes(context_dir: &Path, repo_path: &Path) -> Vec<ScopeDefinition> {
    let scopes_path = resolve_config_file(context_dir, "scopes.yaml");
    let mut scopes = if scopes_path.exists() {
        let scopes_yaml = match fs::read_to_string(&scopes_path) {
            Ok(content) => content,
            Err(e) => {
                tracing::warn!("Cannot read scopes file {}: {e}", scopes_path.display());
                return vec![];
            }
        };
        match serde_yaml::from_str::<ScopesConfig>(&scopes_yaml) {
            Ok(config) => config.scopes,
            Err(e) => {
                tracing::warn!(
                    "Ignoring malformed scopes file {}: {e}",
                    scopes_path.display()
                );
                vec![]
            }
        }
    } else {
        vec![]
    };

    merge_ecosystem_scopes(&mut scopes, repo_path);
    scopes
}

/// Merges ecosystem-detected default scopes into the given scope list.
///
/// Detects the project ecosystem from marker files (Cargo.toml, package.json, etc.)
/// and adds default scopes for that ecosystem, skipping any that already exist by name.
fn merge_ecosystem_scopes(scopes: &mut Vec<ScopeDefinition>, repo_path: &Path) {
    let ecosystem_scopes: Vec<(&str, &str, Vec<&str>)> = if repo_path.join("Cargo.toml").exists() {
        vec![
            (
                "cargo",
                "Cargo.toml and dependency management",
                vec!["Cargo.toml", "Cargo.lock"],
            ),
            (
                "lib",
                "Library code and public API",
                vec!["src/lib.rs", "src/**"],
            ),
            (
                "cli",
                "Command-line interface",
                vec!["src/main.rs", "src/cli/**"],
            ),
            (
                "core",
                "Core application logic",
                vec!["src/core/**", "src/lib/**"],
            ),
            ("test", "Test code", vec!["tests/**", "src/**/test*"]),
            (
                "docs",
                "Documentation",
                vec!["docs/**", "README.md", "**/*.md"],
            ),
            (
                "ci",
                "Continuous integration",
                vec![".github/**", ".gitlab-ci.yml"],
            ),
        ]
    } else if repo_path.join("package.json").exists() {
        vec![
            (
                "deps",
                "Dependencies and package.json",
                vec!["package.json", "package-lock.json"],
            ),
            (
                "config",
                "Configuration files",
                vec!["*.config.js", "*.config.json", ".env*"],
            ),
            (
                "build",
                "Build system and tooling",
                vec!["webpack.config.js", "rollup.config.js"],
            ),
            (
                "test",
                "Test files",
                vec!["test/**", "tests/**", "**/*.test.js"],
            ),
            (
                "docs",
                "Documentation",
                vec!["docs/**", "README.md", "**/*.md"],
            ),
        ]
    } else if repo_path.join("pyproject.toml").exists()
        || repo_path.join("requirements.txt").exists()
    {
        vec![
            (
                "deps",
                "Dependencies and requirements",
                vec!["requirements.txt", "pyproject.toml", "setup.py"],
            ),
            (
                "config",
                "Configuration files",
                vec!["*.ini", "*.cfg", "*.toml"],
            ),
            (
                "test",
                "Test files",
                vec!["test/**", "tests/**", "**/*_test.py"],
            ),
            (
                "docs",
                "Documentation",
                vec!["docs/**", "README.md", "**/*.md", "**/*.rst"],
            ),
        ]
    } else if repo_path.join("go.mod").exists() {
        vec![
            (
                "mod",
                "Go modules and dependencies",
                vec!["go.mod", "go.sum"],
            ),
            ("cmd", "Command-line applications", vec!["cmd/**"]),
            ("pkg", "Library packages", vec!["pkg/**"]),
            ("internal", "Internal packages", vec!["internal/**"]),
            ("test", "Test files", vec!["**/*_test.go"]),
            (
                "docs",
                "Documentation",
                vec!["docs/**", "README.md", "**/*.md"],
            ),
        ]
    } else if repo_path.join("pom.xml").exists() || repo_path.join("build.gradle").exists() {
        vec![
            (
                "build",
                "Build system",
                vec!["pom.xml", "build.gradle", "build.gradle.kts"],
            ),
            (
                "config",
                "Configuration",
                vec!["src/main/resources/**", "application.properties"],
            ),
            ("test", "Test files", vec!["src/test/**"]),
            (
                "docs",
                "Documentation",
                vec!["docs/**", "README.md", "**/*.md"],
            ),
        ]
    } else {
        vec![]
    };

    for (name, description, patterns) in ecosystem_scopes {
        if !scopes.iter().any(|s| s.name == name) {
            scopes.push(ScopeDefinition {
                name: name.to_string(),
                description: description.to_string(),
                examples: vec![],
                file_patterns: patterns.into_iter().map(String::from).collect(),
            });
        }
    }
}

/// Project context discovery system.
pub struct ProjectDiscovery {
    repo_path: PathBuf,
    context_dir: PathBuf,
}

impl ProjectDiscovery {
    /// Creates a new project discovery instance.
    pub fn new(repo_path: PathBuf, context_dir: PathBuf) -> Self {
        Self {
            repo_path,
            context_dir,
        }
    }

    /// Discovers all project context.
    pub fn discover(&self) -> Result<ProjectContext> {
        let mut context = ProjectContext::default();

        // 1. Check custom context directory (highest priority)
        let context_dir_path = if self.context_dir.is_absolute() {
            self.context_dir.clone()
        } else {
            self.repo_path.join(&self.context_dir)
        };
        debug!(
            context_dir = ?context_dir_path,
            exists = context_dir_path.exists(),
            "Looking for context directory"
        );
        debug!("Loading omni-dev config");
        self.load_omni_dev_config(&mut context, &context_dir_path)?;
        debug!("Config loading completed");

        // 2. Standard git configuration files
        self.load_git_config(&mut context)?;

        // 3. Parse project documentation
        self.parse_documentation(&mut context)?;

        // 4. Detect ecosystem conventions
        self.detect_ecosystem(&mut context)?;

        Ok(context)
    }

    /// Loads configuration from .omni-dev/ directory with local override support.
    fn load_omni_dev_config(&self, context: &mut ProjectContext, dir: &Path) -> Result<()> {
        // Load commit guidelines (with local override)
        let guidelines_path = resolve_config_file(dir, "commit-guidelines.md");
        debug!(
            path = ?guidelines_path,
            exists = guidelines_path.exists(),
            "Checking for commit guidelines"
        );
        if guidelines_path.exists() {
            let content = fs::read_to_string(&guidelines_path)?;
            debug!(bytes = content.len(), "Loaded commit guidelines");
            context.commit_guidelines = Some(content);
        } else {
            debug!("No commit guidelines file found");
        }

        // Load PR guidelines (with local override)
        let pr_guidelines_path = resolve_config_file(dir, "pr-guidelines.md");
        debug!(
            path = ?pr_guidelines_path,
            exists = pr_guidelines_path.exists(),
            "Checking for PR guidelines"
        );
        if pr_guidelines_path.exists() {
            let content = fs::read_to_string(&pr_guidelines_path)?;
            debug!(bytes = content.len(), "Loaded PR guidelines");
            context.pr_guidelines = Some(content);
        } else {
            debug!("No PR guidelines file found");
        }

        // Load scopes configuration (with local override)
        let scopes_path = resolve_config_file(dir, "scopes.yaml");
        if scopes_path.exists() {
            let scopes_yaml = fs::read_to_string(&scopes_path)?;
            match serde_yaml::from_str::<ScopesConfig>(&scopes_yaml) {
                Ok(scopes_config) => {
                    context.valid_scopes = scopes_config.scopes;
                }
                Err(e) => {
                    tracing::warn!(
                        "Ignoring malformed scopes file {}: {e}",
                        scopes_path.display()
                    );
                }
            }
        }

        // Load feature contexts (check both local and standard directories)
        let local_contexts_dir = dir.join("local").join("context").join("feature-contexts");
        let contexts_dir = dir.join("context").join("feature-contexts");

        // Load standard feature contexts first
        if contexts_dir.exists() {
            self.load_feature_contexts(context, &contexts_dir)?;
        }

        // Load local feature contexts (will override if same name)
        if local_contexts_dir.exists() {
            self.load_feature_contexts(context, &local_contexts_dir)?;
        }

        Ok(())
    }

    /// Loads git configuration files.
    fn load_git_config(&self, _context: &mut ProjectContext) -> Result<()> {
        // Git configuration loading can be extended here if needed
        Ok(())
    }

    /// Parses project documentation for conventions.
    fn parse_documentation(&self, context: &mut ProjectContext) -> Result<()> {
        // Parse CONTRIBUTING.md
        let contributing_path = self.repo_path.join("CONTRIBUTING.md");
        if contributing_path.exists() {
            let content = fs::read_to_string(contributing_path)?;
            context.project_conventions = self.parse_contributing_conventions(&content)?;
        }

        // Parse README.md for additional conventions
        let readme_path = self.repo_path.join("README.md");
        if readme_path.exists() {
            let content = fs::read_to_string(readme_path)?;
            self.parse_readme_conventions(context, &content)?;
        }

        Ok(())
    }

    /// Detects project ecosystem and applies conventions.
    fn detect_ecosystem(&self, context: &mut ProjectContext) -> Result<()> {
        context.ecosystem = if self.repo_path.join("Cargo.toml").exists() {
            Ecosystem::Rust
        } else if self.repo_path.join("package.json").exists() {
            Ecosystem::Node
        } else if self.repo_path.join("pyproject.toml").exists()
            || self.repo_path.join("requirements.txt").exists()
        {
            Ecosystem::Python
        } else if self.repo_path.join("go.mod").exists() {
            Ecosystem::Go
        } else if self.repo_path.join("pom.xml").exists()
            || self.repo_path.join("build.gradle").exists()
        {
            Ecosystem::Java
        } else {
            Ecosystem::Generic
        };

        merge_ecosystem_scopes(&mut context.valid_scopes, &self.repo_path);

        Ok(())
    }

    /// Loads feature contexts from a directory.
    fn load_feature_contexts(
        &self,
        context: &mut ProjectContext,
        contexts_dir: &Path,
    ) -> Result<()> {
        let entries = match fs::read_dir(contexts_dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    "Cannot read feature contexts dir {}: {e}",
                    contexts_dir.display()
                );
                return Ok(());
            }
        };
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with(".yaml") || name.ends_with(".yml") {
                    let content = fs::read_to_string(entry.path())?;
                    match serde_yaml::from_str::<FeatureContext>(&content) {
                        Ok(feature_context) => {
                            let feature_name = name
                                .trim_end_matches(".yaml")
                                .trim_end_matches(".yml")
                                .to_string();
                            context
                                .feature_contexts
                                .insert(feature_name, feature_context);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "Ignoring malformed feature context {}: {e}",
                                entry.path().display()
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Parses CONTRIBUTING.md for conventions.
    fn parse_contributing_conventions(&self, content: &str) -> Result<ProjectConventions> {
        let mut conventions = ProjectConventions::default();

        // Look for commit message sections
        let lines: Vec<&str> = content.lines().collect();
        let mut in_commit_section = false;

        for (i, line) in lines.iter().enumerate() {
            let line_lower = line.to_lowercase();

            // Detect commit message sections
            if line_lower.contains("commit")
                && (line_lower.contains("message") || line_lower.contains("format"))
            {
                in_commit_section = true;
                continue;
            }

            // End commit section if we hit another header
            if in_commit_section && line.starts_with('#') && !line_lower.contains("commit") {
                in_commit_section = false;
            }

            if in_commit_section {
                // Extract commit format examples
                if line.contains("type(scope):") || line.contains("<type>(<scope>):") {
                    conventions.commit_format = Some("type(scope): description".to_string());
                }

                // Extract required trailers
                if line_lower.contains("signed-off-by") {
                    conventions
                        .required_trailers
                        .push("Signed-off-by".to_string());
                }

                if line_lower.contains("fixes") && line_lower.contains('#') {
                    conventions.required_trailers.push("Fixes".to_string());
                }

                // Extract preferred types
                if line.contains("feat") || line.contains("fix") || line.contains("docs") {
                    let types = extract_commit_types(line);
                    conventions.preferred_types.extend(types);
                }

                // Look ahead for scope examples
                if line_lower.contains("scope") && i + 1 < lines.len() {
                    let scope_requirements = self.extract_scope_requirements(&lines[i..]);
                    conventions.scope_requirements = scope_requirements;
                }
            }
        }

        Ok(conventions)
    }

    /// Parses README.md for additional conventions.
    fn parse_readme_conventions(&self, context: &mut ProjectContext, content: &str) -> Result<()> {
        // Look for development or contribution sections
        let lines: Vec<&str> = content.lines().collect();

        for line in lines {
            let _line_lower = line.to_lowercase();

            // Extract additional scope information from project structure
            if line.contains("src/") || line.contains("lib/") {
                // Try to extract scope information from directory structure mentions
                if let Some(scope) = extract_scope_from_structure(line) {
                    context.valid_scopes.push(ScopeDefinition {
                        name: scope.clone(),
                        description: format!("{scope} related changes"),
                        examples: vec![],
                        file_patterns: vec![format!("{}/**", scope)],
                    });
                }
            }
        }

        Ok(())
    }

    /// Extracts scope requirements from contributing documentation.
    fn extract_scope_requirements(&self, lines: &[&str]) -> ScopeRequirements {
        let mut requirements = ScopeRequirements::default();

        for line in lines.iter().take(10) {
            // Stop at next major section
            if line.starts_with("##") {
                break;
            }

            let line_lower = line.to_lowercase();

            if line_lower.contains("required") || line_lower.contains("must") {
                requirements.required = true;
            }

            // Extract scope examples
            if line.contains(':')
                && (line.contains("auth") || line.contains("api") || line.contains("ui"))
            {
                let scopes = extract_scopes_from_examples(line);
                requirements.valid_scopes.extend(scopes);
            }
        }

        requirements
    }
}

/// Configuration structure for scopes.yaml.
#[derive(serde::Deserialize)]
struct ScopesConfig {
    scopes: Vec<ScopeDefinition>,
}

/// Extracts commit types from a line.
fn extract_commit_types(line: &str) -> Vec<String> {
    let mut types = Vec::new();
    let common_types = [
        "feat", "fix", "docs", "style", "refactor", "test", "chore", "ci", "build", "perf",
    ];

    for &type_str in &common_types {
        if line.to_lowercase().contains(type_str) {
            types.push(type_str.to_string());
        }
    }

    types
}

/// Extracts a scope from a project structure description.
fn extract_scope_from_structure(line: &str) -> Option<String> {
    // Look for patterns like "src/auth/", "lib/config/", etc.
    if let Some(start) = line.find("src/") {
        let after_src = &line[start + 4..];
        if let Some(end) = after_src.find('/') {
            return Some(after_src[..end].to_string());
        }
    }

    None
}

/// Extracts scopes from examples in documentation.
fn extract_scopes_from_examples(line: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let common_scopes = ["auth", "api", "ui", "db", "config", "core", "cli", "web"];

    for &scope in &common_scopes {
        if line.to_lowercase().contains(scope) {
            scopes.push(scope.to_string());
        }
    }

    scopes
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── resolve_config_file ──────────────────────────────────────────

    #[test]
    fn local_override_wins() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let base = dir.path();

        // Create both local and project files
        std::fs::create_dir_all(base.join("local"))?;
        std::fs::write(base.join("local").join("scopes.yaml"), "local")?;
        std::fs::write(base.join("scopes.yaml"), "project")?;

        let resolved = resolve_config_file(base, "scopes.yaml");
        assert_eq!(resolved, base.join("local").join("scopes.yaml"));
        Ok(())
    }

    #[test]
    fn project_fallback() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let base = dir.path();

        // Create only project-level file (no local/)
        std::fs::write(base.join("scopes.yaml"), "project")?;

        let resolved = resolve_config_file(base, "scopes.yaml");
        assert_eq!(resolved, base.join("scopes.yaml"));
        Ok(())
    }

    #[test]
    fn returns_default_when_nothing_exists() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();

        let resolved = resolve_config_file(base, "scopes.yaml");
        // When no local or project file exists, it either returns:
        // - the home directory path if $HOME/.omni-dev/scopes.yaml exists
        // - the project path as fallback default
        // Either way, the resolved path should NOT be the local override path.
        assert_ne!(resolved, base.join("local").join("scopes.yaml"));
    }

    // ── merge_ecosystem_scopes ───────────────────────────────────────

    #[test]
    fn rust_ecosystem_detected() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;

        let mut scopes = vec![];
        merge_ecosystem_scopes(&mut scopes, dir.path());

        let names: Vec<&str> = scopes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"cargo"), "missing 'cargo' scope");
        assert!(names.contains(&"cli"), "missing 'cli' scope");
        assert!(names.contains(&"core"), "missing 'core' scope");
        assert!(names.contains(&"test"), "missing 'test' scope");
        assert!(names.contains(&"docs"), "missing 'docs' scope");
        assert!(names.contains(&"ci"), "missing 'ci' scope");
        Ok(())
    }

    #[test]
    fn node_ecosystem_detected() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("package.json"), "{}")?;

        let mut scopes = vec![];
        merge_ecosystem_scopes(&mut scopes, dir.path());

        let names: Vec<&str> = scopes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"deps"), "missing 'deps' scope");
        assert!(names.contains(&"config"), "missing 'config' scope");
        Ok(())
    }

    #[test]
    fn go_ecosystem_detected() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("go.mod"), "module example")?;

        let mut scopes = vec![];
        merge_ecosystem_scopes(&mut scopes, dir.path());

        let names: Vec<&str> = scopes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"mod"), "missing 'mod' scope");
        assert!(names.contains(&"cmd"), "missing 'cmd' scope");
        assert!(names.contains(&"pkg"), "missing 'pkg' scope");
        Ok(())
    }

    #[test]
    fn existing_scope_not_overridden() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;

        let mut scopes = vec![ScopeDefinition {
            name: "cli".to_string(),
            description: "Custom CLI scope".to_string(),
            examples: vec![],
            file_patterns: vec!["custom/**".to_string()],
        }];
        merge_ecosystem_scopes(&mut scopes, dir.path());

        // The custom "cli" scope should be preserved, not replaced
        let cli_scope = scopes.iter().find(|s| s.name == "cli").unwrap();
        assert_eq!(cli_scope.description, "Custom CLI scope");
        assert_eq!(cli_scope.file_patterns, vec!["custom/**"]);
        Ok(())
    }

    #[test]
    fn no_marker_files_produces_empty() {
        let dir = TempDir::new().unwrap();
        let mut scopes = vec![];
        merge_ecosystem_scopes(&mut scopes, dir.path());
        assert!(scopes.is_empty());
    }

    // ── load_project_scopes ──────────────────────────────────────────

    #[test]
    fn load_project_scopes_with_yaml() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir)?;

        let scopes_yaml = r#"
scopes:
  - name: custom
    description: Custom scope
    examples: []
    file_patterns:
      - "src/custom/**"
"#;
        std::fs::write(config_dir.join("scopes.yaml"), scopes_yaml)?;

        // Also create Cargo.toml so ecosystem scopes get merged
        std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;

        let scopes = load_project_scopes(&config_dir, dir.path());
        let names: Vec<&str> = scopes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"custom"), "missing custom scope");
        // Ecosystem scopes should also be merged
        assert!(names.contains(&"cargo"), "missing ecosystem scope");
        Ok(())
    }

    #[test]
    fn load_project_scopes_no_file() -> anyhow::Result<()> {
        let dir = TempDir::new()?;
        std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;

        let scopes = load_project_scopes(dir.path(), dir.path());
        // Should still get ecosystem defaults
        assert!(!scopes.is_empty());
        Ok(())
    }

    // ── Helper functions ─────────────────────────────────────────────

    #[test]
    fn extract_scope_from_structure_src() {
        assert_eq!(
            extract_scope_from_structure("- `src/auth/` - Authentication"),
            Some("auth".to_string())
        );
    }

    #[test]
    fn extract_scope_from_structure_no_match() {
        assert_eq!(extract_scope_from_structure("No source paths here"), None);
    }

    #[test]
    fn extract_commit_types_from_line() {
        let types = extract_commit_types("feat, fix, docs, test");
        assert!(types.contains(&"feat".to_string()));
        assert!(types.contains(&"fix".to_string()));
        assert!(types.contains(&"docs".to_string()));
        assert!(types.contains(&"test".to_string()));
    }

    #[test]
    fn extract_commit_types_empty_line() {
        let types = extract_commit_types("no types here");
        assert!(types.is_empty());
    }
}

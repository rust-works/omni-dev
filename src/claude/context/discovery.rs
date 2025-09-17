//! Project context discovery system

use crate::data::context::{
    Ecosystem, FeatureContext, ProjectContext, ProjectConventions, ScopeDefinition,
    ScopeRequirements,
};
use anyhow::Result;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Project context discovery system
pub struct ProjectDiscovery {
    repo_path: PathBuf,
    context_dir: PathBuf,
}

impl ProjectDiscovery {
    /// Create a new project discovery instance
    pub fn new(repo_path: PathBuf, context_dir: PathBuf) -> Self {
        Self {
            repo_path,
            context_dir,
        }
    }

    /// Discover all project context
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
        if context_dir_path.exists() {
            debug!("Loading omni-dev config");
            self.load_omni_dev_config(&mut context, &context_dir_path)?;
            debug!("Config loading completed");
        }

        // 2. Standard git configuration files
        self.load_git_config(&mut context)?;

        // 3. Parse project documentation
        self.parse_documentation(&mut context)?;

        // 4. Detect ecosystem conventions
        self.detect_ecosystem(&mut context)?;

        Ok(context)
    }

    /// Load configuration from .omni-dev/ directory with local override support
    fn load_omni_dev_config(&self, context: &mut ProjectContext, dir: &Path) -> Result<()> {
        // Load commit guidelines (with local override)
        let guidelines_path = self.resolve_config_file(dir, "commit-guidelines.md");
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

        // Load commit template (with local override)
        let template_path = self.resolve_config_file(dir, "commit-template.txt");
        if template_path.exists() {
            context.commit_template = Some(fs::read_to_string(template_path)?);
        }

        // Load scopes configuration (with local override)
        let scopes_path = self.resolve_config_file(dir, "scopes.yaml");
        if scopes_path.exists() {
            let scopes_yaml = fs::read_to_string(scopes_path)?;
            if let Ok(scopes_config) = serde_yaml::from_str::<ScopesConfig>(&scopes_yaml) {
                context.valid_scopes = scopes_config.scopes;
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

    /// Resolve configuration file path with local override support and home fallback
    ///
    /// Priority:
    /// 1. .omni-dev/local/{filename} (local override)
    /// 2. .omni-dev/{filename} (shared project config)
    /// 3. $HOME/.omni-dev/{filename} (global user config)
    fn resolve_config_file(&self, dir: &Path, filename: &str) -> PathBuf {
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

    /// Load git configuration files
    fn load_git_config(&self, context: &mut ProjectContext) -> Result<()> {
        // Check for .gitmessage template
        let gitmessage_path = self.repo_path.join(".gitmessage");
        if gitmessage_path.exists() && context.commit_template.is_none() {
            context.commit_template = Some(fs::read_to_string(gitmessage_path)?);
        }

        Ok(())
    }

    /// Parse project documentation for conventions
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

    /// Detect project ecosystem and apply conventions
    fn detect_ecosystem(&self, context: &mut ProjectContext) -> Result<()> {
        context.ecosystem = if self.repo_path.join("Cargo.toml").exists() {
            self.apply_rust_conventions(context)?;
            Ecosystem::Rust
        } else if self.repo_path.join("package.json").exists() {
            self.apply_node_conventions(context)?;
            Ecosystem::Node
        } else if self.repo_path.join("pyproject.toml").exists()
            || self.repo_path.join("requirements.txt").exists()
        {
            self.apply_python_conventions(context)?;
            Ecosystem::Python
        } else if self.repo_path.join("go.mod").exists() {
            self.apply_go_conventions(context)?;
            Ecosystem::Go
        } else if self.repo_path.join("pom.xml").exists()
            || self.repo_path.join("build.gradle").exists()
        {
            self.apply_java_conventions(context)?;
            Ecosystem::Java
        } else {
            Ecosystem::Generic
        };

        Ok(())
    }

    /// Load feature contexts from directory
    fn load_feature_contexts(
        &self,
        context: &mut ProjectContext,
        contexts_dir: &Path,
    ) -> Result<()> {
        if let Ok(entries) = fs::read_dir(contexts_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".yaml") || name.ends_with(".yml") {
                        let content = fs::read_to_string(entry.path())?;
                        if let Ok(feature_context) =
                            serde_yaml::from_str::<FeatureContext>(&content)
                        {
                            let feature_name = name
                                .trim_end_matches(".yaml")
                                .trim_end_matches(".yml")
                                .to_string();
                            context
                                .feature_contexts
                                .insert(feature_name, feature_context);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Parse CONTRIBUTING.md for conventions
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

                if line_lower.contains("fixes") && line_lower.contains("#") {
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

    /// Parse README.md for additional conventions
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
                        description: format!("{} related changes", scope),
                        examples: vec![],
                        file_patterns: vec![format!("{}/**", scope)],
                    });
                }
            }
        }

        Ok(())
    }

    /// Extract scope requirements from contributing documentation
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
            if line.contains(":")
                && (line.contains("auth") || line.contains("api") || line.contains("ui"))
            {
                let scopes = extract_scopes_from_examples(line);
                requirements.valid_scopes.extend(scopes);
            }
        }

        requirements
    }

    /// Apply Rust ecosystem conventions
    fn apply_rust_conventions(&self, context: &mut ProjectContext) -> Result<()> {
        // Add common Rust scopes if not already defined
        let rust_scopes = vec![
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
        ];

        for (name, description, patterns) in rust_scopes {
            if !context.valid_scopes.iter().any(|s| s.name == name) {
                context.valid_scopes.push(ScopeDefinition {
                    name: name.to_string(),
                    description: description.to_string(),
                    examples: vec![],
                    file_patterns: patterns.into_iter().map(String::from).collect(),
                });
            }
        }

        Ok(())
    }

    /// Apply Node.js ecosystem conventions
    fn apply_node_conventions(&self, context: &mut ProjectContext) -> Result<()> {
        let node_scopes = vec![
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
        ];

        for (name, description, patterns) in node_scopes {
            if !context.valid_scopes.iter().any(|s| s.name == name) {
                context.valid_scopes.push(ScopeDefinition {
                    name: name.to_string(),
                    description: description.to_string(),
                    examples: vec![],
                    file_patterns: patterns.into_iter().map(String::from).collect(),
                });
            }
        }

        Ok(())
    }

    /// Apply Python ecosystem conventions
    fn apply_python_conventions(&self, context: &mut ProjectContext) -> Result<()> {
        let python_scopes = vec![
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
        ];

        for (name, description, patterns) in python_scopes {
            if !context.valid_scopes.iter().any(|s| s.name == name) {
                context.valid_scopes.push(ScopeDefinition {
                    name: name.to_string(),
                    description: description.to_string(),
                    examples: vec![],
                    file_patterns: patterns.into_iter().map(String::from).collect(),
                });
            }
        }

        Ok(())
    }

    /// Apply Go ecosystem conventions
    fn apply_go_conventions(&self, context: &mut ProjectContext) -> Result<()> {
        let go_scopes = vec![
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
        ];

        for (name, description, patterns) in go_scopes {
            if !context.valid_scopes.iter().any(|s| s.name == name) {
                context.valid_scopes.push(ScopeDefinition {
                    name: name.to_string(),
                    description: description.to_string(),
                    examples: vec![],
                    file_patterns: patterns.into_iter().map(String::from).collect(),
                });
            }
        }

        Ok(())
    }

    /// Apply Java ecosystem conventions
    fn apply_java_conventions(&self, context: &mut ProjectContext) -> Result<()> {
        let java_scopes = vec![
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
        ];

        for (name, description, patterns) in java_scopes {
            if !context.valid_scopes.iter().any(|s| s.name == name) {
                context.valid_scopes.push(ScopeDefinition {
                    name: name.to_string(),
                    description: description.to_string(),
                    examples: vec![],
                    file_patterns: patterns.into_iter().map(String::from).collect(),
                });
            }
        }

        Ok(())
    }
}

/// Configuration structure for scopes.yaml
#[derive(serde::Deserialize)]
struct ScopesConfig {
    scopes: Vec<ScopeDefinition>,
}

/// Extract commit types from a line
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

/// Extract scope from project structure description
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

/// Extract scopes from examples in documentation
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

//! File-based context analysis and architectural understanding.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::data::context::{
    ArchitecturalLayer, ChangeImpact, FileContext, FilePurpose, ProjectSignificance,
};
use crate::git::CommitInfo;

/// File context analyzer.
pub struct FileAnalyzer;

impl FileAnalyzer {
    /// Analyzes a file and determines its context within the project.
    pub fn analyze_file(path: &Path, change_type: &str) -> FileContext {
        let file_purpose = determine_file_purpose(path);
        let architectural_layer = determine_architectural_layer(path, &file_purpose);
        let change_impact = determine_change_impact(change_type, &file_purpose);
        let project_significance = determine_project_significance(path, &file_purpose);

        FileContext {
            path: path.to_path_buf(),
            file_purpose,
            architectural_layer,
            change_impact,
            project_significance,
        }
    }

    /// Analyzes multiple files to understand the scope of changes.
    pub fn analyze_file_set(files: &[(PathBuf, String)]) -> Vec<FileContext> {
        files
            .iter()
            .map(|(path, change_type)| Self::analyze_file(path, change_type))
            .collect()
    }

    /// Analyzes file changes across a range of commits, deduplicating by path.
    ///
    /// When a file appears in multiple commits, the status from the last
    /// (most recent) commit wins. This provides the most accurate signal
    /// for significance analysis.
    pub fn analyze_commits(commits: &[CommitInfo]) -> Vec<FileContext> {
        let mut file_map: HashMap<PathBuf, String> = HashMap::new();

        for commit in commits {
            for fc in &commit.analysis.file_changes.file_list {
                file_map.insert(PathBuf::from(&fc.file), fc.status.clone());
            }
        }

        let files: Vec<(PathBuf, String)> = file_map.into_iter().collect();
        Self::analyze_file_set(&files)
    }

    /// Determines the primary architectural impact of a set of file changes.
    pub fn primary_architectural_impact(contexts: &[FileContext]) -> ArchitecturalLayer {
        let mut layer_counts = HashMap::new();
        for context in contexts {
            *layer_counts
                .entry(context.architectural_layer.clone())
                .or_insert(0) += 1;
        }

        // Return the most common architectural layer, with precedence for critical layers
        layer_counts
            .into_iter()
            .max_by_key(|(layer, count)| {
                let priority = match layer {
                    ArchitecturalLayer::Business => 100,
                    ArchitecturalLayer::Data => 90,
                    ArchitecturalLayer::Presentation => 80,
                    ArchitecturalLayer::Infrastructure => 70,
                    ArchitecturalLayer::Cross => 60,
                };
                priority + count
            })
            .map_or(ArchitecturalLayer::Cross, |(layer, _)| layer)
    }

    /// Determines if the file changes suggest a significant architectural change.
    #[must_use]
    pub fn is_architectural_change(contexts: &[FileContext]) -> bool {
        let critical_files = contexts
            .iter()
            .filter(|c| matches!(c.project_significance, ProjectSignificance::Critical))
            .count();

        let breaking_changes = contexts
            .iter()
            .filter(|c| {
                matches!(
                    c.change_impact,
                    ChangeImpact::Breaking | ChangeImpact::Critical
                )
            })
            .count();

        critical_files > 0 || breaking_changes > 1 || contexts.len() > 10
    }
}

/// Determines the purpose of a file based on its path and name.
fn determine_file_purpose(path: &Path) -> FilePurpose {
    let path_str = path.to_string_lossy().to_lowercase();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Configuration files
    if is_config_file(&path_str, &file_name) {
        return FilePurpose::Config;
    }

    // Test files
    if is_test_file(&path_str, &file_name) {
        return FilePurpose::Test;
    }

    // Documentation files
    if is_documentation_file(&path_str, &file_name) {
        return FilePurpose::Documentation;
    }

    // Build and tooling files
    if is_build_file(&path_str, &file_name) {
        return FilePurpose::Build;
    }

    // Development tooling
    if is_tooling_file(&path_str, &file_name) {
        return FilePurpose::Tooling;
    }

    // Interface/API files
    if is_interface_file(&path_str, &file_name) {
        return FilePurpose::Interface;
    }

    // Default to core logic
    FilePurpose::CoreLogic
}

/// Determines the architectural layer of a file.
fn determine_architectural_layer(path: &Path, file_purpose: &FilePurpose) -> ArchitecturalLayer {
    let path_str = path.to_string_lossy().to_lowercase();

    match file_purpose {
        FilePurpose::Config | FilePurpose::Build | FilePurpose::Tooling => {
            ArchitecturalLayer::Infrastructure
        }
        FilePurpose::Test | FilePurpose::Documentation => ArchitecturalLayer::Cross,
        FilePurpose::Interface => ArchitecturalLayer::Presentation,
        FilePurpose::CoreLogic => {
            // Analyze path to determine specific layer
            if path_str.contains("ui") || path_str.contains("web") || path_str.contains("cli") {
                ArchitecturalLayer::Presentation
            } else if path_str.contains("data")
                || path_str.contains("db")
                || path_str.contains("storage")
            {
                ArchitecturalLayer::Data
            } else if path_str.contains("core")
                || path_str.contains("business")
                || path_str.contains("logic")
            {
                ArchitecturalLayer::Business
            } else if path_str.contains("infra")
                || path_str.contains("system")
                || path_str.contains("network")
            {
                ArchitecturalLayer::Infrastructure
            } else {
                ArchitecturalLayer::Business // Default assumption
            }
        }
    }
}

/// Determines the impact of changes based on change type and file purpose.
fn determine_change_impact(change_type: &str, file_purpose: &FilePurpose) -> ChangeImpact {
    match change_type {
        "A" | "C" => ChangeImpact::Additive, // Added or Copied
        "D" => {
            // Deleted file - could be breaking depending on purpose
            match file_purpose {
                FilePurpose::Interface | FilePurpose::CoreLogic => ChangeImpact::Breaking,
                _ => ChangeImpact::Modification,
            }
        }
        "M" => {
            // Modified file - depends on purpose
            match file_purpose {
                FilePurpose::Test | FilePurpose::Documentation => ChangeImpact::Style,
                FilePurpose::Interface => ChangeImpact::Breaking, // Potentially breaking
                _ => ChangeImpact::Modification,
            }
        }
        _ => ChangeImpact::Modification, // Renamed, unknown, etc.
    }
}

/// Determines the significance of a file in the project.
fn determine_project_significance(path: &Path, file_purpose: &FilePurpose) -> ProjectSignificance {
    let path_str = path.to_string_lossy().to_lowercase();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Critical files
    if is_critical_file(&path_str, &file_name) {
        return ProjectSignificance::Critical;
    }

    // Important files based on purpose
    match file_purpose {
        FilePurpose::Interface | FilePurpose::CoreLogic | FilePurpose::Build => {
            ProjectSignificance::Important
        }
        FilePurpose::Config => {
            if file_name.contains("cargo.toml") || file_name.contains("package.json") {
                ProjectSignificance::Critical
            } else {
                ProjectSignificance::Important
            }
        }
        FilePurpose::Test | FilePurpose::Documentation | FilePurpose::Tooling => {
            ProjectSignificance::Routine
        }
    }
}

/// Checks if a file is a configuration file.
fn is_config_file(path_str: &str, file_name: &str) -> bool {
    let config_patterns = [
        ".toml",
        ".json",
        ".yaml",
        ".yml",
        ".ini",
        ".cfg",
        ".conf",
        ".env",
        ".properties",
        "config",
        "settings",
        "options",
    ];

    let config_names = [
        "cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        "makefile",
        "dockerfile",
        ".gitignore",
        ".gitattributes",
    ];

    config_patterns
        .iter()
        .any(|pattern| file_name.contains(pattern))
        || config_names.contains(&file_name)
        || path_str.contains("config")
        || path_str.contains(".github/workflows")
}

/// Checks if a file is a test file.
fn is_test_file(path_str: &str, file_name: &str) -> bool {
    path_str.contains("test")
        || path_str.contains("spec")
        || file_name.contains("test")
        || file_name.contains("spec")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.py")
        || file_name.ends_with(".test.js")
        || file_name.ends_with("_test.go")
}

/// Checks if a file is documentation.
fn is_documentation_file(path_str: &str, file_name: &str) -> bool {
    let doc_extensions = [".md", ".rst", ".txt", ".adoc"];
    let doc_names = ["readme", "changelog", "contributing", "license", "authors"];

    doc_extensions.iter().any(|ext| file_name.ends_with(ext))
        || doc_names.iter().any(|name| file_name.contains(name))
        || path_str.contains("doc")
        || path_str.contains("guide")
        || path_str.contains("manual")
}

/// Checks if a file is build-related.
fn is_build_file(path_str: &str, file_name: &str) -> bool {
    let build_names = [
        "makefile",
        "dockerfile",
        "build.gradle",
        "pom.xml",
        "cmake",
        "webpack.config",
        "rollup.config",
        "vite.config",
    ];

    build_names.iter().any(|name| file_name.contains(name))
        || path_str.contains("build")
        || path_str.contains("scripts")
        || file_name.ends_with(".sh")
        || file_name.ends_with(".bat")
}

/// Checks if a file is tooling/development related.
fn is_tooling_file(path_str: &str, file_name: &str) -> bool {
    path_str.contains("tool")
        || path_str.contains("util")
        || path_str.contains(".vscode")
        || path_str.contains(".idea")
        || file_name.starts_with('.')
        || file_name.contains("prettier")
        || file_name.contains("eslint")
        || file_name.contains("clippy")
}

/// Checks if a file defines interfaces/APIs.
fn is_interface_file(path_str: &str, file_name: &str) -> bool {
    path_str.contains("api")
        || path_str.contains("interface")
        || path_str.contains("proto")
        || file_name.contains("lib.rs")
        || file_name.contains("mod.rs")
        || file_name.contains("index")
        || file_name.ends_with(".proto")
        || file_name.ends_with(".graphql")
}

/// Checks if a file is critical to project functionality.
fn is_critical_file(path_str: &str, file_name: &str) -> bool {
    let critical_names = [
        "main.rs",
        "lib.rs",
        "index.js",
        "app.js",
        "main.py",
        "__init__.py",
        "main.go",
        "main.java",
        "cargo.toml",
        "package.json",
        "go.mod",
        "pom.xml",
    ];

    critical_names.contains(&file_name)
        || (path_str.contains("src") && (file_name == "lib.rs" || file_name == "main.rs"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── determine_file_purpose ─────────────────────────────────────

    #[test]
    fn purpose_config_toml() {
        assert!(matches!(
            determine_file_purpose(Path::new("Cargo.toml")),
            FilePurpose::Config
        ));
    }

    #[test]
    fn purpose_config_json() {
        assert!(matches!(
            determine_file_purpose(Path::new("package.json")),
            FilePurpose::Config
        ));
    }

    #[test]
    fn purpose_test_file() {
        assert!(matches!(
            determine_file_purpose(Path::new("tests/integration_test.rs")),
            FilePurpose::Test
        ));
    }

    #[test]
    fn purpose_documentation() {
        assert!(matches!(
            determine_file_purpose(Path::new("README.md")),
            FilePurpose::Documentation
        ));
    }

    #[test]
    fn purpose_build_file() {
        assert!(matches!(
            determine_file_purpose(Path::new("scripts/build.sh")),
            FilePurpose::Build
        ));
    }

    #[test]
    fn purpose_interface_file() {
        assert!(matches!(
            determine_file_purpose(Path::new("src/api/handler.rs")),
            FilePurpose::Interface
        ));
    }

    #[test]
    fn purpose_core_logic_default() {
        assert!(matches!(
            determine_file_purpose(Path::new("src/claude/prompts.rs")),
            FilePurpose::CoreLogic
        ));
    }

    // ── determine_architectural_layer ──────────────────────────────

    #[test]
    fn layer_config_is_infrastructure() {
        let layer = determine_architectural_layer(Path::new("Cargo.toml"), &FilePurpose::Config);
        assert_eq!(layer, ArchitecturalLayer::Infrastructure);
    }

    #[test]
    fn layer_test_is_cross() {
        let layer = determine_architectural_layer(Path::new("tests/test.rs"), &FilePurpose::Test);
        assert_eq!(layer, ArchitecturalLayer::Cross);
    }

    #[test]
    fn layer_interface_is_presentation() {
        let layer =
            determine_architectural_layer(Path::new("src/api/mod.rs"), &FilePurpose::Interface);
        assert_eq!(layer, ArchitecturalLayer::Presentation);
    }

    #[test]
    fn layer_cli_is_presentation() {
        let layer =
            determine_architectural_layer(Path::new("src/cli/git.rs"), &FilePurpose::CoreLogic);
        assert_eq!(layer, ArchitecturalLayer::Presentation);
    }

    #[test]
    fn layer_data_is_data() {
        let layer =
            determine_architectural_layer(Path::new("src/data/check.rs"), &FilePurpose::CoreLogic);
        assert_eq!(layer, ArchitecturalLayer::Data);
    }

    #[test]
    fn layer_core_is_business() {
        let layer =
            determine_architectural_layer(Path::new("src/core/engine.rs"), &FilePurpose::CoreLogic);
        assert_eq!(layer, ArchitecturalLayer::Business);
    }

    #[test]
    fn layer_unknown_defaults_business() {
        let layer = determine_architectural_layer(
            Path::new("src/claude/prompts.rs"),
            &FilePurpose::CoreLogic,
        );
        assert_eq!(layer, ArchitecturalLayer::Business);
    }

    // ── determine_change_impact ────────────────────────────────────

    #[test]
    fn impact_added_is_additive() {
        assert!(matches!(
            determine_change_impact("A", &FilePurpose::CoreLogic),
            ChangeImpact::Additive
        ));
    }

    #[test]
    fn impact_deleted_interface_is_breaking() {
        assert!(matches!(
            determine_change_impact("D", &FilePurpose::Interface),
            ChangeImpact::Breaking
        ));
    }

    #[test]
    fn impact_deleted_test_is_modification() {
        assert!(matches!(
            determine_change_impact("D", &FilePurpose::Test),
            ChangeImpact::Modification
        ));
    }

    #[test]
    fn impact_modified_test_is_style() {
        assert!(matches!(
            determine_change_impact("M", &FilePurpose::Test),
            ChangeImpact::Style
        ));
    }

    #[test]
    fn impact_modified_core_is_modification() {
        assert!(matches!(
            determine_change_impact("M", &FilePurpose::CoreLogic),
            ChangeImpact::Modification
        ));
    }

    #[test]
    fn impact_unknown_type_is_modification() {
        assert!(matches!(
            determine_change_impact("R", &FilePurpose::CoreLogic),
            ChangeImpact::Modification
        ));
    }

    // ── determine_project_significance ─────────────────────────────

    #[test]
    fn significance_main_rs_is_critical() {
        assert!(matches!(
            determine_project_significance(Path::new("src/main.rs"), &FilePurpose::CoreLogic),
            ProjectSignificance::Critical
        ));
    }

    #[test]
    fn significance_cargo_toml_is_critical() {
        assert!(matches!(
            determine_project_significance(Path::new("Cargo.toml"), &FilePurpose::Config),
            ProjectSignificance::Critical
        ));
    }

    #[test]
    fn significance_core_logic_is_important() {
        assert!(matches!(
            determine_project_significance(
                Path::new("src/claude/prompts.rs"),
                &FilePurpose::CoreLogic
            ),
            ProjectSignificance::Important
        ));
    }

    #[test]
    fn significance_test_is_routine() {
        assert!(matches!(
            determine_project_significance(Path::new("tests/test.rs"), &FilePurpose::Test),
            ProjectSignificance::Routine
        ));
    }

    // ── is_* helper functions ──────────────────────────────────────

    #[test]
    fn test_file_detected() {
        assert!(is_test_file("tests/integration.rs", "integration.rs"));
        assert!(is_test_file("src/foo_test.rs", "foo_test.rs"));
        assert!(!is_test_file("src/main.rs", "main.rs"));
    }

    #[test]
    fn documentation_file_detected() {
        assert!(is_documentation_file("README.md", "readme.md"));
        assert!(is_documentation_file("docs/guide.md", "guide.md"));
        assert!(!is_documentation_file("src/main.rs", "main.rs"));
    }

    #[test]
    fn build_file_detected() {
        assert!(is_build_file("scripts/deploy.sh", "deploy.sh"));
        assert!(is_build_file("Makefile", "makefile"));
        assert!(!is_build_file("src/main.rs", "main.rs"));
    }

    #[test]
    fn interface_file_detected() {
        assert!(is_interface_file("src/api/routes.rs", "routes.rs"));
        assert!(is_interface_file("protos/service.proto", "service.proto"));
        assert!(!is_interface_file("src/claude/prompts.rs", "prompts.rs"));
    }

    // ── FileAnalyzer ───────────────────────────────────────────────

    #[test]
    fn analyze_file_rust_source() {
        let ctx = FileAnalyzer::analyze_file(Path::new("src/claude/prompts.rs"), "M");
        assert!(matches!(ctx.file_purpose, FilePurpose::CoreLogic));
        assert!(matches!(ctx.change_impact, ChangeImpact::Modification));
        assert!(matches!(
            ctx.project_significance,
            ProjectSignificance::Important
        ));
    }

    #[test]
    fn analyze_file_set_multiple() {
        let files = vec![
            (PathBuf::from("src/main.rs"), "M".to_string()),
            (PathBuf::from("README.md"), "M".to_string()),
        ];
        let contexts = FileAnalyzer::analyze_file_set(&files);
        assert_eq!(contexts.len(), 2);
    }

    #[test]
    fn primary_architectural_impact_mixed() {
        let contexts = vec![
            FileAnalyzer::analyze_file(Path::new("src/data/check.rs"), "M"),
            FileAnalyzer::analyze_file(Path::new("src/data/yaml.rs"), "M"),
            FileAnalyzer::analyze_file(Path::new("README.md"), "M"),
        ];
        let layer = FileAnalyzer::primary_architectural_impact(&contexts);
        assert_eq!(layer, ArchitecturalLayer::Data);
    }

    #[test]
    fn primary_architectural_impact_empty() {
        let layer = FileAnalyzer::primary_architectural_impact(&[]);
        assert_eq!(layer, ArchitecturalLayer::Cross);
    }

    #[test]
    fn is_architectural_change_critical_files() {
        let contexts = vec![FileAnalyzer::analyze_file(Path::new("src/main.rs"), "D")];
        assert!(FileAnalyzer::is_architectural_change(&contexts));
    }

    #[test]
    fn is_architectural_change_many_files() {
        let contexts: Vec<_> = (0..11)
            .map(|i| FileAnalyzer::analyze_file(Path::new(&format!("src/file{i}.rs")), "M"))
            .collect();
        assert!(FileAnalyzer::is_architectural_change(&contexts));
    }

    #[test]
    fn is_not_architectural_change_small() {
        let contexts = vec![FileAnalyzer::analyze_file(
            Path::new("src/claude/prompts.rs"),
            "M",
        )];
        assert!(!FileAnalyzer::is_architectural_change(&contexts));
    }

    // ── FileAnalyzer::analyze_commits ─────────────────────────────

    mod analyze_commits_tests {
        use super::*;
        use crate::git::commit::{CommitAnalysis, FileChange, FileChanges};

        fn make_commit(files: Vec<(&str, &str)>) -> CommitInfo {
            CommitInfo {
                hash: "a".repeat(40),
                author: "Test <test@test.com>".to_string(),
                date: chrono::Utc::now().fixed_offset(),
                original_message: "test commit".to_string(),
                in_main_branches: Vec::new(),
                analysis: CommitAnalysis {
                    detected_type: String::new(),
                    detected_scope: String::new(),
                    proposed_message: String::new(),
                    file_changes: FileChanges {
                        total_files: files.len(),
                        files_added: files.iter().filter(|(s, _)| *s == "A").count(),
                        files_deleted: files.iter().filter(|(s, _)| *s == "D").count(),
                        file_list: files
                            .into_iter()
                            .map(|(status, file)| FileChange {
                                status: status.to_string(),
                                file: file.to_string(),
                            })
                            .collect(),
                    },
                    diff_summary: String::new(),
                    diff_file: String::new(),
                    file_diffs: Vec::new(),
                },
            }
        }

        #[test]
        fn empty_commits() {
            let result = FileAnalyzer::analyze_commits(&[]);
            assert!(result.is_empty());
        }

        #[test]
        fn single_commit() {
            let commit = make_commit(vec![("M", "src/main.rs"), ("A", "src/new.rs")]);
            let result = FileAnalyzer::analyze_commits(&[commit]);
            assert_eq!(result.len(), 2);
        }

        #[test]
        fn deduplicates_across_commits() {
            let commits = vec![
                make_commit(vec![("A", "src/feature.rs"), ("M", "src/lib.rs")]),
                make_commit(vec![("M", "src/feature.rs"), ("M", "src/main.rs")]),
            ];
            let result = FileAnalyzer::analyze_commits(&commits);
            // 3 unique files: src/feature.rs, src/lib.rs, src/main.rs
            assert_eq!(result.len(), 3);
        }

        #[test]
        fn last_status_wins() {
            let commits = vec![
                make_commit(vec![("A", "src/feature.rs")]),
                make_commit(vec![("M", "src/feature.rs")]),
            ];
            let result = FileAnalyzer::analyze_commits(&commits);
            assert_eq!(result.len(), 1);
            // Added then Modified — last status (M) wins → Modification impact
            assert!(matches!(
                result[0].change_impact,
                ChangeImpact::Modification
            ));
        }
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn arb_file_purpose() -> impl Strategy<Value = FilePurpose> {
            prop_oneof![
                Just(FilePurpose::Config),
                Just(FilePurpose::Test),
                Just(FilePurpose::Documentation),
                Just(FilePurpose::Build),
                Just(FilePurpose::Tooling),
                Just(FilePurpose::Interface),
                Just(FilePurpose::CoreLogic),
            ]
        }

        proptest! {
            #[test]
            fn file_purpose_deterministic(s in "[a-zA-Z0-9_/\\.]{0,100}") {
                let p = Path::new(&s);
                let a = format!("{:?}", determine_file_purpose(p));
                let b = format!("{:?}", determine_file_purpose(p));
                prop_assert_eq!(a, b);
            }

            #[test]
            fn config_extensions_classified(
                name in "[a-z]{1,10}",
                ext in prop_oneof![
                    Just(".toml"),
                    Just(".json"),
                    Just(".yaml"),
                    Just(".yml"),
                    Just(".ini"),
                    Just(".cfg"),
                ],
            ) {
                let path_str = format!("{name}{ext}");
                let purpose = determine_file_purpose(Path::new(&path_str));
                prop_assert!(matches!(purpose, FilePurpose::Config));
            }

            #[test]
            fn test_paths_classified(name in "[a-z_]{1,20}\\.rs") {
                let path_str = format!("tests/{name}");
                let purpose = determine_file_purpose(Path::new(&path_str));
                prop_assert!(matches!(purpose, FilePurpose::Test));
            }

            #[test]
            fn change_impact_added_always_additive(purpose in arb_file_purpose()) {
                let impact = determine_change_impact("A", &purpose);
                prop_assert!(matches!(impact, ChangeImpact::Additive));
            }

            #[test]
            fn architectural_layer_deterministic(
                s in "[a-zA-Z0-9_/\\.]{0,100}",
                purpose in arb_file_purpose(),
            ) {
                let p = Path::new(&s);
                let a = format!("{:?}", determine_architectural_layer(p, &purpose));
                let b = format!("{:?}", determine_architectural_layer(p, &purpose));
                prop_assert_eq!(a, b);
            }
        }
    }
}

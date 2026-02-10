//! File-based context analysis and architectural understanding.

use std::path::{Path, PathBuf};

use crate::data::context::{
    ArchitecturalLayer, ChangeImpact, FileContext, FilePurpose, ProjectSignificance,
};

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

    /// Determines the primary architectural impact of a set of file changes.
    pub fn primary_architectural_impact(contexts: &[FileContext]) -> ArchitecturalLayer {
        use std::collections::HashMap;

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
            .map(|(layer, _)| layer)
            .unwrap_or(ArchitecturalLayer::Cross)
    }

    /// Determines if the file changes suggest a significant architectural change.
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
        "A" => ChangeImpact::Additive, // Added file
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
                FilePurpose::Config => ChangeImpact::Modification,
                FilePurpose::Test | FilePurpose::Documentation => ChangeImpact::Style,
                FilePurpose::Interface => ChangeImpact::Breaking, // Potentially breaking
                _ => ChangeImpact::Modification,
            }
        }
        "R" => ChangeImpact::Modification, // Renamed
        "C" => ChangeImpact::Additive,     // Copied
        _ => ChangeImpact::Modification,   // Unknown change type
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
        FilePurpose::Interface | FilePurpose::CoreLogic => ProjectSignificance::Important,
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
        FilePurpose::Build => ProjectSignificance::Important,
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
        || file_name.starts_with(".")
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

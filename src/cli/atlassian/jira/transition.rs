//! CLI command for listing and executing JIRA issue transitions.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::JiraTransition;
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Lists or executes workflow transitions on a JIRA issue.
#[derive(Parser)]
pub struct TransitionCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Transition name or ID to execute. Omit to list available transitions.
    pub transition: Option<String>,

    /// Lists available transitions (same as omitting the transition argument).
    #[arg(long)]
    pub list: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl TransitionCommand {
    /// Executes the transition command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let transitions = client.get_transitions(&self.key).await?;

        let Some(target) = self.transition.as_deref().filter(|_| !self.list) else {
            if output_as(&transitions, &self.output)? {
                return Ok(());
            }
            print_transitions(&transitions);
            return Ok(());
        };

        let matched = resolve_transition(target, &transitions)?;

        client.do_transition(&self.key, &matched.id).await?;
        println!("Transitioned {} to \"{}\".", self.key, matched.name);

        Ok(())
    }
}

/// Resolves a transition by exact ID or case-insensitive name match.
fn resolve_transition<'a>(
    target: &str,
    transitions: &'a [JiraTransition],
) -> Result<&'a JiraTransition> {
    // Try exact ID match first
    if let Some(t) = transitions.iter().find(|t| t.id == target) {
        return Ok(t);
    }

    // Try case-insensitive name match
    let target_lower = target.to_lowercase();
    let matches: Vec<_> = transitions
        .iter()
        .filter(|t| t.name.to_lowercase() == target_lower)
        .collect();

    match matches.len() {
        0 => {
            let available: Vec<_> = transitions
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "No transition matching \"{target}\" found.\nAvailable transitions: {}",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )
        }
        1 => Ok(matches[0]),
        _ => {
            let dupes: Vec<_> = matches
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "Ambiguous transition \"{target}\". Matches: {}. Use the transition ID instead.",
                dupes.join(", ")
            )
        }
    }
}

/// Prints transitions as a formatted table.
fn print_transitions(transitions: &[JiraTransition]) {
    if transitions.is_empty() {
        println!("No transitions available.");
        return;
    }

    let id_width = transitions
        .iter()
        .map(|t| t.id.len())
        .max()
        .unwrap_or(2)
        .max(2);

    println!("{:<id_width$}  NAME", "ID");
    let name_sep = "-".repeat(4);
    println!("{:<id_width$}  {name_sep}", "-".repeat(id_width));

    for t in transitions {
        println!("{:<id_width$}  {}", t.id, t.name);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_transitions() -> Vec<JiraTransition> {
        vec![
            JiraTransition {
                id: "11".to_string(),
                name: "In Progress".to_string(),
            },
            JiraTransition {
                id: "21".to_string(),
                name: "Done".to_string(),
            },
            JiraTransition {
                id: "31".to_string(),
                name: "Won't Do".to_string(),
            },
        ]
    }

    // ── resolve_transition ─────────────────────────────────────────

    #[test]
    fn resolve_by_exact_id() {
        let transitions = sample_transitions();
        let result = resolve_transition("21", &transitions).unwrap();
        assert_eq!(result.name, "Done");
    }

    #[test]
    fn resolve_by_name_case_insensitive() {
        let transitions = sample_transitions();
        let result = resolve_transition("in progress", &transitions).unwrap();
        assert_eq!(result.id, "11");
    }

    #[test]
    fn resolve_by_name_exact_case() {
        let transitions = sample_transitions();
        let result = resolve_transition("Done", &transitions).unwrap();
        assert_eq!(result.id, "21");
    }

    #[test]
    fn resolve_not_found() {
        let transitions = sample_transitions();
        let err = resolve_transition("Cancelled", &transitions).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No transition matching"));
        assert!(msg.contains("In Progress"));
        assert!(msg.contains("Done"));
    }

    #[test]
    fn resolve_not_found_empty_list() {
        let err = resolve_transition("Done", &[]).unwrap_err();
        assert!(err.to_string().contains("none"));
    }

    #[test]
    fn resolve_ambiguous() {
        let transitions = vec![
            JiraTransition {
                id: "11".to_string(),
                name: "Done".to_string(),
            },
            JiraTransition {
                id: "21".to_string(),
                name: "Done".to_string(),
            },
        ];
        let err = resolve_transition("Done", &transitions).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Ambiguous"));
        assert!(msg.contains("id: 11"));
        assert!(msg.contains("id: 21"));
    }

    #[test]
    fn resolve_id_takes_priority_over_name() {
        // If a transition ID matches exactly, use it even if name also matches
        let transitions = vec![
            JiraTransition {
                id: "Done".to_string(),
                name: "Something Else".to_string(),
            },
            JiraTransition {
                id: "99".to_string(),
                name: "Done".to_string(),
            },
        ];
        let result = resolve_transition("Done", &transitions).unwrap();
        assert_eq!(result.name, "Something Else"); // matched by ID
    }

    // ── print_transitions ──────────────────────────────────────────

    #[test]
    fn print_transitions_with_items() {
        let transitions = sample_transitions();
        // Should not panic
        print_transitions(&transitions);
    }

    #[test]
    fn print_transitions_empty() {
        print_transitions(&[]);
    }

    // ── TransitionCommand struct ───────────────────────────────────

    #[test]
    fn transition_command_list_mode() {
        let cmd = TransitionCommand {
            key: "PROJ-1".to_string(),
            transition: None,
            list: true,
            output: OutputFormat::Table,
        };
        assert!(cmd.list);
        assert!(cmd.transition.is_none());
    }

    #[test]
    fn transition_command_execute_mode() {
        let cmd = TransitionCommand {
            key: "PROJ-1".to_string(),
            transition: Some("Done".to_string()),
            list: false,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.transition.as_deref(), Some("Done"));
    }
}

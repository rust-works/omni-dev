//! Compare candidate route signals with the existing stage questions on frozen issue text.
//! Usage: cargo run --example jev_route_signal_eval -- INPUT.json OUTPUT.json [REPEATS]
//! Output contains public inputs and Jev answers, never credentials.
use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use omni_dev::jev::client::JevClient;
use omni_dev::jev::config::JevConfig;
use omni_dev::jev::protocol::{Question, SystemOneRequest};
use omni_dev::jev::route::{
    build_route_questions, build_route_state, Ladder, Provider, Tiers, DEFAULT_MAX_INPUT_CHARS,
};
use omni_dev::provider::IssueDoc;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Case {
    id: String,
    doc: IssueDoc,
}

#[derive(Serialize)]
struct Observation {
    id: String,
    variant: String,
    repeat: usize,
    request: SystemOneRequest,
    response: serde_json::Value,
}

fn citation(id: &str) -> Option<&'static str> {
    match id {
        "omni-dev-1845-current"
        | "omni-dev-1845-pre-probe-reconstruction"
        | "omni-dev-1843-pre-probe-reconstruction" => Some("#1830"),
        "omni-dev-1871-current" => Some("#1845"),
        "succinctly-1356-current" => Some("#1129"),
        "succinctly-1343-current" => Some("#2063"),
        "succinctly-1740-current" => Some("#1753"),
        "succinctly-1998-current" => Some("#1419"),
        "succinctly-2511-current" => Some("#1351"),
        "succinctly-2705-current" => Some("#2709"),
        _ => None,
    }
}

fn design_question(citation: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "This issue cites {citation}, which is still open. If {citation} is resolved, how +             likely is it that LESS design work would remain for THIS issue than the current +             text implies — as opposed to this issue's own remaining work being unaffected, +             because it is already scoped separately, is a parallel/sibling effort, or +             {citation} is otherwise not a precondition for finishing this issue's own +             remaining work?"
        ),
        criteria: None,
    }
}

fn implementation_question(citation: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "This issue cites {citation}, which is still open. If {citation} is resolved, how +             likely is it that LESS implementation work (code, tests and docs) would remain for +             THIS issue than the current text implies — as opposed to this issue's own remaining +             implementation work being unaffected because it is already scoped separately, is a +             parallel or sibling effort, or {citation} is otherwise not a precondition for +             finishing this issue's own remaining work? Assume any remaining design work has +             been completed well."
        ),
        criteria: None,
    }
}

fn spike_question() -> Question {
    Question::Noul {
        instructions: "For THIS issue's own remaining design work (not a worked example, quoted +            case, cited issue, or proposed feature), is there a specific empirical check that +            has not yet been run and for which THIS issue already states the concrete action to +            take for each relevant result? Score high only if performing that check directly +            settles a live design fork in THIS issue without further architectural judgment. +            Score low if the check is already complete, merely an example, or additional design +            judgment is still needed."
            .to_string(),
        criteria: None,
    }
}

fn class_only(ladder: &Ladder) -> Result<Ladder> {
    let tiers: Vec<_> = ladder
        .tiers
        .as_slice()
        .iter()
        .map(|tier| serde_json::json!({"name":tier.name,"description":tier.description}))
        .collect();
    Ok(Ladder::named(
        ladder.name.clone(),
        Tiers::parse(&serde_json::json!({"tiers":tiers}).to_string())?,
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(args.next().context("expected INPUT.json")?);
    let output = PathBuf::from(args.next().context("expected OUTPUT.json")?);
    let repeats: usize = args.next().as_deref().unwrap_or("2").parse()?;
    anyhow::ensure!(
        repeats > 0 && args.next().is_none(),
        "expected positive REPEATS"
    );
    anyhow::ensure!(!output.exists(), "OUTPUT.json must be new");
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(input)?)?;
    let config = JevConfig::from_env()?;
    let client = JevClient::from_config(&config)?;
    let ladder = class_only(&Ladder::builtin(Provider::Anthropic)?)?;
    let stages = build_route_questions(&[ladder])?;
    let mut observations = Vec::new();
    for case in cases {
        let (state, _) = build_route_state(&case.doc, DEFAULT_MAX_INPUT_CHARS);
        let mut baseline = stages.clone();
        if let Some(target) = citation(&case.id) {
            baseline.insert("could_be_cheaper_0".to_string(), design_question(target));
        }
        let mut candidate: BTreeMap<String, Question> = baseline.clone();
        if let Some(target) = citation(&case.id) {
            candidate.insert(
                "could_be_cheaper_implement_0".to_string(),
                implementation_question(target),
            );
        }
        candidate.insert("bounded_spike".to_string(), spike_question());
        for repeat in 0..repeats {
            for (variant, questions) in [("baseline", &baseline), ("candidate", &candidate)] {
                let request = SystemOneRequest {
                    state: serde_json::Value::String(state.clone()),
                    model: "jev-1.13.0".to_string(),
                    questions: questions.clone(),
                };
                let response = client.system_one(&request).await?;
                observations.push(Observation {
                    id: case.id.clone(),
                    variant: variant.to_string(),
                    repeat,
                    request,
                    response: serde_json::to_value(response)?,
                });
                println!("{} {variant} repeat {repeat}", case.id);
            }
        }
    }
    std::fs::write(output, serde_json::to_vec_pretty(&observations)?)?;
    Ok(())
}

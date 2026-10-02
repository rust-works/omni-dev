//! Compare candidate route signals with the existing stage questions on frozen issue text.
//! Usage: cargo run --example jev_route_signal_eval -- INPUT.json OUTPUT.json [REPEATS]
//! Output contains public inputs and Jev answers, never credentials.
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use omni_dev::jev::client::JevClient;
use omni_dev::jev::config::JevConfig;
use omni_dev::jev::protocol::{Question, SystemOneRequest, SystemOneResponse};
use omni_dev::jev::route::{
    build_route_questions, build_route_state, could_be_cheaper_question, Ladder, Provider, Tiers,
    DEFAULT_MAX_INPUT_CHARS,
};
use omni_dev::provider::IssueDoc;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
struct Case {
    id: String,
    doc: IssueDoc,
    /// A citation open at the recorded input revision, if one is being tested.
    #[serde(default)]
    citation: Option<String>,
}

#[derive(Serialize)]
struct Observation {
    id: String,
    variant: String,
    repeat: usize,
    request: SystemOneRequest,
    response: serde_json::Value,
}

fn implementation_question(citation: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "This issue cites {citation}, which is still open. If {citation} is resolved, how \
             likely is it that LESS implementation work (code, tests and docs) would remain for \
             THIS issue than the current text implies — as opposed to this issue's own remaining \
             implementation work being unaffected because it is already scoped separately, is a \
             parallel or sibling effort, or {citation} is otherwise not a precondition for \
             finishing this issue's own remaining work? Assume any remaining design work has \
             been completed well."
        ),
        criteria: None,
    }
}

fn spike_question() -> Question {
    Question::Noul {
        instructions: "For THIS issue's own remaining design work (not a worked example, quoted \
            case, cited issue, or proposed feature), is there a specific empirical check that \
            has not yet been run and for which THIS issue already states the concrete action to \
            take for each relevant result? Score high only if performing that check directly \
            settles a live design fork in THIS issue without further architectural judgment. \
            Score low if the check is already complete, merely an example, or additional design \
            judgment is still needed."
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

fn question_maps(
    stages: &BTreeMap<String, Question>,
    citation: Option<&str>,
) -> (BTreeMap<String, Question>, BTreeMap<String, Question>) {
    let mut baseline = stages.clone();
    if let Some(target) = citation {
        baseline.insert(
            "could_be_cheaper_0".to_string(),
            could_be_cheaper_question(target),
        );
    }
    let mut candidate = baseline.clone();
    if let Some(target) = citation {
        candidate.insert(
            "could_be_cheaper_implement_0".to_string(),
            implementation_question(target),
        );
    }
    candidate.insert("bounded_spike".to_string(), spike_question());
    (baseline, candidate)
}

fn validate_cases(cases: &[Case]) -> Result<()> {
    anyhow::ensure!(!cases.is_empty(), "INPUT.json must contain cases");
    let mut ids = std::collections::BTreeSet::new();
    for case in cases {
        anyhow::ensure!(!case.id.trim().is_empty(), "case id must not be empty");
        anyhow::ensure!(ids.insert(&case.id), "duplicate case id {:?}", case.id);
        let (_, truncated) = build_route_state(&case.doc, DEFAULT_MAX_INPUT_CHARS);
        anyhow::ensure!(
            !truncated,
            "case {:?} exceeds the route input limit; freeze and label the submitted text first",
            case.id
        );
        if let Some(target) = &case.citation {
            anyhow::ensure!(!target.trim().is_empty(), "empty citation in {:?}", case.id);
        }
    }
    Ok(())
}

fn validate_response(request: &SystemOneRequest, response: &SystemOneResponse) -> Result<()> {
    anyhow::ensure!(
        response.model == request.model,
        "expected model {:?}, received {:?}",
        request.model,
        response.model
    );
    anyhow::ensure!(
        request.questions.keys().eq(response.answers.keys()),
        "response answer keys do not match the requested questions"
    );
    Ok(())
}

/// Keep every completed response if a later request fails. Use the exclusively
/// created file handle so an existing output cannot be overwritten.
fn checkpoint(output: &mut std::fs::File, observations: &[Observation]) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(observations)?;
    output.seek(SeekFrom::Start(0))?;
    output.write_all(&bytes)?;
    output.set_len(bytes.len() as u64)?;
    output.flush()?;
    Ok(())
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
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(input)?)?;
    validate_cases(&cases)?;
    let config = JevConfig::from_env()?;
    let client = JevClient::from_config(&config)?;
    let ladder = class_only(&Ladder::builtin(Provider::Anthropic)?)?;
    let stages = build_route_questions(&[ladder])?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output)
        .with_context(|| format!("OUTPUT.json must be new: {}", output.display()))?;
    let mut observations = Vec::new();
    checkpoint(&mut output, &observations)?;
    for case in cases {
        let (state, _) = build_route_state(&case.doc, DEFAULT_MAX_INPUT_CHARS);
        let (baseline, candidate) = question_maps(&stages, case.citation.as_deref());
        for repeat in 0..repeats {
            for (variant, questions) in [("baseline", &baseline), ("candidate", &candidate)] {
                let request = SystemOneRequest {
                    state: serde_json::Value::String(state.clone()),
                    model: "jev-1.13.0".to_string(),
                    questions: questions.clone(),
                };
                let response = client
                    .system_one(&request)
                    .await
                    .with_context(|| format!("{} {variant} repeat {repeat}", case.id))?;
                // Preserve even an incompatible response for diagnosis before stopping.
                let validation = validate_response(&request, &response);
                observations.push(Observation {
                    id: case.id.clone(),
                    variant: variant.to_string(),
                    repeat,
                    request,
                    response: serde_json::to_value(response)?,
                });
                checkpoint(&mut output, &observations)?;
                validation.with_context(|| format!("{} {variant} repeat {repeat}", case.id))?;
                println!("{} {variant} repeat {repeat}", case.id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn baseline_uses_production_questions_and_explicit_citation() {
        let stages =
            build_route_questions(&[
                class_only(&Ladder::builtin(Provider::Anthropic).unwrap()).unwrap()
            ])
            .unwrap();
        let (baseline, candidate) = question_maps(&stages, Some("other/repo#42"));
        assert_eq!(baseline.len(), stages.len() + 1);
        assert_eq!(candidate.len(), baseline.len() + 2);
        assert_eq!(
            baseline["could_be_cheaper_0"],
            could_be_cheaper_question("other/repo#42")
        );
        for (key, value) in &baseline {
            assert_eq!(candidate[key], *value);
        }
        let (baseline, candidate) = question_maps(&stages, None);
        assert_eq!(baseline, stages);
        assert_eq!(candidate.len(), stages.len() + 1);
        assert!(candidate.contains_key("bounded_spike"));
        assert!(!candidate.contains_key("could_be_cheaper_implement_0"));
    }

    #[test]
    fn candidate_wording_has_no_patch_artifacts() {
        let Question::Noul {
            instructions,
            criteria,
        } = implementation_question("#42")
        else {
            panic!()
        };
        assert!(criteria.is_none());
        assert_eq!(instructions, "This issue cites #42, which is still open. If #42 is resolved, how likely is it that LESS implementation work (code, tests and docs) would remain for THIS issue than the current text implies — as opposed to this issue's own remaining implementation work being unaffected because it is already scoped separately, is a parallel or sibling effort, or #42 is otherwise not a precondition for finishing this issue's own remaining work? Assume any remaining design work has been completed well.");
        let Question::Noul {
            instructions,
            criteria,
        } = spike_question()
        else {
            panic!()
        };
        assert!(criteria.is_none());
        assert_eq!(instructions, "For THIS issue's own remaining design work (not a worked example, quoted case, cited issue, or proposed feature), is there a specific empirical check that has not yet been run and for which THIS issue already states the concrete action to take for each relevant result? Score high only if performing that check directly settles a live design fork in THIS issue without further architectural judgment. Score low if the check is already complete, merely an example, or additional design judgment is still needed.");
    }

    #[test]
    fn checkpoint_keeps_completed_observations() {
        let mut file = tempfile::tempfile().unwrap();
        let observations = vec![Observation {
            id: "case".into(),
            variant: "baseline".into(),
            repeat: 0,
            request: SystemOneRequest {
                state: serde_json::json!("text"),
                model: "jev-1.13.0".into(),
                questions: BTreeMap::new(),
            },
            response: serde_json::json!({"model":"jev-1.13.0"}),
        }];
        checkpoint(&mut file, &observations).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let saved: serde_json::Value = serde_json::from_reader(&file).unwrap();
        assert_eq!(saved[0]["id"], "case");
        checkpoint(&mut file, &[]).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(
            serde_json::from_reader::<_, serde_json::Value>(&file).unwrap(),
            serde_json::json!([])
        );
    }

    #[test]
    fn rejects_model_drift_and_missing_or_unrequested_answers() {
        use omni_dev::jev::protocol::Answer;
        let request = SystemOneRequest {
            state: serde_json::json!("text"),
            model: "jev-1.13.0".into(),
            questions: BTreeMap::from([("bounded_spike".into(), spike_question())]),
        };
        let mut response = SystemOneResponse {
            model: request.model.clone(),
            answers: BTreeMap::from([("bounded_spike".into(), Answer::Noul { noul: 0.2 })]),
            usage: Default::default(),
        };
        validate_response(&request, &response).unwrap();
        response.model = "jev-other".into();
        assert!(validate_response(&request, &response)
            .unwrap_err()
            .to_string()
            .contains("expected model"));
        response.model = request.model.clone();
        response.answers.clear();
        assert!(validate_response(&request, &response).is_err());
        response
            .answers
            .insert("unrequested".into(), Answer::Noul { noul: 0.2 });
        assert!(validate_response(&request, &response).is_err());
        response
            .answers
            .insert("bounded_spike".into(), Answer::Noul { noul: 0.2 });
        assert!(validate_response(&request, &response).is_err());
    }

    #[test]
    fn rejects_inputs_that_would_silently_truncate() {
        let mut cases: Vec<Case> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/corrected-inputs.json"
        ))
        .unwrap();
        cases[0].doc.body = "x".repeat(DEFAULT_MAX_INPUT_CHARS + 1);
        assert!(validate_cases(&cases)
            .unwrap_err()
            .to_string()
            .contains("exceeds the route input limit"));
    }

    #[test]
    fn frozen_inputs_parse_without_truncation() {
        for input in [
            include_str!("../docs/evaluations/jev-route-1871/inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/holdout-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/corrected-inputs.json"),
        ] {
            let cases: Vec<Case> = serde_json::from_str(input).unwrap();
            validate_cases(&cases).unwrap();
            for case in cases {
                let (_, truncated) = build_route_state(&case.doc, DEFAULT_MAX_INPUT_CHARS);
                assert!(
                    !truncated,
                    "{} labels assume the complete frozen input",
                    case.id
                );
            }
        }
    }

    #[test]
    fn rejects_empty_duplicate_ids_and_empty_citations() {
        let mut cases: Vec<Case> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/inputs.json"
        ))
        .unwrap();
        validate_cases(&cases).unwrap();
        assert!(validate_cases(&[]).is_err());
        cases[0].citation = Some(" ".into());
        assert!(validate_cases(&cases).is_err());
        cases[0].citation = None;
        cases[1].id = cases[0].id.clone();
        assert!(validate_cases(&cases).is_err());
        cases[0].id = "".into();
        assert!(validate_cases(&cases).is_err());
    }
}

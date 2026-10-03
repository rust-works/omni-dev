//! Compare candidate route signals with the existing stage questions on frozen issue text.
//!
//! Usage: cargo run --example jev_route_signal_eval -- INPUT.json OUTPUT.json [REPEATS]
//!        [--ladders anthropic,openai,gemini] [--effort-advice] [--seed N] [--only ID,ID]
//!
//! The baseline is the production request: the embedded stage questions for each
//! requested ladder, the independent `open_questions` question, and one design
//! `could_be_cheaper` question per open citation. The candidate adds one
//! implementation-stage `could_be_cheaper` question per open citation and one
//! issue-level spike question. Output contains public inputs and Jev answers,
//! never credentials.
use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use omni_dev::jev::client::JevClient;
use omni_dev::jev::config::JevConfig;
use omni_dev::jev::protocol::{Question, SystemOneRequest, SystemOneResponse};
use omni_dev::jev::route::{
    build_route_questions_for_mode, build_route_state, could_be_cheaper_key,
    could_be_cheaper_question, Ladder, Provider, DEFAULT_MAX_INPUT_CHARS,
};
use omni_dev::provider::IssueDoc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MODEL: &str = "jev-1.13.0";
const DEFAULT_SEED: u64 = 1;

#[derive(Deserialize)]
struct Case {
    id: String,
    doc: IssueDoc,
    /// A single citation open at the recorded input revision (older inputs).
    #[serde(default)]
    citation: Option<String>,
    /// Every open citation being tested, in the order the baseline records them.
    #[serde(default)]
    citations: Vec<String>,
    /// Which variants to run; both when absent. A simulated-resolved input
    /// runs the baseline only, since it has no candidate question to compare.
    #[serde(default)]
    variants: Option<Vec<Variant>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Variant {
    Baseline,
    Candidate,
}

impl Variant {
    const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Candidate => "candidate",
        }
    }
}

impl Case {
    /// The open citations to ask about: `citations`, then a legacy `citation`.
    fn open_citations(&self) -> Vec<String> {
        let mut all = self.citations.clone();
        if let Some(single) = &self.citation {
            if !all.contains(single) {
                all.push(single.clone());
            }
        }
        all
    }

    fn variants(&self) -> Vec<Variant> {
        self.variants
            .clone()
            .unwrap_or_else(|| vec![Variant::Baseline, Variant::Candidate])
    }
}

#[derive(Serialize)]
struct Observation {
    id: String,
    variant: String,
    repeat: usize,
    /// Position of this request within the whole run.
    sequence: usize,
    /// Seed that fixed the baseline/candidate order for this case and repeat.
    order_seed: u64,
    ladders: Vec<String>,
    effort_advice: bool,
    request: SystemOneRequest,
    response: serde_json::Value,
}

struct Options {
    input: PathBuf,
    output: PathBuf,
    repeats: usize,
    ladders: Vec<Provider>,
    effort_advice: bool,
    seed: u64,
    /// Run only these case ids, in the input's order; every case when empty.
    only: Vec<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Options> {
    let mut positional = Vec::new();
    let mut ladders = vec![Provider::Anthropic];
    let mut effort_advice = false;
    let mut seed = DEFAULT_SEED;
    let mut only = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ladders" => {
                ladders = parse_ladders(&args.next().context("--ladders needs a list")?)?;
            }
            "--effort-advice" => effort_advice = true,
            "--only" => {
                only = args
                    .next()
                    .context("--only needs a list of case ids")?
                    .split(',')
                    .map(|id| id.trim().to_string())
                    .collect();
            }
            "--seed" => {
                seed = args
                    .next()
                    .context("--seed needs a number")?
                    .parse()
                    .context("--seed must be an unsigned integer")?;
            }
            flag if flag.starts_with("--") => bail!("unknown flag {flag:?}"),
            _ => positional.push(arg),
        }
    }
    let mut positional = positional.into_iter();
    let input = PathBuf::from(positional.next().context("expected INPUT.json")?);
    let output = PathBuf::from(positional.next().context("expected OUTPUT.json")?);
    let repeats: usize = positional.next().as_deref().unwrap_or("2").parse()?;
    anyhow::ensure!(
        repeats > 0 && positional.next().is_none(),
        "expected positive REPEATS and no further arguments"
    );
    Ok(Options {
        input,
        output,
        repeats,
        ladders,
        effort_advice,
        seed,
        only,
    })
}

/// Keep only the requested cases, rejecting an id the input does not contain
/// so a typo cannot silently shrink the comparison.
fn select_cases(cases: Vec<Case>, only: &[String]) -> Result<Vec<Case>> {
    if only.is_empty() {
        return Ok(cases);
    }
    for id in only {
        anyhow::ensure!(
            cases.iter().any(|c| &c.id == id),
            "--only names {id:?}, which is not in the input"
        );
    }
    Ok(cases.into_iter().filter(|c| only.contains(&c.id)).collect())
}

fn parse_ladders(list: &str) -> Result<Vec<Provider>> {
    let mut out: Vec<Provider> = Vec::new();
    for name in list.split(',') {
        let provider = Provider::ALL
            .into_iter()
            .find(|p| p.name() == name.trim())
            .with_context(|| format!("unknown ladder {name:?}"))?;
        anyhow::ensure!(!out.contains(&provider), "duplicate ladder {name:?}");
        out.push(provider);
    }
    Ok(out)
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

/// The production stage questions for the requested ladders, built by the
/// function `route` itself calls, with or without `--effort-advice`.
fn stage_questions(
    providers: &[Provider],
    effort_advice: bool,
) -> Result<BTreeMap<String, Question>> {
    let ladders = providers
        .iter()
        .map(|&provider| Ladder::builtin(provider))
        .collect::<Result<Vec<_>>>()?;
    build_route_questions_for_mode(&ladders, effort_advice)
}

fn question_maps(
    stages: &BTreeMap<String, Question>,
    citations: &[String],
) -> (BTreeMap<String, Question>, BTreeMap<String, Question>) {
    let mut baseline = stages.clone();
    for (i, target) in citations.iter().enumerate() {
        baseline.insert(could_be_cheaper_key(i), could_be_cheaper_question(target));
    }
    let mut candidate = baseline.clone();
    for (i, target) in citations.iter().enumerate() {
        candidate.insert(
            format!("could_be_cheaper_implement_{i}"),
            implementation_question(target),
        );
    }
    candidate.insert("bounded_spike".to_string(), spike_question());
    (baseline, candidate)
}

/// Which of the two variants goes first for one case and repeat. Derived from
/// the seed alone, so a run is reproducible and order is not confounded with
/// the variant.
fn candidate_first(seed: u64, id: &str, repeat: usize) -> bool {
    let digest = Sha256::digest(format!("{seed}:{id}:{repeat}").as_bytes());
    digest[0] & 1 == 1
}

/// The case's variants in the order to run them. A pair is ordered by the
/// seed's bit, whichever order the input listed it in.
fn ordered_variants(variants: &[Variant], seed: u64, id: &str, repeat: usize) -> Vec<Variant> {
    if variants.len() == 2
        && variants.contains(&Variant::Baseline)
        && variants.contains(&Variant::Candidate)
    {
        return if candidate_first(seed, id, repeat) {
            vec![Variant::Candidate, Variant::Baseline]
        } else {
            vec![Variant::Baseline, Variant::Candidate]
        };
    }
    variants.to_vec()
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
        let citations = case.open_citations();
        let mut seen = std::collections::BTreeSet::new();
        for target in &citations {
            anyhow::ensure!(!target.trim().is_empty(), "empty citation in {:?}", case.id);
            anyhow::ensure!(
                seen.insert(target),
                "duplicate citation {target:?} in {:?}",
                case.id
            );
        }
        let variants = case.variants();
        anyhow::ensure!(!variants.is_empty(), "case {:?} runs no variant", case.id);
        let distinct: std::collections::BTreeSet<_> = variants.iter().map(|v| v.name()).collect();
        anyhow::ensure!(
            distinct.len() == variants.len(),
            "case {:?} repeats a variant",
            case.id
        );
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
    let options = parse_args(std::env::args().skip(1))?;
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(&options.input)?)?;
    let cases = select_cases(cases, &options.only)?;
    validate_cases(&cases)?;
    let config = JevConfig::from_env()?;
    let client = JevClient::from_config(&config)?;
    let stages = stage_questions(&options.ladders, options.effort_advice)?;
    let ladder_names: Vec<String> = options
        .ladders
        .iter()
        .map(|p| p.name().to_string())
        .collect();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&options.output)
        .with_context(|| format!("OUTPUT.json must be new: {}", options.output.display()))?;
    let mut observations = Vec::new();
    checkpoint(&mut output, &observations)?;
    println!(
        "seed {} ladders {} effort_advice {}",
        options.seed,
        ladder_names.join(","),
        options.effort_advice
    );
    for case in cases {
        let (state, _) = build_route_state(&case.doc, DEFAULT_MAX_INPUT_CHARS);
        let (baseline, candidate) = question_maps(&stages, &case.open_citations());
        for repeat in 0..options.repeats {
            for variant in ordered_variants(&case.variants(), options.seed, &case.id, repeat) {
                let questions = match variant {
                    Variant::Baseline => &baseline,
                    Variant::Candidate => &candidate,
                };
                let request = SystemOneRequest {
                    state: serde_json::Value::String(state.clone()),
                    model: MODEL.to_string(),
                    questions: questions.clone(),
                };
                let name = variant.name();
                let response = client
                    .system_one(&request)
                    .await
                    .with_context(|| format!("{} {name} repeat {repeat}", case.id))?;
                // Preserve even an incompatible response for diagnosis before stopping.
                let validation = validate_response(&request, &response);
                observations.push(Observation {
                    id: case.id.clone(),
                    variant: name.to_string(),
                    repeat,
                    sequence: observations.len(),
                    order_seed: options.seed,
                    ladders: ladder_names.clone(),
                    effort_advice: options.effort_advice,
                    request,
                    response: serde_json::to_value(response)?,
                });
                checkpoint(&mut output, &observations)?;
                validation.with_context(|| format!("{} {name} repeat {repeat}", case.id))?;
                println!("{} {name} repeat {repeat}", case.id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn class_only_stages() -> BTreeMap<String, Question> {
        stage_questions(&[Provider::Anthropic], false).unwrap()
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn corrected_cases() -> Vec<Case> {
        serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/corrected-inputs.json"
        ))
        .unwrap()
    }

    #[test]
    fn baseline_uses_production_questions_and_explicit_citation() {
        let stages = class_only_stages();
        let citations = vec!["other/repo#42".to_string()];
        let (baseline, candidate) = question_maps(&stages, &citations);
        assert_eq!(baseline.len(), stages.len() + 1);
        assert_eq!(candidate.len(), baseline.len() + 2);
        assert_eq!(
            baseline["could_be_cheaper_0"],
            could_be_cheaper_question("other/repo#42")
        );
        for (key, value) in &baseline {
            assert_eq!(candidate[key], *value);
        }
        let (baseline, candidate) = question_maps(&stages, &[]);
        assert_eq!(baseline, stages);
        assert_eq!(candidate.len(), stages.len() + 1);
        assert!(candidate.contains_key("bounded_spike"));
        assert!(!candidate.contains_key("could_be_cheaper_implement_0"));
    }

    #[test]
    fn every_open_citation_gets_its_own_indexed_questions() {
        let stages = class_only_stages();
        let citations = vec!["#1".to_string(), "#2".to_string(), "#3".to_string()];
        let (baseline, candidate) = question_maps(&stages, &citations);
        assert_eq!(baseline.len(), stages.len() + 3);
        assert_eq!(candidate.len(), baseline.len() + 3 + 1);
        for (i, target) in citations.iter().enumerate() {
            assert_eq!(
                baseline[&format!("could_be_cheaper_{i}")],
                could_be_cheaper_question(target)
            );
            assert_eq!(
                candidate[&format!("could_be_cheaper_implement_{i}")],
                implementation_question(target)
            );
        }
        // The spike question is asked once per issue, however many citations it has.
        let spikes = candidate.keys().filter(|k| k.contains("spike")).count();
        assert_eq!(spikes, 1);
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
    fn production_shape_covers_every_ladder_and_optional_effort_advice() {
        let class_only = stage_questions(&Provider::ALL, false).unwrap();
        let with_effort = stage_questions(&Provider::ALL, true).unwrap();
        for provider in Provider::ALL {
            for stage in ["stage_design", "stage_implement", "stage_review"] {
                assert!(
                    class_only.contains_key(&format!("{}.{stage}", provider.name())),
                    "{} {stage}",
                    provider.name()
                );
            }
        }
        assert!(class_only.contains_key("open_questions"));
        assert!(
            with_effort.len() > class_only.len(),
            "effort advice must add questions to the same request"
        );
        for (key, question) in &class_only {
            assert_eq!(
                with_effort[key], *question,
                "{key} changed under effort advice"
            );
        }
        assert_eq!(
            stage_questions(&[Provider::Anthropic], false)
                .unwrap()
                .len()
                + 6,
            class_only.len(),
            "each of two further ladders adds the three stage questions"
        );
    }

    #[test]
    fn parses_flags_in_any_position_and_defaults() {
        let o = parse_args(args(&["in.json", "out.json"])).unwrap();
        assert_eq!(o.repeats, 2);
        assert_eq!(o.ladders, vec![Provider::Anthropic]);
        assert!(!o.effort_advice);
        assert_eq!(o.seed, DEFAULT_SEED);
        let o = parse_args(args(&[
            "--ladders",
            "openai,anthropic",
            "in.json",
            "out.json",
            "3",
            "--effort-advice",
            "--seed",
            "7",
        ]))
        .unwrap();
        assert_eq!(o.repeats, 3);
        assert_eq!(o.ladders, vec![Provider::OpenAi, Provider::Anthropic]);
        assert!(o.effort_advice);
        assert_eq!(o.seed, 7);
    }

    #[test]
    fn only_selects_named_cases_and_rejects_unknown_ids() {
        let o = parse_args(args(&["in.json", "out.json", "--only", "a, b"])).unwrap();
        assert_eq!(o.only, vec!["a".to_string(), "b".to_string()]);
        assert!(parse_args(args(&["in.json", "out.json", "--only"])).is_err());
        let cases = corrected_cases();
        let first = cases[0].id.clone();
        let third = cases[2].id.clone();
        let kept = select_cases(corrected_cases(), &[third.clone(), first.clone()]).unwrap();
        assert_eq!(
            kept.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
            vec![first, third],
            "input order is kept"
        );
        assert_eq!(
            select_cases(corrected_cases(), &[]).unwrap().len(),
            cases.len()
        );
        assert!(select_cases(corrected_cases(), &["nope".to_string()]).is_err());
    }

    #[test]
    fn rejects_bad_arguments() {
        for bad in [
            vec!["in.json"],
            vec!["in.json", "out.json", "0"],
            vec!["in.json", "out.json", "2", "extra"],
            vec!["in.json", "out.json", "--ladders", "nope"],
            vec!["in.json", "out.json", "--ladders", "openai,openai"],
            vec!["in.json", "out.json", "--seed", "x"],
            vec!["in.json", "out.json", "--seed"],
            vec!["in.json", "out.json", "--wat"],
        ] {
            assert!(parse_args(args(&bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn order_is_reproducible_balanced_and_never_drops_a_variant() {
        let both = [Variant::Baseline, Variant::Candidate];
        assert_eq!(
            ordered_variants(&both, 1, "case", 0),
            ordered_variants(&both, 1, "case", 0)
        );
        let mut candidate_first_count = 0;
        for repeat in 0..200 {
            let order = ordered_variants(&both, 9, "case", repeat);
            assert_eq!(order.len(), 2);
            assert_ne!(order[0], order[1]);
            if order[0] == Variant::Candidate {
                candidate_first_count += 1;
            }
        }
        assert!(
            (70..=130).contains(&candidate_first_count),
            "order should be roughly balanced, got {candidate_first_count}/200"
        );
        // A different seed or case changes the schedule somewhere.
        let schedule = |seed: u64, id: &str| -> Vec<bool> {
            (0..64).map(|r| candidate_first(seed, id, r)).collect()
        };
        assert_ne!(schedule(1, "a"), schedule(2, "a"));
        assert_ne!(schedule(1, "a"), schedule(1, "b"));
        assert_eq!(
            ordered_variants(&[Variant::Baseline], 1, "case", 3),
            vec![Variant::Baseline]
        );
        // The listing order of a pair must not decide who goes first.
        let reversed = [Variant::Candidate, Variant::Baseline];
        for repeat in 0..64 {
            assert_eq!(
                ordered_variants(&reversed, 5, "case", repeat),
                ordered_variants(&both, 5, "case", repeat),
                "repeat {repeat}"
            );
            assert_eq!(
                ordered_variants(&both, 5, "case", repeat)[0] == Variant::Candidate,
                candidate_first(5, "case", repeat)
            );
        }
    }

    #[test]
    fn legacy_single_citation_and_new_list_merge_without_duplicates() {
        let mut case = corrected_cases().remove(0);
        case.citations.clear();
        case.citation = Some("#1830".into());
        assert_eq!(case.open_citations(), vec!["#1830".to_string()]);
        case.citations = vec!["#7".into(), "#1830".into()];
        assert_eq!(
            case.open_citations(),
            vec!["#7".to_string(), "#1830".to_string()]
        );
        case.citation = None;
        assert_eq!(
            case.open_citations(),
            vec!["#7".to_string(), "#1830".to_string()]
        );
    }

    #[test]
    fn rejects_empty_duplicate_ids_empty_and_duplicate_citations() {
        let mut cases = corrected_cases();
        validate_cases(&cases).unwrap();
        assert!(validate_cases(&[]).is_err(), "no cases");
        cases[0].citations = vec!["#1".into(), " ".into()];
        assert!(validate_cases(&cases)
            .unwrap_err()
            .to_string()
            .contains("empty citation"));
        cases[0].citations = vec!["#1".into(), "#1".into()];
        assert!(validate_cases(&cases)
            .unwrap_err()
            .to_string()
            .contains("duplicate citation"));
        cases[0].citations.clear();
        cases[0].citation = Some(" ".into());
        assert!(validate_cases(&cases).is_err(), "blank legacy citation");
        cases[0].citation = None;
        cases[1].id = cases[0].id.clone();
        assert!(validate_cases(&cases)
            .unwrap_err()
            .to_string()
            .contains("duplicate case id"));
        cases[0].id = String::new();
        assert!(validate_cases(&cases).is_err(), "empty id");
    }

    #[test]
    fn variants_default_to_both_and_can_be_baseline_only() {
        let json = serde_json::json!({
            "id": "sim",
            "doc": serde_json::to_value(&corrected_cases()[0].doc).unwrap(),
            "variants": ["baseline"],
        });
        let case: Case = serde_json::from_value(json).unwrap();
        assert_eq!(case.variants(), vec![Variant::Baseline]);
        assert_eq!(
            corrected_cases()[0].variants(),
            vec![Variant::Baseline, Variant::Candidate]
        );
        let mut bad = corrected_cases();
        bad[0].variants = Some(vec![]);
        assert!(validate_cases(&bad).is_err());
        bad[0].variants = Some(vec![Variant::Candidate, Variant::Candidate]);
        assert!(validate_cases(&bad).is_err());
    }

    #[test]
    fn checkpoint_keeps_completed_observations() {
        let mut file = tempfile::tempfile().unwrap();
        let observations = vec![Observation {
            id: "case".into(),
            variant: "baseline".into(),
            repeat: 0,
            sequence: 0,
            order_seed: 1,
            ladders: vec!["anthropic".into()],
            effort_advice: false,
            request: SystemOneRequest {
                state: serde_json::json!("text"),
                model: MODEL.into(),
                questions: BTreeMap::new(),
            },
            response: serde_json::json!({"model":MODEL}),
        }];
        checkpoint(&mut file, &observations).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let saved: serde_json::Value = serde_json::from_reader(&file).unwrap();
        assert_eq!(saved[0]["id"], "case");
        assert_eq!(saved[0]["order_seed"], 1);
        assert_eq!(saved[0]["ladders"], serde_json::json!(["anthropic"]));
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
            model: MODEL.into(),
            questions: BTreeMap::from([("bounded_spike".into(), spike_question())]),
        };
        let mut response = SystemOneResponse {
            model: request.model.clone(),
            answers: BTreeMap::from([("bounded_spike".into(), Answer::Noul { noul: 0.2 })]),
            usage: omni_dev::jev::protocol::Usage::default(),
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
        let mut cases = corrected_cases();
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
            include_str!("../docs/evaluations/jev-route-1871/round4-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-spike-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-variant-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-posthoc-inputs.json"),
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
    fn round4_labels_were_recorded_for_every_frozen_case() {
        let labels: serde_json::Value = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/round4-labels.json"
        ))
        .unwrap();
        let implementation: Vec<Case> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/round4-inputs.json"
        ))
        .unwrap();
        let spikes: Vec<Case> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/round4-spike-inputs.json"
        ))
        .unwrap();
        let labelled: std::collections::BTreeSet<(String, String)> = labels
            ["implementation_bearing"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["id"].as_str().unwrap().to_string(),
                    l["citation"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let expected: std::collections::BTreeSet<(String, String)> = implementation
            .iter()
            .flat_map(|c| {
                c.open_citations()
                    .into_iter()
                    .map(|cit| (c.id.clone(), cit))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(labelled, expected, "every open citation needs both labels");
        let spike_labelled: std::collections::BTreeSet<&str> = labels["spike"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        let spike_expected: std::collections::BTreeSet<&str> =
            spikes.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(spike_labelled, spike_expected);
        for l in labels["implementation_bearing"].as_array().unwrap() {
            assert!(l["author"].is_string() && l["blind"].is_string());
        }
    }

    #[test]
    fn round4_variants_change_only_what_they_record() {
        let base: std::collections::BTreeMap<String, Case> = serde_json::from_str::<Vec<Case>>(
            include_str!("../docs/evaluations/jev-route-1871/round4-inputs.json"),
        )
        .unwrap()
        .into_iter()
        .map(|c| (c.id.clone(), c))
        .collect();
        let mut variants: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/round4-variant-inputs.json"
        ))
        .unwrap();
        variants.extend(
            serde_json::from_str::<Vec<serde_json::Value>>(include_str!(
                "../docs/evaluations/jev-route-1871/round4-posthoc-inputs.json"
            ))
            .unwrap(),
        );
        assert!(!variants.is_empty());
        for variant in variants {
            let derived = variant["derived_from"].as_str().unwrap();
            let original = &base[derived];
            let case: Case = serde_json::from_value(variant.clone()).unwrap();
            let edit = &variant["edit"];
            let text = |doc: &IssueDoc| -> String {
                let mut all = doc.body.clone();
                for c in &doc.comments {
                    all.push('\n');
                    all.push_str(&c.body);
                }
                all
            };
            let (before, after) = (text(&original.doc), text(&case.doc));
            assert_ne!(before, after, "{} must differ from its source", case.id);
            assert_eq!(case.doc.title, original.doc.title);
            assert_eq!(case.doc.comments.len(), original.doc.comments.len());
            if case.id.ends_with("-noclass") {
                let removed = edit["removed"].as_str().unwrap();
                assert!(removed.starts_with("**Class:"));
                assert!(before.contains(removed));
                assert!(!after.contains("**Class:"));
                assert!(!after.contains("Opus"));
                // Nothing but the recorded paragraph may differ, ignoring the
                // whitespace the removal tidies.
                let squash = |t: &str| t.split_whitespace().collect::<Vec<_>>().join(" ");
                assert_eq!(
                    squash(&before.replace(removed, "")),
                    squash(&after),
                    "{} differs by more than its recorded paragraph",
                    case.id
                );
                assert_eq!(case.open_citations(), original.open_citations());
            } else {
                let (from, to) = (edit["from"].as_str().unwrap(), edit["to"].as_str().unwrap());
                assert_eq!(after.replace(to, from), before, "{}", case.id);
                assert_eq!(after.matches(to).count(), 1);
                assert_eq!(variant["reconstructed"], true);
            }
        }
    }

    #[test]
    fn round4_archives_hold_exactly_the_requests_the_harness_builds() {
        // The round before this one was invalidated by patch artifacts in its
        // question strings. Every archived request must equal, question for
        // question, what this file builds today from the production builders
        // for the ladders and effort mode the observation recorded.
        fn read(bytes: &[u8]) -> Vec<serde_json::Value> {
            use std::io::Read;
            let mut text = String::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_string(&mut text)
                .unwrap();
            serde_json::from_str(&text).unwrap()
        }
        let mut cases = BTreeMap::new();
        for input in [
            include_str!("../docs/evaluations/jev-route-1871/round4-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-variant-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-spike-inputs.json"),
            include_str!("../docs/evaluations/jev-route-1871/round4-posthoc-inputs.json"),
        ] {
            for case in serde_json::from_str::<Vec<Case>>(input).unwrap() {
                cases.insert(case.id.clone(), case);
            }
        }
        let archives = [
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-A-asis-results.json.gz"
            )),
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-B-variant-results.json.gz"
            )),
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-C-spike-results.json.gz"
            )),
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-D1-production-impl-results.json.gz"
            )),
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-D2-production-spike-results.json.gz"
            )),
            read(include_bytes!(
                "../docs/evaluations/jev-route-1871/round4-E-posthoc-absorb-results.json.gz"
            )),
        ];
        let mut checked = 0;
        for observation in archives.iter().flatten() {
            let id = observation["id"].as_str().unwrap();
            let case = &cases[id];
            let providers: Vec<Provider> = observation["ladders"]
                .as_array()
                .unwrap()
                .iter()
                .map(|name| {
                    Provider::ALL
                        .into_iter()
                        .find(|p| p.name() == name.as_str().unwrap())
                        .unwrap()
                })
                .collect();
            let stages =
                stage_questions(&providers, observation["effort_advice"].as_bool().unwrap())
                    .unwrap();
            let (baseline, candidate) = question_maps(&stages, &case.open_citations());
            let expected = match observation["variant"].as_str().unwrap() {
                "baseline" => baseline,
                "candidate" => candidate,
                other => panic!("{id}: unknown variant {other}"),
            };
            assert!(
                case.variants()
                    .iter()
                    .any(|v| v.name() == observation["variant"]),
                "{id} ran a variant its input does not list"
            );
            assert_eq!(
                observation["request"]["questions"],
                serde_json::to_value(&expected).unwrap(),
                "{id} {} request differs from the harness's",
                observation["variant"]
            );
            assert_eq!(observation["request"]["model"], MODEL, "{id}");
            assert_eq!(observation["response"]["model"], MODEL, "{id}");
            let answered: Vec<_> = observation["response"]["answers"]
                .as_object()
                .unwrap()
                .keys()
                .collect();
            assert_eq!(answered, expected.keys().collect::<Vec<_>>(), "{id}");
            checked += 1;
        }
        assert_eq!(
            checked, 254,
            "54 + 66 + 76 + 16 + 24 + 18 recorded requests"
        );
    }

    #[test]
    fn round4_implementation_citations_match_the_census_baseline() {
        // The candidate must ask about exactly the open citations the archived
        // production baseline recorded, in its order, or the comparison is not
        // like-for-like with that baseline.
        let baseline: serde_json::Value = {
            use std::io::Read;
            let bytes = include_bytes!("../docs/evaluations/jev-route-2052/baseline.json.gz");
            let mut text = String::new();
            flate2::read::GzDecoder::new(&bytes[..])
                .read_to_string(&mut text)
                .unwrap();
            serde_json::from_str(&text).unwrap()
        };
        let cases: Vec<Case> = serde_json::from_str(include_str!(
            "../docs/evaluations/jev-route-1871/round4-inputs.json"
        ))
        .unwrap();
        let mut checked = 0;
        for case in &cases {
            let refname = format!("{}#{}", case.doc.project, case.doc.number);
            let issue = baseline["issues"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["ref"] == refname)
                .unwrap_or_else(|| panic!("{refname} is not in the census baseline"));
            let recorded: Vec<String> = issue["depends_on"]
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .map(|d| d["ref"].as_str().unwrap().to_string())
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(case.open_citations(), recorded, "{}", case.id);
            checked += 1;
        }
        assert_eq!(checked, cases.len());
    }
}

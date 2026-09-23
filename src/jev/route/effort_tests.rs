use super::*;
use crate::jev::route::{build_route_questions, Provider, ProviderRoute, Tiers};

fn provider_routes(
    answers: &BTreeMap<String, Answer>,
    ladders: &[Ladder],
    close_call: f64,
) -> anyhow::Result<BTreeMap<String, ProviderRoute>> {
    super::super::provider_routes(answers, ladders, close_call, true)
}

const CUSTOM: &str = r"
tiers:
  - name: small,alternative
    description: Executes a clear local specification.
    models:
      - name: vendor.small.v1
        effort:
          kind: configurable
          control: thinking
          supported_levels: [quick, thorough]
          levels:
            - name: quick
              description: Routine local work.
              criteria: {review: Check a local change against an explicit checklist.}
            - name: thorough
              description: Work with interacting constraints.
      - name: vendor.fixed
        effort: {kind: fixed, control: thinking, level: automatic, reason: Always adaptive.}
  - name: large
    description: Explores open designs.
    models:
      - name: vendor.large
        stages: [design]
        effort: {kind: unavailable, reason: No verified control.}
      - name: vendor.large.coding
        stages: [implement, review]
        effort: {kind: fixed, control: thinking, level: deep, reason: Fixed mode.}
";

fn custom() -> Ladder {
    Ladder::named("custom".to_string(), Tiers::parse(CUSTOM).unwrap())
}

fn response(ladders: &[Ladder]) -> BTreeMap<String, Answer> {
    build_route_questions(ladders)
        .unwrap()
        .into_iter()
        .map(|(key, q)| {
            let Question::Choice { criteria, .. } = q else {
                panic!()
            };
            let choice = if key.ends_with(".remaining") {
                "remaining".to_string()
            } else if key.contains(".effort_") {
                criteria
                    .keys()
                    .find(|k| ![NOT_NEEDED, INSUFFICIENT].contains(&k.as_str()))
                    .unwrap()
                    .clone()
            } else {
                ladders
                    .iter()
                    .find(|l| key.starts_with(&format!("{}.", l.name)))
                    .unwrap()
                    .tiers
                    .as_slice()[0]
                    .name
                    .clone()
            };
            let probabilities = criteria
                .keys()
                .map(|k| (k.clone(), if k == &choice { 1.0 } else { 0.0 }))
                .collect();
            (
                key,
                Answer::Choice {
                    choice,
                    confidence: 0.9,
                    probabilities,
                },
            )
        })
        .collect()
}

fn set_choice(answers: &mut BTreeMap<String, Answer>, key: &str, value: &str) {
    let Answer::Choice { choice, .. } = answers.get_mut(key).unwrap() else {
        panic!()
    };
    *choice = value.to_string();
}

#[test]
fn builtins_offer_only_their_native_levels_and_keep_original_stage_questions() {
    let ladders: Vec<_> = Provider::ALL
        .into_iter()
        .map(|p| Ladder::builtin(p).unwrap())
        .collect();
    let q = build_route_questions(&ladders).unwrap();
    // 9 class + 24 configurable-model effort + 3 Deep Think applicability questions.
    assert_eq!(q.len(), 36);
    let keys = |key: &str| {
        let Question::Choice { criteria, .. } = &q[key] else {
            panic!()
        };
        criteria.keys().map(String::as_str).collect::<Vec<_>>()
    };
    assert!(keys("openai.stage_implement.effort_0_0").contains(&"none"));
    assert!(!keys("openai.stage_implement.effort_2_0").contains(&"none"));
    assert!(keys("gemini.stage_review.effort_0_0").contains(&"minimal"));
    assert!(!keys("gemini.stage_review.effort_1_0").contains(&"minimal"));
    assert!(!keys("gemini.stage_review.effort_1_0").contains(&"max"));
    assert!(keys("anthropic.stage_design.effort_0_0").contains(&"max"));
    assert!(!keys("anthropic.stage_design.effort_0_0").contains(&"none"));
    assert!(keys("anthropic.stage_design").contains(&"none"));
    for ladder in &ladders {
        for model in ladder
            .tiers
            .as_slice()
            .iter()
            .flat_map(|t| t.models.iter().flatten())
        {
            assert_eq!(model.verified.as_deref(), Some("2026-09-23"));
            assert!(!model.sources.is_empty());
        }
    }
}

#[test]
fn custom_models_use_stage_overrides_and_shared_applicability() {
    let ladder = custom();
    let q = build_route_questions(std::slice::from_ref(&ladder)).unwrap();
    assert_eq!(q.len(), 9); // three class, three configurable, three applicability
    let Question::Choice {
        criteria,
        instructions,
    } = &q["custom.stage_review.effort_0_0"]
    else {
        panic!()
    };
    assert_eq!(
        criteria["quick"],
        "Check a local change against an explicit checklist."
    );
    assert!(instructions.contains("vendor.small.v1"));
    assert!(instructions.contains("Review:"));
    assert!(!q.keys().any(|k| k.contains("vendor")));
    let routes = provider_routes(&response(std::slice::from_ref(&ladder)), &[ladder], 0.3).unwrap();
    let r = &routes["custom"];
    assert_eq!(
        r.stages.design.effort_by_model["large"][0].model,
        "vendor.large"
    );
    assert_eq!(
        r.stages.implement.effort_by_model["large"][0].model,
        "vendor.large.coding"
    );
    assert_eq!(
        r.stages.review.effort_by_model["small,alternative"].len(),
        2
    );
    assert_eq!(
        r.stages.design.effort_by_model["large"][0].status,
        Status::Unavailable
    );
    assert_eq!(
        r.stages.implement.effort_by_model["large"][0].status,
        Status::Fixed
    );
}

#[test]
fn strict_ladder_validation_rejects_bad_metadata() {
    let cases = [
        ("level: automatic", "level: not_needed", "reserved outcome"),
        (
            "supported_levels: [quick, thorough]",
            "supported_levels: [quick, alien]",
            "unknown supported level",
        ),
        (
            "supported_levels: [quick, thorough]",
            "supported_levels: [quick, quick]",
            "duplicate supported level",
        ),
        (
            "supported_levels: [quick, thorough]",
            "supported_levels: [thorough, quick]",
            "must follow the order",
        ),
        ("- name: thorough", "- name: quick", "duplicate level"),
        (
            "description: Work with interacting constraints.",
            "description: ''",
            "empty criteria",
        ),
        (
            "description: Work with interacting constraints.",
            "criteria: {design: Only design.}",
            "missing criteria",
        ),
        (
            "supported_levels: [quick, thorough]",
            "supported_levels: [quick]",
            "at least two",
        ),
        ("- name: thorough", "- name: not_needed", "reserved outcome"),
        ("control: thinking", "controll: thinking", "unknown field"),
        (
            "stages: [design]",
            "stages: [design, design]",
            "more than once",
        ),
        ("stages: [design]", "stages: [desgin]", "unknown variant"),
        (
            "stages: [implement, review]",
            "stages: [review]",
            "no model binding",
        ),
        ("reason: Fixed mode.", "reason: ''", "non-empty"),
    ];
    for (from, to, error) in cases {
        let err = Tiers::parse(&CUSTOM.replace(from, to)).unwrap_err();
        assert!(
            format!("{err:#}").contains(error),
            "{from} -> {to}: {err:#}"
        );
    }
    let err =
        Tiers::parse("tiers: [{name: a, description: A, models: []}, {name: b, description: B}]")
            .unwrap_err();
    assert!(format!("{err:#}").contains("models must not be empty"));

    let err =
        Tiers::parse(&CUSTOM.replace("name: vendor.large\n", "name: vendor.fixed\n")).unwrap_err();
    let message = format!("{err:#}");
    assert!(
        message.contains("vendor.fixed")
            && message.contains("small,alternative")
            && message.contains("large")
            && message.contains("Design"),
        "{message}"
    );
}

#[test]
fn decoding_preserves_class_data_and_reports_every_rung_in_every_stage() {
    let ladders: Vec<_> = Provider::ALL
        .into_iter()
        .map(|p| Ladder::builtin(p).unwrap())
        .collect();
    let answers = response(&ladders);
    let routes = provider_routes(&answers, &ladders, 0.3).unwrap();
    for ladder in &ladders {
        let route = &routes[&ladder.name];
        assert_eq!(route.class, ladder.tiers.as_slice()[0].name);
        for stage in Stage::ALL {
            let a = route.stages.get(stage);
            assert_eq!(a.effort_by_model.len(), 3);
            let Answer::Choice {
                choice,
                confidence,
                probabilities,
            } = &answers[&stage.question_key(&ladder.name)]
            else {
                panic!()
            };
            assert_eq!(
                (&a.choice, a.confidence, &a.probabilities),
                (choice, *confidence, probabilities)
            );
        }
    }
}

#[test]
fn no_design_suppresses_all_effort_and_other_stages_can_be_completed_or_insufficient() {
    let ladder = custom();
    let mut answers = response(std::slice::from_ref(&ladder));
    set_choice(&mut answers, "custom.stage_design", "none");
    set_choice(
        &mut answers,
        "custom.stage_implement.effort_0_0",
        INSUFFICIENT,
    );
    set_choice(&mut answers, "custom.stage_review.effort_0_0", NOT_NEEDED);
    set_choice(&mut answers, "custom.stage_review.remaining", NOT_NEEDED);
    let routes = provider_routes(&answers, &[ladder], 0.3).unwrap();
    for e in routes["custom"]
        .stages
        .design
        .effort_by_model
        .values()
        .flatten()
    {
        assert_eq!(e.status, Status::NotNeeded);
        assert!(e.level.is_none() && e.assessment.is_none());
    }
    assert_eq!(
        routes["custom"].stages.implement.effort_by_model["small,alternative"][0].status,
        Status::Insufficient
    );
    for e in routes["custom"]
        .stages
        .review
        .effort_by_model
        .values()
        .flatten()
    {
        assert_eq!(e.status, Status::NotNeeded);
        assert!(e.level.is_none());
    }
}

#[test]
fn missing_wrong_type_unsupported_or_bad_probability_answers_fail_with_context() {
    let ladder = custom();
    let key = "custom.stage_implement.effort_0_0";
    let original = response(std::slice::from_ref(&ladder));
    let mut cases = Vec::new();
    let mut missing = original.clone();
    missing.remove(key);
    cases.push(missing);
    let mut unsupported = original.clone();
    set_choice(&mut unsupported, key, "high");
    cases.push(unsupported);
    let mut wrong = original.clone();
    wrong.insert(key.into(), Answer::Noul { noul: 0.9 });
    cases.push(wrong);
    for bad in ["unknown", "missing", "invalid", "confidence"] {
        let mut answers = original.clone();
        let Answer::Choice {
            confidence,
            probabilities,
            ..
        } = answers.get_mut(key).unwrap()
        else {
            panic!()
        };
        match bad {
            "unknown" => {
                probabilities.insert("alien".into(), 0.1);
            }
            "missing" => {
                probabilities.remove("quick");
            }
            "invalid" => {
                probabilities.insert("quick".into(), f64::NAN);
            }
            _ => {
                *confidence = 2.0;
            }
        }
        cases.push(answers);
    }
    for answers in cases {
        let err = provider_routes(&answers, std::slice::from_ref(&ladder), 0.3).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(key) && message.contains("vendor.small.v1"),
            "{message}"
        );
    }
}

#[test]
fn serialization_and_text_keep_native_levels_and_close_call_evidence() {
    let ladder = custom();
    let mut answers = response(std::slice::from_ref(&ladder));
    let Answer::Choice {
        choice,
        confidence,
        probabilities,
    } = answers
        .get_mut("custom.stage_implement.effort_0_0")
        .unwrap()
    else {
        panic!()
    };
    *choice = "quick".into();
    *confidence = 0.2;
    probabilities.insert("quick".into(), 0.2);
    probabilities.insert("thorough".into(), 0.4);
    probabilities.insert(INSUFFICIENT.into(), 0.4);
    let routes = provider_routes(&answers, std::slice::from_ref(&ladder), 0.3).unwrap();
    let route = &routes["custom"];
    let json = serde_json::to_value(route).unwrap();
    let yaml: serde_json::Value =
        serde_yaml::from_str(&serde_yaml::to_string(route).unwrap()).unwrap();
    assert_eq!(json, yaml);
    let e = &json["stages"]["implement"]["effort_by_model"]["small,alternative"][0];
    assert_eq!(e["level"], "quick");
    assert_eq!(e["status"], "recommended");
    assert_eq!(e["assessment"]["confidence"], 0.2);
    assert_eq!(e["close_call"], true);
    assert!(route.close_calls.is_empty());
    let lines = super::super::render_provider_line(
        "custom",
        route,
        2,
        Some(&ladder),
        super::super::TerminalStyle::default(),
    );
    let text = lines.join("\n");
    assert!(text.starts_with("  custom:\n"));
    assert!(text.contains("quick [1]"), "{text}");
    assert!(
        text.contains("    [1] small,alternative [vendor.small.v1], implement:"),
        "{text}"
    );
    assert!(text.contains("small,alternative [vendor.small.v1], implement: quick (0.20, close call — insufficient 0.40)"), "{text}");
    assert!(text.contains("vendor.large.coding]"));
    assert!(text.contains("deep (fixed)"));
    assert!(!text.contains('\x1b'));
}

#[test]
fn legacy_custom_ladders_are_explicitly_unspecified() {
    let ladder = Ladder::named(
        "old".into(),
        Tiers::parse("tiers: [{name: a, description: A}, {name: b, description: B}]").unwrap(),
    );
    let mut answers = response(std::slice::from_ref(&ladder));
    assert_eq!(answers.len(), 3);
    set_choice(&mut answers, "old.stage_design", "none");
    let routes = provider_routes(&answers, &[ladder], 0.3).unwrap();
    assert_eq!(
        routes["old"].stages.design.effort_by_model["a"][0].status,
        Status::NotNeeded
    );
    assert_eq!(
        routes["old"].stages.implement.effort_by_model["a"][0].status,
        Status::Unspecified
    );
    let text = render(&routes["old"].stages, None).join("\n");
    assert!(text.contains("not needed"), "{text}");
    assert!(text.contains("unspecified (add effort metadata)"), "{text}");
}

#[tokio::test]
async fn all_ladders_and_efforts_share_one_request_and_fail_only_the_bad_issue() {
    use crate::jev::route::{run_route, OpenDependencies, RouteOptions, RouteOutcome};
    use crate::provider::{GitProvider, IssueDoc, ItemKind, ItemState};
    let ladders: Vec<_> = Provider::ALL
        .into_iter()
        .map(|p| Ladder::builtin(p).unwrap())
        .chain([custom()])
        .collect();
    let server = wiremock::MockServer::start().await;
    let mut answers = response(&ladders);
    answers.insert("could_be_cheaper_0".into(), Answer::Noul { noul: 0.4 });
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = request.body_json().unwrap();
            let mut answers = answers.clone();
            if body["state"].as_str().unwrap().contains("# #2 ") {
                answers.remove("openai.stage_review.effort_2_0");
            }
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "jev-test", "answers": answers,
                "usage": {"input_tokens": 100, "output_tokens": 20}
            }))
        })
        .expect(3)
        .mount(&server)
        .await;
    let client = crate::jev::client::JevClient::new(&server.uri(), "test").unwrap();
    let doc = IssueDoc {
        provider: GitProvider::GitHub,
        project: "owner/repo".into(),
        number: 1,
        kind: ItemKind::Issue,
        title: "A change".into(),
        state: ItemState::Open,
        body: "Remaining work".into(),
        comments: vec![],
        closed_by: vec![],
        url: "https://github.com/owner/repo/issues/1".into(),
    };
    let options = RouteOptions {
        model: "jev-test".into(),
        close_call: 0.3,
        effort_advice: true,
        max_input_chars: 60000,
        allow_closed: false,
    };
    let mut bad_doc = doc.clone();
    bad_doc.number = 2;
    let dependencies = OpenDependencies::from([(
        ("owner/repo".into(), 1),
        crate::jev::citations::find_citations(
            "#9",
            "owner/repo",
            &crate::provider::ItemRef {
                provider: GitProvider::GitHub,
                project: "owner/repo".into(),
                kind: ItemKind::Issue,
                number: 1,
            },
        ),
    )]);
    let report = run_route(
        &client,
        &[doc.clone(), bad_doc, doc],
        &ladders,
        &options,
        &dependencies,
    )
    .await
    .unwrap();
    assert!(matches!(
        report.issues[0].outcome,
        RouteOutcome::Routed { .. }
    ));
    assert!(report.issues[1].failed());
    assert!(!report.issues[2].failed());
    let RouteOutcome::Routed { depends_on, .. } = &report.issues[0].outcome else {
        panic!()
    };
    assert_eq!(depends_on.len(), 1);
    assert_eq!(report.usage.input_tokens, 300);
    let json = serde_json::to_value(&report).unwrap();
    assert!(json["issues"][0]["providers"]["openai"]["stages"]["design"]
        .get("effort_by_model")
        .is_some());
    let text = super::super::render_route_text_styled(
        &report,
        60000,
        &ladders,
        super::super::TerminalStyle::default(),
    );
    assert!(text.contains("Model / effort"), "{text}");
    let requests = server.received_requests().await.unwrap();
    let request: serde_json::Value = requests[0].body_json().unwrap();
    assert_eq!(request["questions"].as_object().unwrap().len(), 46);
    assert!(request["questions"].get("could_be_cheaper_0").is_some());
}

#[tokio::test]
async fn class_only_route_omits_effort_questions_and_output_for_builtin_and_custom_ladders() {
    use crate::jev::route::{run_route, OpenDependencies, RouteOptions, RouteOutcome};
    use crate::provider::{GitProvider, IssueDoc, ItemKind, ItemRef, ItemState};

    let ladders = [
        Ladder::builtin(Provider::OpenAi).unwrap(),
        Ladder::builtin(Provider::Anthropic).unwrap(),
        custom(),
    ];
    let questions = super::super::build_route_questions_for_mode(&ladders, false).unwrap();
    assert_eq!(questions.len(), 9);
    assert!(questions.keys().all(|key| key.contains(".stage_")));
    let mut answers = response(&ladders);
    answers.retain(|key, _| questions.contains_key(key));
    assert_eq!(answers.len(), 9);
    answers.insert("could_be_cheaper_0".into(), Answer::Noul { noul: 0.4 });

    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "jev-test", "answers": answers,
                "usage": {"input_tokens": 10, "output_tokens": 2}
            })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = crate::jev::client::JevClient::new(&server.uri(), "test").unwrap();
    let doc = IssueDoc {
        provider: GitProvider::GitHub,
        project: "owner/repo".into(),
        number: 1,
        kind: ItemKind::Issue,
        title: "A change".into(),
        state: ItemState::Open,
        body: "Remaining work".into(),
        comments: vec![],
        closed_by: vec![],
        url: "https://github.com/owner/repo/issues/1".into(),
    };
    let dependencies = OpenDependencies::from([(
        ("owner/repo".into(), 1),
        crate::jev::citations::find_citations(
            "#9",
            "owner/repo",
            &ItemRef {
                provider: GitProvider::GitHub,
                project: "owner/repo".into(),
                kind: ItemKind::Issue,
                number: 1,
            },
        ),
    )]);
    let report = run_route(
        &client,
        &[doc],
        &ladders,
        &RouteOptions {
            model: "jev-test".into(),
            close_call: 0.3,
            effort_advice: false,
            max_input_chars: 60000,
            allow_closed: false,
        },
        &dependencies,
    )
    .await
    .unwrap();
    let RouteOutcome::Routed { depends_on, .. } = &report.issues[0].outcome else {
        panic!("expected a routed issue");
    };
    assert_eq!(depends_on.len(), 1);
    let json = serde_json::to_value(&report).unwrap();
    let yaml: serde_json::Value =
        serde_yaml::from_str(&serde_yaml::to_string(&report).unwrap()).unwrap();
    assert_eq!(json, yaml);
    for provider in ["openai", "anthropic", "custom"] {
        for stage in ["design", "implement", "review"] {
            assert!(json["issues"][0]["providers"][provider]["stages"][stage]
                .get("effort_by_model")
                .is_none());
        }
    }
    let text = super::super::render_route_text(&report, 60000);
    assert!(!text.contains("Model / effort"), "{text}");
    assert!(text.contains("openai:") && text.contains("anthropic:") && text.contains("custom:"));
    let requests = server.received_requests().await.unwrap();
    let request: serde_json::Value = requests[0].body_json().unwrap();
    assert_eq!(request["questions"].as_object().unwrap().len(), 10);
    assert!(request["questions"].get("could_be_cheaper_0").is_some());
}

#[test]
fn text_groups_builtin_models_in_ladder_order_with_three_stage_columns() {
    let ladder = Ladder::builtin(Provider::OpenAi).unwrap();
    let routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let lines = render(&routes["openai"].stages, Some(&ladder));
    assert_eq!(lines.len(), 4, "{lines:#?}");
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["Model", "/", "effort", "Design", "Implement", "Review"]
    );
    for (line, tier) in lines[1..].iter().zip(ladder.tiers.as_slice()) {
        assert!(line.trim_start().starts_with(&tier.name), "{line}");
        assert_eq!(line.matches("(0.90)").count(), 3, "{line}");
    }
}

#[test]
fn text_aligns_columns_when_a_custom_model_name_is_wide() {
    use unicode_width::UnicodeWidthStr;

    let ladder = Ladder::builtin(Provider::OpenAi).unwrap();
    let mut routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let stages = &mut routes.get_mut("openai").unwrap().stages;
    for stage in [
        &mut stages.design,
        &mut stages.implement,
        &mut stages.review,
    ] {
        stage.effort_by_model.get_mut("terra").unwrap()[0].model = "模型".into();
    }
    let detail = render_detail(&stages.design.effort_by_model["terra"][0]);
    let lines = render(stages, Some(&ladder));
    let row = lines
        .iter()
        .find(|line| line.contains("terra [模型]"))
        .unwrap();
    let header_prefix = lines[0].split_once("Design").unwrap().0;
    let row_prefix = row.split_once(&detail).unwrap().0;
    assert_eq!(
        UnicodeWidthStr::width(header_prefix),
        UnicodeWidthStr::width(row_prefix)
    );
}

#[test]
fn text_handles_stage_specific_models_and_missing_ladder_metadata() {
    let ladder = custom();
    let routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let lines = render(&routes["custom"].stages, None);
    let design_only = lines
        .iter()
        .find(|line| line.contains("large [vendor.large]"))
        .unwrap();
    assert_eq!(
        design_only
            .split_whitespace()
            .rev()
            .take(2)
            .collect::<Vec<_>>(),
        ["—", "—"]
    );
    let coding = lines
        .iter()
        .find(|line| line.contains("large [vendor.large.coding]"))
        .unwrap();
    assert_eq!(coding.matches("deep (fixed)").count(), 2, "{coding}");
    assert!(lines
        .iter()
        .any(|line| line.contains("No verified control.")));
}

#[test]
fn text_uses_declared_model_order_even_if_stage_results_are_reordered() {
    let ladder = custom();
    let mut routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let stages = &mut routes.get_mut("custom").unwrap().stages;
    stages
        .design
        .effort_by_model
        .get_mut("small,alternative")
        .unwrap()
        .reverse();
    let lines = render(stages, Some(&ladder));
    let labels: Vec<_> = lines
        .iter()
        .filter(|line| line.contains("small,alternative ["))
        .map(String::as_str)
        .collect();
    assert_eq!(labels.len(), 2);
    assert!(labels[0].contains("[vendor.small.v1]"), "{lines:#?}");
    assert!(labels[1].contains("[vendor.fixed]"), "{lines:#?}");
}

#[test]
fn text_preserves_all_statuses_and_scopes_numbered_notes_to_the_table() {
    let ladder = custom();
    let mut routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let stages = &mut routes.get_mut("custom").unwrap().stages;
    let design = &mut stages
        .design
        .effort_by_model
        .get_mut("small,alternative")
        .unwrap()[0];
    design.status = Status::NotNeeded;
    design.level = None;
    design.assessment = None;
    let implement = &mut stages
        .implement
        .effort_by_model
        .get_mut("small,alternative")
        .unwrap()[0];
    implement.status = Status::Insufficient;
    implement.level = None;
    let review = &mut stages
        .review
        .effort_by_model
        .get_mut("small,alternative")
        .unwrap()[0];
    review.status = Status::Unspecified;
    review.level = None;
    review.assessment = None;
    let lines = render(stages, Some(&ladder));
    let small = lines
        .iter()
        .find(|line| line.contains("[vendor.small.v1]"))
        .unwrap();
    assert!(small.contains("not needed"), "{small}");
    assert!(small.contains("insufficient capability"), "{small}");
    assert!(small.contains("unspecified"), "{small}");
    let unavailable = lines
        .iter()
        .find(|line| line.contains("large [vendor.large] "))
        .unwrap();
    assert!(unavailable.contains("unavailable [1]"), "{unavailable}");
    assert!(lines
        .iter()
        .any(|line| line.starts_with("    [1] ") && line.contains("No verified control.")));
    assert_eq!(render(stages, Some(&ladder)), lines);
}

#[test]
fn text_omits_empty_effort_tables() {
    let ladder = custom();
    let mut routes = provider_routes(
        &response(std::slice::from_ref(&ladder)),
        std::slice::from_ref(&ladder),
        0.3,
    )
    .unwrap();
    let stages = &mut routes.get_mut("custom").unwrap().stages;
    stages.design.effort_by_model.clear();
    stages.implement.effort_by_model.clear();
    stages.review.effort_by_model.clear();
    assert!(render(stages, Some(&ladder)).is_empty());
}

#[test]
fn text_keeps_each_providers_effort_table_below_its_class_summary() {
    let ladders = [
        Ladder::builtin(Provider::OpenAi).unwrap(),
        Ladder::builtin(Provider::Anthropic).unwrap(),
    ];
    let routes = provider_routes(&response(&ladders), &ladders, 0.3).unwrap();
    let mut lines = Vec::new();
    for ladder in &ladders {
        lines.extend(super::super::render_provider_line(
            &ladder.name,
            &routes[&ladder.name],
            ladders.len(),
            Some(ladder),
            super::super::TerminalStyle::default(),
        ));
    }
    let text = lines.join("\n");
    let openai = text.find("  openai:").unwrap();
    let anthropic = text.find("  anthropic:").unwrap();
    assert!(openai < text.find("terra [gpt-5.6-terra]").unwrap());
    assert!(text.find("astra [gpt-6-astra]").unwrap() < anthropic);
    assert!(anthropic < text.find("sonnet [claude-sonnet-5]").unwrap());
    assert_eq!(text.matches("Model / effort").count(), 2);
}

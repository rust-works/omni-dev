//! Reproduce class-only vs effort-aware routing on frozen IssueDoc inputs.
//! Usage: cargo run --example jev_route_eval -- INPUT.json OUTPUT_DIR [REPEATS]
//! Uses configured Jev credentials; output never contains credentials.
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use omni_dev::jev::client::JevClient;
use omni_dev::jev::config::JevConfig;
use omni_dev::jev::route::{
    build_route_questions, run_route, Ladder, OpenDependencies, Provider, RouteOptions, Tiers,
    DEFAULT_CLOSE_CALL, DEFAULT_MAX_INPUT_CHARS,
};
use omni_dev::provider::IssueDoc;

fn class_only(ladder: &Ladder) -> Result<Ladder> {
    let tiers: Vec<_> = ladder
        .tiers
        .as_slice()
        .iter()
        .map(|t| serde_json::json!({"name":t.name,"description":t.description}))
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
    let output = PathBuf::from(args.next().context("expected OUTPUT_DIR")?);
    let repeats: usize = args.next().as_deref().unwrap_or("2").parse()?;
    if repeats == 0 || args.next().is_some() {
        bail!("expected INPUT.json OUTPUT_DIR [positive REPEATS]");
    }
    let docs: Vec<IssueDoc> = serde_json::from_slice(&std::fs::read(&input)?)?;
    if output.exists() {
        bail!("OUTPUT_DIR must be new, to avoid mixing different inputs or questions");
    }
    std::fs::create_dir_all(&output)?;
    std::fs::write(
        output.join("inputs.json"),
        serde_json::to_vec_pretty(&docs)?,
    )?;
    let config = JevConfig::from_env()?;
    let client = JevClient::from_config(&config)?;
    let options = RouteOptions {
        model: config.model,
        close_call: DEFAULT_CLOSE_CALL,
        max_input_chars: DEFAULT_MAX_INPUT_CHARS,
        allow_closed: true,
    };
    let builtins = Provider::ALL
        .into_iter()
        .map(Ladder::builtin)
        .collect::<Result<Vec<_>>>()?;
    let mut variants: Vec<(String, Vec<Ladder>)> = builtins
        .iter()
        .map(|l| (l.name.clone(), vec![l.clone()]))
        .collect();
    variants.push(("combined".into(), builtins));
    // Same concrete Anthropic models under versioned rung names exercise custom key handling.
    let mut custom: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../src/templates/jev-route-tiers-anthropic.yaml"
    ))?;
    if let Some(tiers) = custom["tiers"].as_sequence_mut() {
        for tier in tiers {
            tier["name"] = tier["models"][0]["name"].clone();
        }
    }
    variants.push((
        "versioned".into(),
        vec![Ladder::named(
            "versioned".into(),
            Tiers::parse(&serde_yaml::to_string(&custom)?)?,
        )],
    ));
    for (name, ladders) in variants {
        for with_effort in [false, true] {
            let selected = if with_effort {
                ladders.clone()
            } else {
                ladders.iter().map(class_only).collect::<Result<Vec<_>>>()?
            };
            let mode = format!("{name}-{}", if with_effort { "effort" } else { "baseline" });
            std::fs::write(
                output.join(format!("{mode}-questions.json")),
                serde_json::to_vec_pretty(&build_route_questions(&selected)?)?,
            )?;
            for repeat in 0..repeats {
                let path = output.join(format!("{mode}-{repeat}.json"));
                let start = Instant::now();
                let report = run_route(
                    &client,
                    &docs,
                    &selected,
                    &options,
                    &OpenDependencies::new(),
                )
                .await?;
                let elapsed = start.elapsed().as_secs_f64();
                let failures = report.issues.iter().filter(|i| i.failed()).count();
                std::fs::write(
                    path,
                    serde_json::to_vec_pretty(
                        &serde_json::json!({"seconds":elapsed,"report":report}),
                    )?,
                )?;
                println!(
                    "{mode} repeat {repeat}: {} issues, {failures} failures, {elapsed:.1}s",
                    docs.len()
                );
            }
        }
    }
    Ok(())
}

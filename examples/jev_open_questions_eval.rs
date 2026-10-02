//! Compare unchanged stage requests with the added open-question choice on frozen states.
//! Usage: cargo run --example jev_open_questions_eval -- INPUT.json OUTPUT_DIR [REPEATS]
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use omni_dev::jev::client::JevClient;
use omni_dev::jev::config::JevConfig;
use omni_dev::jev::protocol::SystemOneRequest;
use omni_dev::jev::route::{build_route_questions, Ladder, Provider, Tiers};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    id: String,
    state: String,
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
    let cases: Vec<Case> = serde_json::from_slice(&std::fs::read(&input)?)?;
    if cases.is_empty() || output.exists() {
        bail!("inputs must be nonempty and OUTPUT_DIR must be new");
    }
    let config = JevConfig::from_env()?;
    let client = JevClient::from_config(&config)?;
    std::fs::create_dir_all(&output)?;
    std::fs::copy(&input, output.join("inputs.json"))?;
    // Strip effort metadata to isolate the added classifier, across all built-in ladders.
    let ladders =
        Provider::ALL
            .into_iter()
            .map(|provider| {
                let ladder = Ladder::builtin(provider)?;
                let tiers: Vec<_> = ladder.tiers.as_slice().iter().map(|tier| {
            serde_json::json!({"name": tier.name, "description": tier.description})
        }).collect();
                Ok(Ladder::named(
                    ladder.name,
                    Tiers::parse(&serde_json::json!({"tiers": tiers}).to_string())?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
    let tier_order: std::collections::BTreeMap<_, Vec<_>> = ladders
        .iter()
        .map(|ladder| {
            (
                ladder.name.clone(),
                ladder
                    .tiers
                    .as_slice()
                    .iter()
                    .map(|tier| tier.name.clone())
                    .collect(),
            )
        })
        .collect();
    std::fs::write(
        output.join("tier-order.json"),
        serde_json::to_vec_pretty(&tier_order)?,
    )?;
    let augmented = build_route_questions(&ladders)?;
    let mut baseline = augmented.clone();
    baseline.remove("open_questions");
    let mut results = Vec::new();
    for repeat in 0..repeats {
        for case in &cases {
            // Reverse order on alternate repeats to avoid a fixed request-order bias.
            let modes = if repeat % 2 == 0 {
                ["baseline", "augmented"]
            } else {
                ["augmented", "baseline"]
            };
            for mode in modes {
                let request = SystemOneRequest {
                    state: serde_json::Value::String(case.state.clone()),
                    model: config.model.clone(),
                    questions: if mode == "baseline" {
                        baseline.clone()
                    } else {
                        augmented.clone()
                    },
                };
                let response = match client.system_one(&request).await {
                    Ok(response) => serde_json::json!({"response": response}),
                    Err(error) => serde_json::json!({"error": format!("{error:#}")}),
                };
                results.push(serde_json::json!({"id": case.id, "repeat": repeat, "mode": mode, "request": request, "result": response}));
                // Persist every completed call, including errors, so an interrupted run is auditable.
                std::fs::write(
                    output.join("results.json"),
                    serde_json::to_vec_pretty(&results)?,
                )?;
                println!("{} repeat {repeat}: {mode}", case.id);
            }
        }
    }
    Ok(())
}

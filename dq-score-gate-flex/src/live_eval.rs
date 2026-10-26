// Copyright 2026 Salesforce, Inc. All rights reserved.
//! Runs the gate on a real catalog export, outside the repo:
//!
//! ```sh
//! DQ_LIVE_EXPORT=/path/export.json cargo test --lib live_eval -- --ignored --nocapture
//! ```
//!
//! The export is `{"server": name, "tools": [[location, name], ...], "rules": [hit, ...]}`
//! with the `sourceAsMap`s of the MCP Server's tools and of the rule occurrences under
//! its location, as `fetch_scores` queries them.

use serde_json::Value;

use crate::gate::{evaluate, Settings};
use crate::rules::build;

#[test]
#[ignore]
fn live_eval() {
    let path = std::env::var("DQ_LIVE_EXPORT").expect("DQ_LIVE_EXPORT");
    let export: Value = serde_json::from_slice(&std::fs::read(path).expect("read export")).expect("parse export");
    let tools: Vec<(String, String)> = export["tools"].as_array().expect("tools").iter()
        .filter_map(|t| Some((t[0].as_str()?.to_string(), t[1].as_str()?.to_string()))).collect();
    let hits = export["rules"].as_array().cloned().unwrap_or_default();
    let cards = build(export["server"].as_str().map(str::to_string), &tools, &hits);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    for (tool, rules) in &cards.tools {
        let v = evaluate(tool, rules, &Settings::default(), now);
        println!("{tool}: {} score={:?} rules={}", v.status(), v.score, rules.len());
        for r in &v.reasons {
            println!("    {r}");
        }
    }
}

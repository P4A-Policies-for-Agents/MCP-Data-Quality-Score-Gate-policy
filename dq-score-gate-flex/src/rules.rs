// Copyright 2026 Salesforce, Inc. All rights reserved.
//! CDGC data quality rule occurrences (`RuleInstance`) and the tools they score.
//!
//! A rule occurrence sits under the asset it measures: its `core.location` is the
//! measured asset's location plus the occurrence id. Under an MCP Server that asset is
//! an MCP Tool or one of its output fields, so a rule belongs to the tool whose
//! location is the longest prefix of its own, and the rest of the parent location is
//! the field path.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RULE_CLASS: &str = "com.infa.ccgf.models.governance.RuleInstance";
const G: &str = "com.infa.ccgf.models.governance.";

/// A score a rule had before the current one.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Previous {
    pub score: f64,
    pub scanned_ms: Option<i64>,
}

/// One rule occurrence and its latest measurement.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RuleScore {
    pub id: String,
    pub reference: Option<String>,
    pub name: Option<String>,
    /// Output field path, e.g. `orders[].customer_email`; none for a tool-level rule.
    pub field: Option<String>,
    pub dimension: Option<String>,
    pub score: Option<f64>,
    pub threshold: Option<f64>,
    pub target: Option<f64>,
    /// CDGC's own verdict, e.g. `Good` or `Not Acceptable`.
    pub result: Option<String>,
    pub scanned_ms: Option<i64>,
    pub criticality: Option<String>,
    pub records: Option<f64>,
    /// The score before the last change this gateway saw.
    #[serde(default)]
    pub previous: Option<Previous>,
}

/// The MCP server's tools and their rules. A catalogued tool without rules maps to
/// an empty list, so "unscored" differs from "unknown tool".
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Scorecards {
    pub server_name: Option<String>,
    pub tools: BTreeMap<String, Vec<RuleScore>>,
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().trim_end_matches('%').trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn text(map: &Value, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn attr<'a>(map: &'a Value, name: &str) -> Option<&'a Value> {
    map.get(format!("{G}{name}").as_str())
}

/// Parse one search hit (`sourceAsMap`) of a rule occurrence, without its field.
pub fn parse_rule(hit: &Value) -> Option<RuleScore> {
    Some(RuleScore {
        id: text(hit, "core.identity")?,
        reference: text(hit, "core.externalId"),
        name: text(hit, "core.name"),
        field: None,
        dimension: attr(hit, "RuleType").and_then(Value::as_str).map(str::to_string),
        score: num(attr(hit, "value")),
        threshold: num(attr(hit, "Threshold")),
        target: num(attr(hit, "Target")),
        result: attr(hit, "thresholdResult").and_then(Value::as_str).map(str::to_string),
        scanned_ms: num(attr(hit, "scannedTime")).map(|f| f as i64),
        criticality: attr(hit, "Criticality").and_then(Value::as_str).map(str::to_string),
        records: num(attr(hit, "totalCount")),
        previous: None,
    })
}

/// The measured asset's location: the rule's parent.
fn parent_location(hit: &Value) -> Option<String> {
    text(hit, "__meta.parentLocation").or_else(|| {
        let loc = text(hit, "core.location")?;
        loc.rsplit_once('/').map(|(p, _)| p.to_string())
    })
}

/// Assign rule hits to tools. `tools` are (location, name) pairs.
pub fn build(server_name: Option<String>, tools: &[(String, String)], rule_hits: &[Value]) -> Scorecards {
    let mut cards = Scorecards { server_name, tools: BTreeMap::new() };
    for (_, name) in tools {
        cards.tools.entry(name.clone()).or_default();
    }
    for hit in rule_hits {
        let (Some(mut rule), Some(parent)) = (parse_rule(hit), parent_location(hit)) else { continue };
        let owner = tools.iter()
            .filter(|(loc, _)| parent == *loc || parent.starts_with(&format!("{loc}/")))
            .max_by_key(|(loc, _)| loc.len());
        let Some((loc, tool)) = owner else { continue };
        rule.field = parent.strip_prefix(&format!("{loc}/")).map(str::to_string);
        cards.tools.entry(tool.clone()).or_default().push(rule);
    }
    for rules in cards.tools.values_mut() {
        rules.sort_by(|a, b| (&a.field, &a.dimension, &a.id).cmp(&(&b.field, &b.dimension, &b.id)));
    }
    cards
}

/// Keep what the gateway knew before: a rule whose score changed since the last
/// refresh records the old score as `previous`; an unchanged one keeps its previous.
pub fn carry_history(old: Option<&Scorecards>, mut new: Scorecards) -> Scorecards {
    let Some(old) = old else { return new };
    let before: BTreeMap<&str, &RuleScore> =
        old.tools.values().flatten().map(|r| (r.id.as_str(), r)).collect();
    for rule in new.tools.values_mut().flatten() {
        let Some(prev) = before.get(rule.id.as_str()) else { continue };
        rule.previous = match prev.score {
            Some(score) if prev.score != rule.score => Some(Previous { score, scanned_ms: prev.scanned_ms }),
            _ => prev.previous.clone(),
        };
    }
    new
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SERVER: &str = "origin://origin/Server";

    fn tools() -> Vec<(String, String)> {
        vec![
            (format!("{SERVER}/search_orders"), "search_orders".to_string()),
            (format!("{SERVER}/search_orders_v2"), "search_orders_v2".to_string()),
            (format!("{SERVER}/check_stock"), "check_stock".to_string()),
        ]
    }

    fn hit(id: &str, parent: &str, score: Value) -> Value {
        json!({
            "core.identity": id,
            "core.externalId": "DQO-1",
            "core.name": "Email format",
            "core.location": format!("{parent}/{id}"),
            "com.infa.ccgf.models.governance.RuleType": "Validity",
            "com.infa.ccgf.models.governance.value": score,
            "com.infa.ccgf.models.governance.Threshold": 95,
            "com.infa.ccgf.models.governance.Target": "98",
            "com.infa.ccgf.models.governance.thresholdResult": "Not Acceptable",
            "com.infa.ccgf.models.governance.scannedTime": 1_700_000_000_000_i64,
            "com.infa.ccgf.models.governance.Criticality": "High",
            "com.infa.ccgf.models.governance.totalCount": 12
        })
    }

    #[test]
    fn parses_numbers_and_strings() {
        let r = parse_rule(&hit("r1", SERVER, json!("61.5 %"))).unwrap();
        assert_eq!(r.score, Some(61.5));
        assert_eq!(r.threshold, Some(95.0));
        assert_eq!(r.target, Some(98.0));
        assert_eq!(r.scanned_ms, Some(1_700_000_000_000));
        assert_eq!(r.dimension.as_deref(), Some("Validity"));
        assert_eq!(r.result.as_deref(), Some("Not Acceptable"));
        assert_eq!(r.reference.as_deref(), Some("DQO-1"));
        assert!(parse_rule(&json!({"core.name": "no id"})).is_none());
        assert_eq!(parse_rule(&hit("r2", SERVER, json!("n/a"))).unwrap().score, None);
    }

    #[test]
    fn assigns_rules_to_the_longest_tool_prefix_and_field() {
        let hits = vec![
            hit("r1", &format!("{SERVER}/search_orders/orders[].customer_email"), json!(61.5)),
            hit("r2", &format!("{SERVER}/search_orders_v2/orders[].iban"), json!(99)),
            hit("r3", &format!("{SERVER}/search_orders"), json!(80)),
            hit("r4", "origin://origin/Other/tool/field", json!(10)),
        ];
        let c = build(Some("Server".into()), &tools(), &hits);
        let so = &c.tools["search_orders"];
        assert_eq!(so.len(), 2);
        assert_eq!(so[0].field, None, "tool-level rule sorts first");
        assert_eq!(so[1].field.as_deref(), Some("orders[].customer_email"));
        assert_eq!(c.tools["search_orders_v2"][0].field.as_deref(), Some("orders[].iban"));
        assert!(c.tools["check_stock"].is_empty(), "catalogued but unscored");
        assert_eq!(c.tools.values().flatten().count(), 3, "rules outside the server are dropped");
    }

    #[test]
    fn prefers_the_parent_location_attribute() {
        let mut h = hit("r1", "ignored", json!(50));
        h["__meta.parentLocation"] = json!(format!("{SERVER}/check_stock/stock_items[].sku"));
        let c = build(None, &tools(), &[h]);
        assert_eq!(c.tools["check_stock"][0].field.as_deref(), Some("stock_items[].sku"));
    }

    fn card(score: Option<f64>, scanned: i64) -> Scorecards {
        let mut r = parse_rule(&hit("r1", SERVER, json!(0))).unwrap();
        r.score = score;
        r.scanned_ms = Some(scanned);
        let mut c = Scorecards::default();
        c.tools.insert("t".into(), vec![r]);
        c
    }

    #[test]
    fn records_the_previous_score_when_it_changes() {
        let first = carry_history(None, card(Some(61.5), 1));
        assert_eq!(first.tools["t"][0].previous, None);

        let second = carry_history(Some(&first), card(Some(98.7), 2));
        assert_eq!(second.tools["t"][0].previous, Some(Previous { score: 61.5, scanned_ms: Some(1) }));

        // Unchanged score: the before/after pair survives further refreshes.
        let third = carry_history(Some(&second), card(Some(98.7), 2));
        assert_eq!(third.tools["t"][0].previous, Some(Previous { score: 61.5, scanned_ms: Some(1) }));

        // Re-measured with the same score: nothing new to compare, keep the pair.
        let fourth = carry_history(Some(&third), card(Some(98.7), 3));
        assert_eq!(fourth.tools["t"][0].previous, Some(Previous { score: 61.5, scanned_ms: Some(1) }));

        // A rule that had no score yet has no previous score.
        let fifth = carry_history(Some(&card(None, 1)), card(Some(80.0), 2));
        assert_eq!(fifth.tools["t"][0].previous, None);
    }
}

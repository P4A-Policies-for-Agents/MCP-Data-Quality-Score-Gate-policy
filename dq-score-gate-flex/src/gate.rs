// Copyright 2026 Salesforce, Inc. All rights reserved.
//! The gate: a tool's rule scores → pass, warn or block, and the `_dq` report.

use serde_json::{json, Value};

use crate::rules::RuleScore;

pub const POLICY: &str = "mcp-data-quality-score-gate";
const HOUR_MS: i64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Pass,
    Warn,
    Block,
}

impl Level {
    /// `allow|ignore` → Pass, `warn` → Warn, `block` → Block; anything else → `default`.
    fn parse(s: Option<&str>, default: Level) -> Level {
        match s.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("allow") | Some("ignore") => Level::Pass,
            Some("warn") => Level::Warn,
            Some("block") => Level::Block,
            _ => default,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aggregation {
    Minimum,
    Average,
}

impl Aggregation {
    fn name(self) -> &'static str {
        match self {
            Aggregation::Minimum => "minimum",
            Aggregation::Average => "average",
        }
    }
    fn apply(self, scores: &[f64]) -> Option<f64> {
        if scores.is_empty() {
            return None;
        }
        Some(match self {
            Aggregation::Minimum => scores.iter().copied().fold(f64::INFINITY, f64::min),
            Aggregation::Average => scores.iter().sum::<f64>() / scores.len() as f64,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub warn_below: f64,
    pub block_below: f64,
    pub aggregation: Aggregation,
    /// Lower-cased rule types that count; empty = all.
    pub dimensions: Vec<String>,
    pub honour_rule_thresholds: bool,
    /// None = scores never go stale.
    pub max_age_ms: Option<i64>,
    pub stale: Level,
    pub unscored: Level,
    /// false = annotate only: a would-be block is reported as a warning.
    pub enforce_block: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            warn_below: 90.0,
            block_below: 70.0,
            aggregation: Aggregation::Minimum,
            dimensions: Vec::new(),
            honour_rule_thresholds: true,
            max_age_ms: Some(168 * HOUR_MS),
            stale: Level::Warn,
            unscored: Level::Pass,
            enforce_block: true,
        }
    }
}

impl Settings {
    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        warn_below: Option<f64>,
        block_below: Option<f64>,
        aggregation: Option<&str>,
        dimensions: Option<&[String]>,
        honour_rule_thresholds: Option<bool>,
        max_score_age_hours: Option<i64>,
        stale_action: Option<&str>,
        unscored_action: Option<&str>,
        enforcement: Option<&str>,
    ) -> Settings {
        let d = Settings::default();
        let clamp = |v: Option<f64>, def: f64| v.filter(|f| f.is_finite()).map(|f| f.clamp(0.0, 100.0)).unwrap_or(def);
        let warn_below = clamp(warn_below, d.warn_below);
        Settings {
            warn_below,
            // A block threshold above the warning one would block without warning first.
            block_below: clamp(block_below, d.block_below).min(warn_below),
            aggregation: match aggregation.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
                Some("average") => Aggregation::Average,
                _ => Aggregation::Minimum,
            },
            dimensions: dimensions.unwrap_or_default().iter()
                .map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()).collect(),
            honour_rule_thresholds: honour_rule_thresholds.unwrap_or(d.honour_rule_thresholds),
            max_age_ms: match max_score_age_hours {
                Some(h) if h <= 0 => None,
                Some(h) => Some(h.saturating_mul(HOUR_MS)),
                None => d.max_age_ms,
            },
            stale: Level::parse(stale_action, d.stale),
            unscored: Level::parse(unscored_action, d.unscored),
            enforce_block: !enforcement.is_some_and(|e| e.trim().eq_ignore_ascii_case("annotate")),
        }
    }
}

/// One counted rule, as the gate saw it.
#[derive(Clone, Debug)]
pub struct RuleView {
    pub rule: RuleScore,
    pub stale: bool,
    pub below_threshold: bool,
}

#[derive(Clone, Debug)]
pub struct Verdict {
    pub tool: String,
    pub level: Level,
    /// The tool has no scored rules (that count).
    pub unscored: bool,
    /// A block that `enforcement: annotate` turned into a warning.
    pub would_block: bool,
    pub score: Option<f64>,
    /// The tool score from each rule's previous score, when any rule changed.
    pub previous_score: Option<f64>,
    pub rules: Vec<RuleView>,
    pub reasons: Vec<String>,
    pub last_scanned_ms: Option<i64>,
    aggregation: Aggregation,
    warn_below: f64,
    block_below: f64,
    enforce_block: bool,
}

/// A score for people: at most one decimal, no trailing `.0`.
pub fn fmt(score: f64) -> String {
    let r = (score * 10.0).round() / 10.0;
    if r.fract() == 0.0 { format!("{r:.0}") } else { format!("{r:.1}") }
}

fn label(r: &RuleScore) -> String {
    let what = r.dimension.as_deref().unwrap_or("rule");
    let rule = r.reference.as_deref().unwrap_or(&r.id);
    match &r.field {
        Some(f) => format!("{f} {what} ({rule})"),
        None => format!("{what} ({rule})"),
    }
}

fn age(ms: i64) -> String {
    let h = ms / HOUR_MS;
    if h >= 48 { format!("{} days", h / 24) } else { format!("{h} hours") }
}

pub fn evaluate(tool: &str, rules: &[RuleScore], s: &Settings, now_ms: i64) -> Verdict {
    let counted: Vec<RuleView> = rules.iter()
        .filter(|r| s.dimensions.is_empty()
            || r.dimension.as_deref().is_some_and(|d| s.dimensions.contains(&d.trim().to_ascii_lowercase())))
        .map(|r| {
            let stale = s.max_age_ms.is_some_and(|max| r.scanned_ms.is_none_or(|t| now_ms - t > max));
            let below_threshold = s.honour_rule_thresholds && r.score.is_some() && (
                matches!((r.score, r.threshold), (Some(v), Some(t)) if v < t)
                || r.result.as_deref().is_some_and(|x| x.trim().eq_ignore_ascii_case("not acceptable")));
            RuleView { rule: r.clone(), stale, below_threshold }
        })
        .collect();

    let scores: Vec<f64> = counted.iter().filter_map(|v| v.rule.score).collect();
    let score = s.aggregation.apply(&scores);
    let previous_score = if counted.iter().any(|v| v.rule.previous.is_some()) {
        let before: Vec<f64> = counted.iter()
            .filter_map(|v| v.rule.previous.as_ref().map(|p| p.score).or(v.rule.score)).collect();
        s.aggregation.apply(&before)
    } else {
        None
    };
    let last_scanned_ms = counted.iter().filter_map(|v| v.rule.scanned_ms).max();

    let mut level = Level::Pass;
    let mut reasons = Vec::new();
    let unscored = score.is_none();
    match score {
        None => {
            level = s.unscored;
            if level > Level::Pass {
                reasons.push(format!("{tool} has no data quality score in the catalog"));
            }
        }
        Some(v) if v < s.block_below => {
            level = Level::Block;
            reasons.push(format!("Tool score {} is below the block threshold {}", fmt(v), fmt(s.block_below)));
        }
        Some(v) if v < s.warn_below => {
            level = Level::Warn;
            reasons.push(format!("Tool score {} is below the warning threshold {}", fmt(v), fmt(s.warn_below)));
        }
        Some(_) => {}
    }
    for v in &counted {
        if v.below_threshold {
            level = level.max(Level::Warn);
            let (score, threshold) = (v.rule.score.map(fmt).unwrap_or_default(), v.rule.threshold.map(fmt));
            reasons.push(match threshold {
                Some(t) => format!("{} scores {score}, below its threshold {t}", label(&v.rule)),
                None => format!("{} scores {score}: Not Acceptable", label(&v.rule)),
            });
        }
        if v.stale && v.rule.score.is_some() && s.stale > Level::Pass {
            level = level.max(s.stale);
            reasons.push(match v.rule.scanned_ms {
                Some(t) => format!("{} was last measured {} ago", label(&v.rule), age(now_ms - t)),
                None => format!("{} has no measurement time", label(&v.rule)),
            });
        }
    }
    let would_block = level == Level::Block && !s.enforce_block;
    if would_block {
        level = Level::Warn;
    }
    Verdict {
        tool: tool.to_string(),
        level,
        unscored,
        would_block,
        score,
        previous_score,
        rules: counted,
        reasons,
        last_scanned_ms,
        aggregation: s.aggregation,
        warn_below: s.warn_below,
        block_below: s.block_below,
        enforce_block: s.enforce_block,
    }
}

impl Verdict {
    /// `x-dq-status`: pass, warn, blocked or unscored.
    pub fn status(&self) -> &'static str {
        match self.level {
            Level::Block => "blocked",
            Level::Warn => "warn",
            Level::Pass if self.unscored => "unscored",
            Level::Pass => "pass",
        }
    }

    fn summary(&self) -> String {
        let n = self.rules.iter().filter(|v| v.rule.score.is_some()).count();
        match self.score {
            Some(v) => format!("{} scores {} ({} of {n} rule{})", self.tool, fmt(v), self.aggregation.name(),
                if n == 1 { "" } else { "s" }),
            None => format!("{} has no data quality score", self.tool),
        }
    }

    /// The JSON-RPC error message of a refused call.
    pub fn block_message(&self) -> String {
        let mut m = format!("Refused by the data quality gate: {}.", self.summary());
        for r in &self.reasons {
            m.push(' ');
            m.push_str(r);
            m.push('.');
        }
        m.push_str(" The data owner must fix the data and re-run the data quality rules before agents can use it.");
        m
    }

    /// Guidance for the agent when the result is delivered with a warning.
    pub fn notice(&self) -> Option<String> {
        (self.level == Level::Warn).then(|| format!(
            "Data quality warning: {}. {}. Tell the user that this result may be unreliable.",
            self.summary(), self.reasons.join(". ")))
    }

    /// The `_dq` report.
    pub fn to_json(&self) -> Value {
        let rules: Vec<Value> = self.rules.iter().map(|v| {
            let r = &v.rule;
            json!({
                "rule": r.reference.as_deref().unwrap_or(&r.id),
                "name": r.name,
                "field": r.field,
                "dimension": r.dimension,
                "score": r.score,
                "previousScore": r.previous.as_ref().map(|p| p.score),
                "threshold": r.threshold,
                "target": r.target,
                "result": r.result,
                "criticality": r.criticality,
                "records": r.records,
                "scannedAt": r.scanned_ms,
                "stale": v.stale,
                "belowThreshold": v.below_threshold,
            })
        }).collect();
        let mut out = json!({
            "policy": POLICY,
            "source": "cdgc",
            "tool": self.tool,
            "status": self.status(),
            "score": self.score,
            "previousScore": self.previous_score,
            "aggregation": self.aggregation.name(),
            "thresholds": {"warn": self.warn_below, "block": self.block_below},
            "enforcement": if self.enforce_block { "block" } else { "annotate" },
            "wouldBlock": self.would_block,
            "lastScannedAt": self.last_scanned_ms,
            "reasons": self.reasons,
            "rules": rules,
        });
        if let Some(n) = self.notice() {
            out["notice"] = json!(n);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::Previous;

    const NOW: i64 = 1_800_000_000_000;

    fn rule(id: &str, field: &str, dim: &str, score: Option<f64>, threshold: Option<f64>) -> RuleScore {
        RuleScore {
            id: id.into(),
            reference: Some(format!("DQO-{id}")),
            name: None,
            field: Some(field.into()),
            dimension: Some(dim.into()),
            score,
            threshold,
            target: None,
            result: None,
            scanned_ms: Some(NOW - HOUR_MS),
            criticality: None,
            records: None,
            previous: None,
        }
    }

    #[test]
    fn passes_good_data() {
        let v = evaluate("t", &[rule("1", "a", "Validity", Some(99.0), Some(95.0))], &Settings::default(), NOW);
        assert_eq!((v.level, v.status()), (Level::Pass, "pass"));
        assert!(v.reasons.is_empty());
        assert!(v.to_json().get("notice").is_none());
    }

    #[test]
    fn minimum_blocks_on_the_weakest_rule_and_average_warns() {
        let rules = [rule("1", "a", "Validity", Some(61.5), None), rule("2", "b", "Completeness", Some(100.0), None)];
        let v = evaluate("t", &rules, &Settings::default(), NOW);
        assert_eq!((v.status(), v.score), ("blocked", Some(61.5)));
        assert!(v.block_message().contains("t scores 61.5 (minimum of 2 rules)"), "{}", v.block_message());

        let avg = Settings { aggregation: Aggregation::Average, ..Settings::default() };
        let v = evaluate("t", &rules, &avg, NOW);
        assert_eq!((v.status(), v.score), ("warn", Some(80.75)));
        assert!(v.notice().unwrap().contains("Tell the user"));
    }

    #[test]
    fn a_rule_below_its_own_threshold_warns() {
        let v = evaluate("t", &[rule("1", "orders[].email", "Validity", Some(92.0), Some(95.0))], &Settings::default(), NOW);
        assert_eq!(v.status(), "warn");
        assert_eq!(v.reasons, vec!["orders[].email Validity (DQO-1) scores 92, below its threshold 95"]);

        let mut r = rule("1", "a", "Validity", Some(99.0), None);
        r.result = Some("Not Acceptable".into());
        assert_eq!(evaluate("t", &[r.clone()], &Settings::default(), NOW).status(), "warn");
        let ignore = Settings { honour_rule_thresholds: false, ..Settings::default() };
        assert_eq!(evaluate("t", &[r], &ignore, NOW).status(), "pass");
    }

    #[test]
    fn stale_scores_follow_stale_action() {
        let mut r = rule("1", "a", "Validity", Some(99.0), None);
        r.scanned_ms = Some(NOW - 200 * HOUR_MS);
        let v = evaluate("t", &[r.clone()], &Settings::default(), NOW);
        assert_eq!(v.status(), "warn");
        assert!(v.reasons[0].contains("last measured 8 days ago"), "{:?}", v.reasons);
        assert!(v.rules[0].stale);

        let block = Settings { stale: Level::Block, ..Settings::default() };
        assert_eq!(evaluate("t", &[r.clone()], &block, NOW).status(), "blocked");
        let never = Settings { max_age_ms: None, ..Settings::default() };
        assert_eq!(evaluate("t", &[r], &never, NOW).status(), "pass");
    }

    #[test]
    fn unscored_tools_follow_unscored_action() {
        let s = Settings::default();
        let v = evaluate("t", &[], &s, NOW);
        assert_eq!((v.status(), v.score), ("unscored", None));
        let unmeasured = [rule("1", "a", "Validity", None, Some(95.0))];
        assert_eq!(evaluate("t", &unmeasured, &s, NOW).status(), "unscored", "a rule without a value doesn't count");
        let block = Settings { unscored: Level::Block, ..s };
        let v = evaluate("t", &[], &block, NOW);
        assert_eq!(v.status(), "blocked");
        assert!(v.block_message().contains("t has no data quality score"));
    }

    #[test]
    fn dimension_filter() {
        let rules = [rule("1", "a", "Validity", Some(10.0), None), rule("2", "b", "Completeness", Some(99.0), None)];
        let s = Settings { dimensions: vec!["completeness".into()], ..Settings::default() };
        let v = evaluate("t", &rules, &s, NOW);
        assert_eq!((v.status(), v.rules.len()), ("pass", 1));
    }

    #[test]
    fn annotate_turns_a_block_into_a_warning() {
        let s = Settings { enforce_block: false, ..Settings::default() };
        let v = evaluate("t", &[rule("1", "a", "Validity", Some(40.0), None)], &s, NOW);
        assert_eq!((v.status(), v.would_block), ("warn", true));
        let j = v.to_json();
        assert_eq!(j["wouldBlock"], true);
        assert_eq!(j["enforcement"], "annotate");
    }

    #[test]
    fn reports_before_and_after() {
        let mut fixed = rule("1", "a", "Validity", Some(98.7), Some(95.0));
        fixed.previous = Some(Previous { score: 61.5, scanned_ms: Some(1) });
        let other = rule("2", "b", "Completeness", Some(100.0), None);
        let v = evaluate("t", &[fixed, other], &Settings::default(), NOW);
        assert_eq!((v.status(), v.score, v.previous_score), ("pass", Some(98.7), Some(61.5)));
        let j = v.to_json();
        assert_eq!(j["previousScore"], 61.5);
        assert_eq!(j["rules"][0]["previousScore"], 61.5);
        assert_eq!(j["rules"][1]["previousScore"], Value::Null);
        assert_eq!(j["policy"], POLICY);
    }

    #[test]
    fn settings_from_config() {
        let s = Settings::from_config(Some(80.0), Some(95.0), Some("Average"), Some(&[" Validity ".to_string()]),
            Some(false), Some(0), Some("block"), Some("warn"), Some("annotate"));
        assert_eq!(s.block_below, 80.0, "block threshold capped at the warning one");
        assert_eq!(s.aggregation, Aggregation::Average);
        assert_eq!(s.dimensions, vec!["validity"]);
        assert_eq!((s.max_age_ms, s.stale, s.unscored, s.enforce_block), (None, Level::Block, Level::Warn, false));
        let d = Settings::from_config(None, None, None, None, None, None, Some("bogus"), None, None);
        assert_eq!((d.warn_below, d.block_below, d.stale, d.enforce_block), (90.0, 70.0, Level::Warn, true));
        assert_eq!(Settings::from_config(Some(500.0), None, None, None, None, None, None, None, None).warn_below, 100.0);
    }

    #[test]
    fn formats_scores() {
        assert_eq!((fmt(61.5), fmt(98.0), fmt(80.749)), ("61.5".into(), "98".into(), "80.7".into()));
    }
}

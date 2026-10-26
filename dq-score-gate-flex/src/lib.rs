// Copyright 2026 Salesforce, Inc. All rights reserved.
//! MCP Data Quality Score Gate — inbound Omni/Flex Gateway policy.
//!
//! Gates MCP `tools/call` on the data quality scores Informatica CDGC records for the
//! called tool. The scores are the **rule occurrences** (`RuleInstance`, DQO-n)
//! catalogued under the configured MCP Server: on an MCP Tool or on one of its output
//! fields. Each carries its latest score (0-100), its own threshold and target, the
//! rule type (dimension) and when it was measured. Rules measured by Cloud Data
//! Quality and externally measured ones are read the same way.
//!
//! **Request leg** (`tools/call` only): the tool's rule scores combine into a tool
//! score (`aggregation`) and a verdict (see `gate`). Below `blockBelow` the call is
//! refused with a JSON-RPC error (`data.policy` = this policy) before it reaches the
//! upstream, so no low-quality data is fetched. `enforcement: annotate` never refuses.
//!
//! **Response leg**: the verdict rides in a `_dq` report in the tool result and in
//! `x-dq-*` headers. A warning carries a `notice` telling the agent to caution the
//! user.
//!
//! The scores are cached (`scoreRefreshSeconds`, lazy refresh, single-flight). Each
//! refresh compares with the cached scores and keeps a rule's previous score when it
//! changed, so `_dq` reports before and after a data fix. A failed refresh keeps using
//! stale scores; with none, `failOpenOnIdmcError` decides.

mod gate;
mod generated;
#[cfg(test)]
mod live_eval;
mod payload;
mod rules;

use std::rc::Rc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use pdk::data_storage::{DataStorage, DataStorageBuilder, DataStorageError, StoreMode};
use pdk::hl::timer::Clock;
use pdk::hl::*;
use pdk::logger;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gate::{fmt, Level, Settings, Verdict, POLICY};
use crate::generated::config::Config;
use crate::payload::{annotate, called_tool, frame, parse_rpc, rpc_error};
use crate::rules::{Scorecards, RULE_CLASS};

const CACHE_NAMESPACE: &str = "dq-cache";
const LOCK_NAMESPACE: &str = "dq-refresh-lock";
const SCORES_KEY_PREFIX: &str = "dq-scores-";
const LOCK_KEY_PREFIX: &str = "dq-lock-";
const REFRESH_LOCK_TTL_SECONDS: i64 = 30;
const REFRESH_LOCK_TTL_MS: u32 = (REFRESH_LOCK_TTL_SECONDS as u32) * 1000;
const STORE_TTL_MS: u32 = 7 * 24 * 60 * 60 * 1000;
const CAS_MAX_RETRIES: u32 = 3;
const DEFAULT_TIMEOUT_MS: i64 = 5_000;
/// Budget for one chained refresh (login, JWT, then the searches).
const REFRESH_BUDGET_MS: i64 = 15_000;
const DEFAULT_SCORE_REFRESH_SECONDS: i64 = 300;
const MAX_TOOLS: usize = 1_000;
const MAX_RULES: usize = 5_000;

const SEARCH_PATH: &str = "/ccgf-searchv2/api/v1/search";
const DEFAULT_TOOL_CLASS_TYPES: &[&str] = &["a2d.agentic.McpTool"];

const STATUS_HEADER: &str = "x-dq-status";
const SCORE_HEADER: &str = "x-dq-score";
const PREVIOUS_HEADER: &str = "x-dq-previous-score";
const RULES_HEADER: &str = "x-dq-rules";
const RPC_ERR_DATA_QUALITY: i64 = -32010;
const RPC_ERR_UNAVAILABLE: i64 = -32603;

#[derive(Deserialize)]
struct LoginResponse {
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(rename = "orgId")]
    org_id: String,
}
#[derive(Deserialize)]
struct JwtResponse {
    jwt_token: String,
}

/// An authenticated IDMC session.
struct Session {
    jwt: String,
    org: String,
}

/// A cached value and when it was fetched (Unix seconds).
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Cached<T> {
    value: T,
    timestamp: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct RefreshLock {
    acquired_at: i64,
}

/// What the request leg hands the response leg. `verdict` is none when the scores
/// were unavailable and the call was let through.
#[derive(Clone)]
struct Ctx {
    tool: String,
    verdict: Option<Verdict>,
}

fn now_secs(clock: &Clock) -> i64 {
    clock.now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
fn now_ms(clock: &Clock) -> i64 {
    clock.now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Per-call timeouts within one refresh budget.
struct Budget<'a> {
    clock: &'a Clock,
    start: SystemTime,
    per_call_ms: i64,
}

impl Budget<'_> {
    fn next(&self, what: &str) -> Result<Duration> {
        let elapsed = self.clock.now().duration_since(self.start).map(|d| d.as_millis() as i64).unwrap_or(0);
        let remaining = REFRESH_BUDGET_MS - elapsed;
        if remaining <= 0 {
            return Err(anyhow!("IDMC refresh budget spent before {what}"));
        }
        Ok(Duration::from_millis(self.per_call_ms.min(remaining).max(1) as u64))
    }
}

fn nonce(now: SystemTime) -> String {
    now.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos().to_string()).unwrap_or_else(|_| "0".to_string())
}

/// IDMC Login → JWT.
async fn login(client: &HttpClient, config: &Config, b: &Budget<'_>) -> Result<Session> {
    let body = serde_json::to_vec(&json!({"username": config.cdgc_org_username, "password": config.cdgc_org_password}))?;
    let resp = client.request(&config.cdgc_login_url).path("/identity-service/api/v1/Login")
        .headers(vec![("Content-Type", "application/json")]).body(&body).timeout(b.next("login")?).post().await
        .map_err(|e| anyhow!("IDMC login failed: {e}"))?;
    if resp.status_code() >= 300 {
        return Err(anyhow!("IDMC login status {}", resp.status_code()));
    }
    let l: LoginResponse = serde_json::from_slice(resp.body()).map_err(|e| anyhow!("parse login: {e}"))?;
    let cookie = format!("USER_SESSION={}", l.session_id);
    let resp = client.request(&config.cdgc_login_url)
        .path(&format!("/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce={}", nonce(b.clock.now())))
        .headers(vec![("cookie", cookie.as_str()), ("IDS-SESSION-ID", l.session_id.as_str())])
        .timeout(b.next("JWT")?).get().await.map_err(|e| anyhow!("IDMC JWT failed: {e}"))?;
    if resp.status_code() >= 300 {
        return Err(anyhow!("IDMC JWT status {}", resp.status_code()));
    }
    let j: JwtResponse = serde_json::from_slice(resp.body()).map_err(|e| anyhow!("parse jwt: {e}"))?;
    Ok(Session { jwt: j.jwt_token, org: l.org_id })
}

/// One ccgf-searchv2 query; returns the hits' `sourceAsMap`s.
async fn search(client: &HttpClient, config: &Config, b: &Budget<'_>, s: &Session, query: Value, size: usize) -> Result<Vec<Value>> {
    let authz = format!("Bearer {}", s.jwt);
    let payload = serde_json::to_vec(&json!({"from": 0, "size": size, "query": query}))?;
    let resp = client.request(&config.cdgc_api_url).path(SEARCH_PATH)
        .headers(vec![
            ("Authorization", authz.as_str()),
            ("X-INFA-ORG-ID", s.org.as_str()),
            ("X-INFA-SEARCH-LANGUAGE", "elasticsearch"),
            ("Content-Type", "application/json"),
        ])
        .body(&payload).timeout(b.next("search")?).post().await
        .map_err(|e| anyhow!("CDGC search failed: {e}"))?;
    if resp.status_code() >= 300 {
        return Err(anyhow!("CDGC search status {}", resp.status_code()));
    }
    let v: Value = serde_json::from_slice(resp.body()).map_err(|e| anyhow!("parse search: {e}"))?;
    Ok(v.pointer("/hits/hits").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|h| h.get("sourceAsMap").cloned()).collect())
        .unwrap_or_default())
}

fn s(map: &Value, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

fn tool_class_types(config: &Config) -> Vec<String> {
    let v: Vec<String> = config.tool_class_types.as_deref().unwrap_or_default().iter()
        .map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if v.is_empty() { DEFAULT_TOOL_CLASS_TYPES.iter().map(|s| s.to_string()).collect() } else { v }
}

/// The MCP server's tools and the rule occurrences under them.
async fn fetch_scores(client: &HttpClient, config: &Config, clock: &Clock) -> Result<Scorecards> {
    let b = Budget { clock, start: clock.now(), per_call_ms: config.timeout.unwrap_or(DEFAULT_TIMEOUT_MS) };
    let sess = login(client, config, &b).await?;
    let server_id = config.mcp_server_asset_id.trim();
    let server = search(client, config, &b, &sess, json!({"bool": {"must": [
        {"terms": {"elementType": ["OBJECT"]}}, {"terms": {"core.identity": [server_id]}}]}}), 1).await?
        .into_iter().next().ok_or_else(|| anyhow!("MCP server asset '{server_id}' not found"))?;
    let location = s(&server, "core.location").ok_or_else(|| anyhow!("MCP server asset has no core.location"))?;
    let tools = search(client, config, &b, &sess, json!({"bool": {
        "must": [{"terms": {"core.classType": tool_class_types(config)}}],
        "filter": [{"terms": {"core.location::path_hierarchy.parent": [location.clone()]}}]}}), MAX_TOOLS).await?;
    // A rule occurrence's location extends the measured asset's, so one prefix query
    // finds every rule on the server's tools and fields.
    let rule_hits = search(client, config, &b, &sess, json!({"bool": {
        "must": [{"terms": {"core.classType": [RULE_CLASS]}}],
        "filter": [{"prefix": {"core.location": format!("{location}/")}}]}}), MAX_RULES).await?;
    let tools: Vec<(String, String)> = tools.iter()
        .filter_map(|t| Some((s(t, "core.location")?, s(t, "core.name")?.trim().to_string()))).collect();
    Ok(rules::build(s(&server, "core.name"), &tools, &rule_hits))
}

// ---- cache ----

async fn read_cached<S: DataStorage, T: DeserializeOwned>(store: &S, key: &str) -> Option<Cached<T>> {
    match store.get::<Cached<T>>(key).await {
        Ok(Some((c, _))) => Some(c),
        Ok(None) => None,
        Err(e) => {
            logger::warn!("dq: cache read failed for {key}: {e}");
            None
        }
    }
}
async fn write_cached<S: DataStorage, T: Serialize + DeserializeOwned>(store: &S, key: &str, entry: &Cached<T>) {
    for _ in 0..CAS_MAX_RETRIES {
        let mode = match store.get::<Cached<T>>(key).await {
            Ok(Some((_, v))) => StoreMode::Cas(v),
            Ok(None) => StoreMode::Absent,
            Err(e) => { logger::warn!("dq: read-before-persist failed for {key}: {e}"); return; }
        };
        match store.store(key, &mode, entry).await {
            Ok(()) => return,
            Err(DataStorageError::CasMismatch) => continue,
            Err(e) => { logger::warn!("dq: persist failed for {key}: {e}"); return; }
        }
    }
}
async fn try_acquire_refresh_lock<S: DataStorage>(store: &S, key: &str, now: i64) -> Result<bool, DataStorageError> {
    let entry = RefreshLock { acquired_at: now };
    match store.store(key, &StoreMode::Absent, &entry).await {
        Ok(()) => Ok(true),
        Err(DataStorageError::CasMismatch) => match store.get::<RefreshLock>(key).await? {
            Some((existing, v)) if now - existing.acquired_at >= REFRESH_LOCK_TTL_SECONDS => {
                match store.store(key, &StoreMode::Cas(v), &entry).await {
                    Ok(()) => Ok(true),
                    Err(DataStorageError::CasMismatch) => Ok(false),
                    Err(e) => Err(e),
                }
            }
            Some(_) => Ok(false),
            None => match store.store(key, &StoreMode::Absent, &entry).await {
                Ok(()) => Ok(true),
                Err(DataStorageError::CasMismatch) => Ok(false),
                Err(e) => Err(e),
            },
        },
        Err(e) => Err(e),
    }
}

/// The server's scorecards, refreshed when older than `scoreRefreshSeconds`. Only one
/// replica/worker refreshes stale scores while the others keep serving them; with
/// nothing cached, every caller fetches. A refresh carries each rule's previous score
/// forward; a failed one falls back to the stale scores.
async fn scorecards<S: DataStorage>(store: &S, locks: &S, config: &Config, client: &HttpClient, clock: &Clock) -> Option<Scorecards> {
    let key = format!("{SCORES_KEY_PREFIX}{}", config.mcp_server_asset_id.trim());
    let ttl = config.score_refresh_seconds.unwrap_or(DEFAULT_SCORE_REFRESH_SECONDS);
    let now = now_secs(clock);
    let current = read_cached::<S, Scorecards>(store, &key).await;
    if let Some(c) = &current {
        if now - c.timestamp < ttl {
            return Some(c.value.clone());
        }
        if !try_acquire_refresh_lock(locks, &format!("{LOCK_KEY_PREFIX}{key}"), now).await.unwrap_or(true) {
            return Some(c.value.clone());
        }
    }
    match fetch_scores(client, config, clock).await {
        Ok(fresh) => {
            let value = rules::carry_history(current.as_ref().map(|c| &c.value), fresh);
            write_cached(store, &key, &Cached { value: value.clone(), timestamp: now }).await;
            Some(value)
        }
        Err(e) => {
            logger::warn!("dq: refresh of {key} failed: {e}");
            current.map(|c| c.value)
        }
    }
}

fn settings(config: &Config) -> Settings {
    Settings::from_config(
        config.warn_below,
        config.block_below,
        config.aggregation.as_deref(),
        config.dimensions.as_deref(),
        config.honour_rule_thresholds,
        config.max_score_age_hours,
        config.stale_action.as_deref(),
        config.unscored_action.as_deref(),
        config.enforcement.as_deref(),
    )
}

fn verdict_headers(v: &Verdict) -> Vec<(String, String)> {
    let mut h = vec![
        (STATUS_HEADER.to_string(), v.status().to_string()),
        (RULES_HEADER.to_string(), v.rules.len().to_string()),
    ];
    if let Some(score) = v.score {
        h.push((SCORE_HEADER.to_string(), fmt(score)));
    }
    if let Some(prev) = v.previous_score {
        h.push((PREVIOUS_HEADER.to_string(), fmt(prev)));
    }
    h
}

fn refuse(status: u32, headers: Vec<(String, String)>, body: Value) -> Response {
    let mut h = vec![("content-type".to_string(), "application/json".to_string())];
    h.extend(headers);
    Response::new(status).with_headers(h).with_body(body.to_string())
}

// ---- request leg ----

async fn request_filter<S: DataStorage>(
    request_state: RequestState,
    config: Rc<Config>,
    client: Rc<HttpClient>,
    clock: Rc<Clock>,
    store: Rc<S>,
    locks: Rc<S>,
) -> Flow<Option<Ctx>> {
    let state = request_state.into_headers_body_state().await;
    let h = state.handler();
    let ct = h.header("content-type").unwrap_or_default();
    if !ct.starts_with("application/json") || !state.contains_body() {
        return Flow::Continue(None);
    }
    let Ok(rpc) = serde_json::from_slice::<Value>(&h.body()) else { return Flow::Continue(None) };
    let Some(tool) = called_tool(&rpc).map(str::to_string) else { return Flow::Continue(None) };
    let rpc_id = rpc.get("id").cloned().unwrap_or(Value::Null);

    let Some(cards) = scorecards(&*store, &*locks, &config, &client, &clock).await else {
        if config.fail_open_on_idmc_error.unwrap_or(true) {
            logger::warn!("dq: no scores (IDMC unavailable); tool={tool} passed through ungated");
            return Flow::Continue(Some(Ctx { tool, verdict: None }));
        }
        return Flow::Break(refuse(503, vec![(STATUS_HEADER.to_string(), "unavailable".to_string())],
            rpc_error(&rpc_id, RPC_ERR_UNAVAILABLE, "Data quality could not be verified: IDMC is unavailable.",
                json!({"policy": POLICY, "tool": tool, "status": "unavailable"}))));
    };
    let Some(rules) = cards.tools.get(&tool) else {
        logger::info!("dq: tool '{tool}' is not an MCP Tool of '{}' in CDGC; not gated",
            cards.server_name.as_deref().unwrap_or(config.mcp_server_asset_id.trim()));
        return Flow::Continue(None);
    };
    let verdict = gate::evaluate(&tool, rules, &settings(&config), now_ms(&clock));
    logger::info!("dq: tool={tool} status={} score={:?} rules={}", verdict.status(), verdict.score, verdict.rules.len());
    if verdict.level == Level::Block {
        let report = verdict.to_json();
        return Flow::Break(refuse(422, verdict_headers(&verdict),
            rpc_error(&rpc_id, RPC_ERR_DATA_QUALITY, &verdict.block_message(), json!({
                "policy": POLICY,
                "tool": tool,
                "status": verdict.status(),
                "score": verdict.score,
                "blockBelow": report["thresholds"]["block"],
                "report": report,
            }))));
    }
    Flow::Continue(Some(Ctx { tool, verdict: Some(verdict) }))
}

// ---- response leg ----

async fn response_filter(response_state: ResponseState, request_data: RequestData<Option<Ctx>>) {
    let RequestData::Continue(Some(ctx)) = request_data else { return };
    let hs = response_state.into_headers_state().await;
    let ct = hs.handler().header("content-type").unwrap_or_default();
    let is_sse = ct.contains("event-stream");
    let (headers, report, notice) = match &ctx.verdict {
        Some(v) => (verdict_headers(v), v.to_json(), v.notice()),
        None => (vec![(STATUS_HEADER.to_string(), "unavailable".to_string())],
            json!({"policy": POLICY, "source": "cdgc", "tool": ctx.tool, "status": "unavailable"}), None),
    };
    for (k, v) in &headers {
        hs.handler().set_header(k, v);
    }
    if !is_sse && !ct.contains("json") {
        return;
    }
    hs.handler().remove_header("content-length");
    hs.handler().remove_header("content-encoding");
    let bs = hs.into_body_state().await;
    let Ok(text) = String::from_utf8(bs.handler().body()) else { return };
    let Some(mut rpc) = parse_rpc(&text, is_sse) else { return };
    if annotate(&mut rpc, &report, notice.as_deref()) {
        if let Err(e) = bs.handler().set_body(frame(&rpc, is_sse).as_bytes()) {
            logger::warn!("dq: set_body failed: {e:?}");
        }
    }
}

fn launch_policy<S: DataStorage + 'static>(
    launcher: Launcher, config: Rc<Config>, client: Rc<HttpClient>, clock: Rc<Clock>, store: Rc<S>, locks: Rc<S>,
) -> impl std::future::Future<Output = Result<()>> {
    let filter = on_request(move |rs| {
        let (c, cl, ck, st, lk) = (config.clone(), client.clone(), clock.clone(), store.clone(), locks.clone());
        async move { request_filter(rs, c, cl, ck, st, lk).await }
    })
    .on_response(response_filter);
    async move { launcher.launch(filter).await.map_err(Into::into) }
}

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    client: HttpClient,
    storage_builder: DataStorageBuilder,
    clock: Clock,
) -> Result<()> {
    // Never echo the configuration: it carries the service-account password.
    let config: Config = serde_json::from_slice(&bytes).map_err(|err| anyhow!("Failed to parse configuration: {err}"))?;
    let (config, client, clock) = (Rc::new(config), Rc::new(client), Rc::new(clock));
    if config.distributed.unwrap_or(false) {
        let store = Rc::new(storage_builder.remote(CACHE_NAMESPACE, STORE_TTL_MS));
        let locks = Rc::new(storage_builder.remote(LOCK_NAMESPACE, REFRESH_LOCK_TTL_MS));
        launch_policy(launcher, config, client, clock, store, locks).await
    } else {
        let store = Rc::new(storage_builder.local(CACHE_NAMESPACE));
        let locks = Rc::new(storage_builder.local(LOCK_NAMESPACE));
        launch_policy(launcher, config, client, clock, store, locks).await
    }
}

# MCP Data Quality Score Gate — MuleSoft Omni/Flex Gateway Policy

An inbound custom policy (PDK 1.10, Rust → wasm32-wasip1) that gates MCP `tools/call`
on the data quality scores Informatica Data Governance and Catalog (CDGC) records for
the called tool. Low-quality data never reaches the agent: below `blockBelow` the call
is refused before the upstream MCP server is called. Between `blockBelow` and
`warnBelow` the result is delivered with a warning the agent passes to the user.

The scores are CDGC **data quality rule occurrences** (`RuleInstance`, `DQO-n`)
catalogued on an MCP Tool or on one of its output fields. A rule measured by Cloud Data
Quality and an externally measured one are read the same way. Each carries:

- its latest score (0–100);
- its own threshold and target;
- its rule type (the dimension: Validity, Completeness, Accuracy, …);
- its criticality, record count and measurement time.

## Flow

```
agent ──tools/call──▶ gateway ─┬─ JWT validation, CDAM, Entitlement Filter, …
                               └─ DQ gate (request leg)
                                    │ tool's rule scores (cached, from CDGC search)
                                    │ → tool score → pass | warn | block
                                    ├─ block → 422 + JSON-RPC -32010, upstream not called
                                    └─ pass/warn → upstream MCP server
                                                     │
                               DQ gate (response leg) ◀┘
                                    adds the `_dq` report and `x-dq-*` headers
```

1. The request leg reads the JSON-RPC body. Only `tools/call` is gated; `initialize`,
   `tools/list` and the rest pass through.
2. It looks up the called tool among the MCP Server's tools in CDGC. A tool that is not
   catalogued under the configured server is not gated, and the reason is logged.
3. It combines the tool's rule scores into a tool score and reaches a verdict (see
   [Scoring](#scoring)).
4. A blocked call is answered by the gateway. Otherwise the response leg adds the verdict
   to the result.

## Finding the scores

Three CDGC searches (`/ccgf-searchv2/api/v1/search`) run per refresh:

| Search | Query |
|---|---|
| The MCP Server | `core.identity` = `mcpServerAssetId` |
| Its tools | `core.classType` in `toolClassTypes`, with `core.location::path_hierarchy.parent` = the server's location |
| The rule occurrences under it | `core.classType` = `com.infa.ccgf.models.governance.RuleInstance`, with `core.location` prefixed by the server's location |

A rule occurrence's location extends the location of the asset it measures. A rule
belongs to the tool whose location is the longest prefix of the rule's parent location.
The rest of that location is the field path, e.g. `orders[].customer_email`.

The scorecards are cached for `scoreRefreshSeconds`. Refresh is lazy and single-flight:
one worker, or one replica with `distributed: true`, refreshes while the others keep
serving the cached scores. Each refresh compares the new scores with the cached ones.
When a rule's score changed, the old score is kept as its previous score. That gives the
`_dq` report a before → after view once the data is fixed and the rules re-run.

## Scoring

| Step | Rule |
|---|---|
| Rules counted | All the tool's rules, or only those whose rule type is in `dimensions` |
| Tool score | `minimum` (default: the weakest rule decides) or `average` of the counted scores |
| Block | Tool score < `blockBelow` (default 70) |
| Warn | Tool score < `warnBelow` (default 90) |
| Warn | `honourRuleThresholds` and a rule below its own threshold, or CDGC says `Not Acceptable` |
| Stale | A rule measured more than `maxScoreAgeHours` ago (default 168) → `staleAction` (`ignore`, `warn` (default) or `block`) |
| Unscored | A catalogued tool with no scored rule → `unscoredAction` (`allow` (default), `warn` or `block`) |
| Annotate mode | `enforcement: annotate` never refuses: a block becomes a warning with `wouldBlock: true` |

`blockBelow` is capped at `warnBelow`. Both are clamped to 0–100.

## What the client receives

**Refused call** (HTTP 422):

```json
{"jsonrpc": "2.0", "id": 7, "error": {
  "code": -32010,
  "message": "Refused by the data quality gate: search_sales_orders scores 61.5 (minimum of 4 rules). Tool score 61.5 is below the block threshold 70. … The data owner must fix the data and re-run the data quality rules before agents can use it.",
  "data": {"policy": "mcp-data-quality-score-gate", "tool": "search_sales_orders",
           "status": "blocked", "score": 61.5, "blockBelow": 70, "report": {"…": "the _dq report"}}}}
```

**Delivered result**: the `_dq` report is added to `structuredContent` and to every text
content item that holds a JSON object. A result with only free text gets the warning as
an extra text item, `[Data quality warning: …]`.

```json
"_dq": {
  "policy": "mcp-data-quality-score-gate", "source": "cdgc",
  "tool": "search_sales_orders", "status": "pass",
  "score": 98.7, "previousScore": 61.5, "aggregation": "minimum",
  "thresholds": {"warn": 90, "block": 70}, "enforcement": "block", "wouldBlock": false,
  "lastScannedAt": 1793081560000,
  "reasons": [],
  "rules": [{"rule": "DQO-1", "name": "Customer email format", "field": "orders[].customer_email",
             "dimension": "Validity", "score": 98.7, "previousScore": 61.5, "threshold": 95,
             "target": 98, "result": "Good", "criticality": "High", "records": 250,
             "scannedAt": 1793081560000, "stale": false, "belowThreshold": false}]
}
```

Times are Unix epoch milliseconds. A warning adds `"notice": "Data quality warning: …. Tell the user that this result may be
unreliable."`.

**Headers** on every gated call:

| Header | Value |
|---|---|
| `x-dq-status` | `pass`, `warn`, `blocked`, `unscored` or `unavailable` |
| `x-dq-score` | Tool score, one decimal |
| `x-dq-previous-score` | Tool score before the last change, when one was seen |
| `x-dq-rules` | Number of rules counted |

## Failure handling

| Case | Behaviour |
|---|---|
| A refresh fails, scores cached | The cached scores are used, however old |
| A refresh fails, nothing cached | `failOpenOnIdmcError: true` (default): passed through with `x-dq-status: unavailable`. `false`: HTTP 503, JSON-RPC -32603 |
| Tool not catalogued under the server | Not gated |
| Non-JSON or non-`tools/call` request | Not gated |

Fail-open is the default. A quality gate that takes every tool down when IDMC is
unreachable does more harm than the data it protects against. Use `failOpenOnIdmcError:
false` where the data must never be used unverified.

## Configuration

| Property | Default | |
|---|---|---|
| `cdgcLoginUrl` | — | IDMC login host, e.g. `https://dm-us.informaticacloud.com` |
| `cdgcApiUrl` | — | CDGC API host, e.g. `https://cdgc-api.dm-us.informaticacloud.com` |
| `cdgcOrgUsername`, `cdgcOrgPassword` | — | IDMC service account with read access to the catalog (sensitive) |
| `mcpServerAssetId` | — | CDGC asset id of the MCP Server |
| `toolClassTypes` | `[a2d.agentic.McpTool]` | Class types of the server's tools |
| `warnBelow` / `blockBelow` | 90 / 70 | Tool score thresholds |
| `aggregation` | `minimum` | `minimum` or `average` |
| `dimensions` | `[]` (all) | Rule types to count |
| `honourRuleThresholds` | `true` | Warn when a rule is below its own threshold |
| `maxScoreAgeHours` | 168 | 0 = never stale |
| `staleAction` | `warn` | `ignore`, `warn`, `block` |
| `unscoredAction` | `allow` | `allow`, `warn`, `block` |
| `enforcement` | `block` | `block` or `annotate` (never refuse) |
| `scoreRefreshSeconds` | 300 | Cache lifetime, at least 30 |
| `failOpenOnIdmcError` | `true` | See [Failure handling](#failure-handling) |
| `distributed` | `false` | Share the cache across replicas |
| `timeout` | 5000 | Per IDMC call (ms); one refresh is capped at 15 s |

### Chain order

Put the gate after the policies that authenticate and authorize the caller: JWT
validation, CDAM Data Access Enforcement, MCP Tool Entitlement Filter. That way an
unauthorized call is refused for authorization reasons, not data quality ones. The gate
does not change the result's data, so its position relative to response-rewriting
policies does not matter.

### Limits

- Only rules catalogued under the MCP Server's tools and output fields count. Rules on
  the source tables an output field is lineaged to are not followed yet.
- The previous score is what this gateway's cache saw. It starts empty after a restart,
  or per replica without `distributed`.

## Build

```sh
cd dq-score-gate-definition && make release-local     # after editing gcl.yaml
cd ../dq-score-gate-flex
cargo anypoint config-gen --manifest ../dq-score-gate-definition/gcl.yaml --output ./src/generated/config.rs
cargo test --lib                                         # unit tests
make build                                               # wasm
make run                                                 # local playground (playground/)
make publish / make release                              # Exchange
```

## Evaluating against a real tenant (offline)

`src/live_eval.rs` runs the gate on an export of the server's tools and rules, without a
gateway. Keep the export outside the repo.

```sh
DQ_LIVE_EXPORT=/path/export.json cargo test --lib live_eval -- --ignored --nocapture
```

## Demo (`demo/`)

`demo/dq_scenes.py` creates demo rule occurrences on the demo MCP Server and switches
their scores between a *before* (broken data), *degraded* and *after* (fixed data)
scene. See [demo/README.md](demo/README.md).

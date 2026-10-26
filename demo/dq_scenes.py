#!/usr/bin/env python3
# Copyright 2026 Salesforce, Inc. All rights reserved.
"""Demo data quality scores for the MCP Data Quality Score Gate.

Creates externally measured data quality rule occurrences (CDGC RuleInstance, DQO-n)
on the output fields of the demo MCP Server's tools, and switches their scores
between scenes, so the gate can be shown before and after a data fix without running
a Cloud Data Quality job.

    python3 dq_scenes.py provision          # create the demo rules (scene "after")
    python3 dq_scenes.py before             # broken data: search_sales_orders is blocked
    python3 dq_scenes.py degraded           # one rule below its own threshold: warning
    python3 dq_scenes.py after              # fixed data: passes, trace shows before → after
    python3 dq_scenes.py status             # the scores and the verdict the gate reaches
    python3 dq_scenes.py teardown           # delete the demo rules

Reads the gateway policy configuration from config.json next to this script (CDGC
URLs, service account, mcpServerAssetId, thresholds). The service account needs
write access to the catalog for provision, scenes and teardown. Nothing secret is
printed.
"""

import json
import os
import sys
import time

import requests  # pip install requests

G = "com.infa.ccgf.models.governance."
RULE_CLASS = G + "RuleInstance"
REL_FIELD_RULE = G + "asscParentDataElementRuleInstance"
TOOL_CLASSES = ["a2d.agentic.McpTool"]
FIELD_CLASS = "a2d.agentic.McpToolOutputField"
NAME_PREFIX = "DQ Gate demo · "

# (tool name suffix, field, dimension, rule name, threshold, target, criticality, scores per scene)
RULES = [
    ("search_sales_orders", "orders[].customer_email", "Validity", "Customer email format",
     95, 98, "High", {"broken": 61.5, "degraded": 93.2, "fixed": 98.7}),
    ("search_sales_orders", "orders[].shipping_country", "Completeness", "Shipping country populated",
     98, 100, "Medium", {"broken": 88.0, "degraded": 99.1, "fixed": 100}),
    ("search_sales_orders", "orders[].order_total", "Accuracy", "Order total equals quantity × unit price",
     99, 100, "High", {"broken": 97.0, "degraded": 99.4, "fixed": 99.6}),
    ("search_sales_orders", "orders[].customer_id", "Uniqueness", "Customer id unique per customer",
     100, 100, "Medium", {"broken": 100, "degraded": 100, "fixed": 100}),
    ("check_inventory", "stock_items[].available_qty", "Validity", "Available quantity not negative",
     99, 100, "High", {"broken": 100, "degraded": 100, "fixed": 100}),
    ("check_inventory", "stock_items[].last_counted_at", "Completeness", "Last stock count date populated",
     95, 99, "Low", {"broken": 97.5, "degraded": 97.5, "fixed": 97.5}),
]
SCENES = {"before": "broken", "broken": "broken", "degraded": "degraded", "after": "fixed", "fixed": "fixed"}
RECORDS = 250

CFG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.json")


def load_config():
    try:
        with open(CFG_PATH) as f:
            return json.load(f)
    except FileNotFoundError:
        sys.exit(f"Missing {CFG_PATH}: copy config.json.example and fill it in.")


CFG = load_config()


def http(method, url, body=None, headers=None):
    r = requests.request(method, url, json=body, headers=headers or {}, timeout=60)
    if r.status_code >= 300 and r.status_code != 207:
        # Status only: IDMC error bodies can echo request details.
        sys.exit(f"{method} {url.split('?')[0]} failed: HTTP {r.status_code}")
    return r.status_code, (r.json() if r.content else None)


def login():
    base = CFG["cdgcLoginUrl"].rstrip("/")
    _, l = http("POST", base + "/identity-service/api/v1/Login",
                {"username": CFG["cdgcOrgUsername"], "password": CFG["cdgcOrgPassword"]})
    _, j = http("GET", f"{base}/identity-service/api/v1/jwt/Token?client_id=idmc_api&nonce={time.time_ns()}",
                headers={"cookie": f"USER_SESSION={l['sessionId']}", "IDS-SESSION-ID": l["sessionId"]})
    return {"Authorization": f"Bearer {j['jwt_token']}", "X-INFA-ORG-ID": l["orgId"], "Accept": "application/json"}


AUTH = login()
API = CFG["cdgcApiUrl"].rstrip("/")


def search(query, size=500):
    h = dict(AUTH, **{"X-INFA-SEARCH-LANGUAGE": "elasticsearch"})
    _, v = http("POST", API + "/ccgf-searchv2/api/v1/search", {"from": 0, "size": size, "query": query}, h)
    return [x.get("sourceAsMap", {}) for x in (v or {}).get("hits", {}).get("hits", [])]


def publish(items):
    _, v = http("POST", API + "/ccgf-contentv2/api/v1/publish", {"items": items},
                dict(AUTH, **{"X-INFA-PRODUCT-ID": "CDGC"}))
    failed = [i for i in (v or {}).get("items", []) if int(i.get("statusCode") or 0) >= 300]
    for i in failed:
        print(f"  ! {i.get('elementType')} {i.get('operation')} {i.get('statusCode')} {i.get('messageCode')}: "
              + "; ".join(str(x.get("message", x)) for x in i.get("validations") or []))
    if failed:
        sys.exit("Publish failed.")
    return v


def under(classes, parents):
    return {"bool": {"must": [{"terms": {"core.classType": classes}}],
                     "filter": [{"terms": {"core.location::path_hierarchy.parent": parents}}]}}


def server():
    sid = CFG["mcpServerAssetId"].strip()
    hits = search({"bool": {"must": [{"terms": {"elementType": ["OBJECT"]}}, {"terms": {"core.identity": [sid]}}]}}, 1)
    if not hits:
        sys.exit("The MCP Server asset was not found: check mcpServerAssetId.")
    return hits[0]


def demo_rules(srv):
    hits = search({"bool": {"must": [{"terms": {"core.classType": [RULE_CLASS]}}],
                            "filter": [{"prefix": {"core.location": srv["core.location"] + "/"}}]}}, 1000)
    return [h for h in hits if str(h.get("core.name", "")).startswith(NAME_PREFIX)]


def tools_and_fields(srv):
    tools = search(under(TOOL_CLASSES, [srv["core.location"]]))
    fields = search(under([FIELD_CLASS], [t["core.location"] for t in tools]), 5000) if tools else []
    by_loc = {f["core.location"]: f for f in fields}
    return tools, fields, by_loc


def resolve(tools, fields):
    """Each demo rule's (tool, field) assets."""
    out = []
    for spec in RULES:
        suffix, field = spec[0], spec[1]
        tool = next((t for t in tools if t["core.name"].endswith(suffix)), None)
        if not tool:
            sys.exit(f"No MCP Tool ending in '{suffix}' under the server.")
        f = next((x for x in fields if x["core.name"] == field
                  and x["core.location"].startswith(tool["core.location"] + "/")), None)
        if not f:
            sys.exit(f"No output field '{field}' under {tool['core.name']}.")
        out.append((spec, tool, f))
    return out


def wait_for(srv, check, what, timeout=90):
    """Poll search until the index reflects a publish."""
    start = time.time()
    while time.time() - start < timeout:
        rules = demo_rules(srv)
        if check(rules):
            return rules
        time.sleep(5)
    print(f"  (the search index hasn't caught up with {what} yet; re-run status in a minute)")
    return demo_rules(srv)


def result_of(score, threshold):
    return "Good" if score >= threshold else "Not Acceptable"


def provision():
    srv = server()
    tools, fields, _ = tools_and_fields(srv)
    existing = {r["core.name"] for r in demo_rules(srv)}
    now = int(time.time() * 1000)
    items = []
    for n, (spec, tool, f) in enumerate(resolve(tools, fields)):
        _, field, dim, name, threshold, target, crit, scores = spec
        full = NAME_PREFIX + name
        if full in existing:
            continue
        pid = f"dq-gate-demo-{n}"
        score = scores["fixed"]
        items += [
            {"elementType": "OBJECT", "operation": "INSERT", "type": RULE_CLASS, "identity": pid,
             "identityType": "PROVISIONAL", "attributes": {
                 "core.name": full, "core.origin": srv["core.origin"],
                 G + "RuleType": dim, G + "MeasuringMethod": "TechnicalScript", G + "isExternal": True,
                 G + "Criticality": crit, G + "Frequency": "Daily", G + "Threshold": threshold, G + "Target": target,
                 G + "TechnicalDescription": f"Demo rule for the MCP Data Quality Score Gate on {field}.",
                 G + "value": score, G + "thresholdResult": result_of(score, threshold),
                 G + "totalCount": RECORDS, G + "scannedTime": now}},
            {"elementType": "RELATIONSHIP", "operation": "INSERT", "type": REL_FIELD_RULE,
             "fromIdentity": f["core.identity"], "toIdentity": pid,
             "sourceIdentityType": "INTERNAL", "targetIdentityType": "PROVISIONAL", "attributes": {}},
        ]
    if not items:
        print("All demo rules exist.")
        return
    publish(items)
    print(f"Created {len(items) // 2} demo rule(s); waiting for the search index…")
    wait_for(srv, lambda rules: len(rules) >= len(RULES), "the new rules")
    status()


def scene(name):
    key = SCENES[name]
    srv = server()
    rules = demo_rules(srv)
    if len(rules) < len(RULES):
        sys.exit("Demo rules missing: run provision first.")
    specs = {NAME_PREFIX + s[3]: s for s in RULES}
    now = int(time.time() * 1000)
    items, want = [], {}
    for r in rules:
        spec = specs.get(r["core.name"])
        if not spec:
            continue
        score, threshold = spec[7][key], spec[4]
        want[r["core.identity"]] = score
        attrs = {G + "value": score, G + "thresholdResult": result_of(score, threshold), G + "scannedTime": now}
        items.append({"elementType": "OBJECT", "operation": "UPDATE", "type": RULE_CLASS,
                      "identity": r["core.identity"], "identityType": "INTERNAL", "attributes": attrs,
                      "attributeUpdateOperationTypeMap": {k: "OVERWRITE" for k in attrs}})
    publish(items)
    print(f"Scene '{name}' ({key}) published; waiting for the search index…")
    wait_for(srv, lambda rs: all(float(r.get(G + "value", -1)) == want.get(r["core.identity"], -1)
                                 for r in rs if r["core.identity"] in want), f"scene '{name}'")
    status()
    ttl = CFG.get("scoreRefreshSeconds", 300)
    print(f"\nThe gateway picks this up within scoreRefreshSeconds ({ttl}s) of its last refresh.")


def verdict(scores, rules):
    """The gate's verdict for the configured thresholds (staleness aside: scenes are fresh)."""
    warn, block = float(CFG.get("warnBelow", 90)), float(CFG.get("blockBelow", 70))
    block = min(block, warn)
    if not scores:
        return "unscored"
    agg = round(sum(scores) / len(scores) if CFG.get("aggregation") == "average" else min(scores), 1)
    if agg < block:
        if CFG.get("enforcement", "block") == "annotate":
            return f"warn, would block ({agg:g} < {block:g})"
        return f"blocked ({agg:g} < {block:g})"
    if agg < warn:
        return f"warn ({agg:g} < {warn:g})"
    if CFG.get("honourRuleThresholds", True) and any(score < threshold for score, threshold in rules):
        return f"warn ({agg:g}, a rule is below its own threshold)"
    return f"pass ({agg:g})"


def status():
    srv = server()
    tools, _, by_loc = tools_and_fields(srv)
    hits = search({"bool": {"must": [{"terms": {"core.classType": [RULE_CLASS]}}],
                            "filter": [{"prefix": {"core.location": srv["core.location"] + "/"}}]}}, 1000)
    print(f"\n{srv.get('core.name')}: {len(hits)} data quality rule(s)")
    for t in sorted(tools, key=lambda t: t["core.name"]):
        mine = [h for h in hits if h.get("core.location", "").startswith(t["core.location"] + "/")]
        rows, scores = [], []
        for h in sorted(mine, key=lambda h: h.get("core.location", "")):
            parent = h.get("__meta.parentLocation") or h["core.location"].rsplit("/", 1)[0]
            field = by_loc.get(parent, {}).get("core.name", "(tool)")
            v, th = h.get(G + "value"), h.get(G + "Threshold")
            if isinstance(v, (int, float)):
                scores.append(float(v))
                rows.append((float(v), float(th) if isinstance(th, (int, float)) else 0.0))
            print(f"    {h.get('core.externalId', '?'):>7}  {field:<32} {h.get(G + 'RuleType', '?'):<13}"
                  f" {v!s:>6} / {th!s:<5} {h.get(G + 'thresholdResult', '')}")
        print(f"  {t['core.name']}: {verdict(scores, rows)}")


def teardown():
    srv = server()
    rules = demo_rules(srv)
    if not rules:
        print("No demo rules.")
        return
    _, _, by_loc = tools_and_fields(srv)
    items = []
    for r in rules:
        parent = r.get("__meta.parentLocation") or r["core.location"].rsplit("/", 1)[0]
        f = by_loc.get(parent)
        if f:
            items.append({"elementType": "RELATIONSHIP", "operation": "DELETE", "type": REL_FIELD_RULE,
                          "fromIdentity": f["core.identity"], "toIdentity": r["core.identity"],
                          "sourceIdentityType": "INTERNAL", "targetIdentityType": "INTERNAL", "attributes": {}})
        items.append({"elementType": "OBJECT", "operation": "DELETE", "type": RULE_CLASS,
                      "identity": r["core.identity"], "identityType": "INTERNAL", "attributes": {}})
    publish(items)
    print(f"Deleted {len(rules)} demo rule(s).")


if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else "status"
    if cmd == "provision":
        provision()
    elif cmd in SCENES:
        scene(cmd)
    elif cmd == "status":
        status()
    elif cmd == "teardown":
        teardown()
    else:
        sys.exit(__doc__)

// Copyright 2026 Salesforce, Inc. All rights reserved.
//! JSON-RPC framing and the `_dq` annotation of tool results.

use serde_json::{json, Value};

pub const ANNOTATION: &str = "_dq";

pub fn parse_rpc(text: &str, is_sse: bool) -> Option<Value> {
    if !is_sse {
        return serde_json::from_str(text).ok();
    }
    text.lines()
        .filter_map(|l| l.trim_start().strip_prefix("data:"))
        .find_map(|d| serde_json::from_str::<Value>(d.trim()).ok())
}

pub fn frame(rpc: &Value, is_sse: bool) -> String {
    let j = rpc.to_string();
    if is_sse { format!("event: message\ndata: {j}\n\n") } else { j }
}

pub fn called_tool(rpc: &Value) -> Option<&str> {
    if rpc.get("method").and_then(Value::as_str) != Some("tools/call") {
        return None;
    }
    rpc.pointer("/params/name")?.as_str().map(str::trim).filter(|s| !s.is_empty())
}

pub fn rpc_error(id: &Value, code: i64, message: &str, data: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})
}

/// Add `report` as `_dq` to every JSON object the result carries: `structuredContent`
/// and each JSON text item. A result with only free text gets `notice` (if any) as an
/// extra text item, so the agent still sees the warning. Returns whether anything
/// changed.
pub fn annotate(rpc: &mut Value, report: &Value, notice: Option<&str>) -> bool {
    let Some(result) = rpc.get_mut("result").and_then(Value::as_object_mut) else { return false };
    let mut annotated = false;
    if let Some(sc) = result.get_mut("structuredContent").and_then(Value::as_object_mut) {
        sc.insert(ANNOTATION.to_string(), report.clone());
        annotated = true;
    }
    let mut has_text = false;
    if let Some(items) = result.get_mut("content").and_then(Value::as_array_mut) {
        for item in items.iter_mut().filter(|i| i.get("type").and_then(Value::as_str) == Some("text")) {
            let Some(t) = item.get("text").and_then(Value::as_str) else { continue };
            has_text = true;
            if let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(t) {
                obj.insert(ANNOTATION.to_string(), report.clone());
                item["text"] = Value::String(Value::Object(obj).to_string());
                annotated = true;
            }
        }
        if !annotated && has_text {
            if let Some(n) = notice {
                items.push(json!({"type": "text", "text": format!("[{n}]")}));
                return true;
            }
        }
    }
    annotated
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Value {
        json!({"status": "warn", "score": 80})
    }

    #[test]
    fn annotates_structured_content_and_its_text_mirror() {
        let mut rpc = json!({"jsonrpc": "2.0", "id": 1, "result": {
            "structuredContent": {"orders": []},
            "content": [{"type": "text", "text": "{\"orders\":[]}"}]}});
        assert!(annotate(&mut rpc, &report(), Some("n")));
        assert_eq!(rpc["result"]["structuredContent"]["_dq"]["score"], 80);
        let text: Value = serde_json::from_str(rpc["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text["_dq"]["status"], "warn");
        assert_eq!(rpc["result"]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn free_text_gets_the_notice() {
        let mut rpc = json!({"result": {"content": [{"type": "text", "text": "12 orders found"}]}});
        assert!(annotate(&mut rpc, &report(), Some("Data quality warning: x")));
        assert_eq!(rpc["result"]["content"][1]["text"], "[Data quality warning: x]");

        let mut quiet = json!({"result": {"content": [{"type": "text", "text": "12 orders found"}]}});
        assert!(!annotate(&mut quiet, &report(), None));
        assert_eq!(quiet["result"]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn leaves_errors_and_arrays_alone() {
        let mut err = json!({"error": {"code": 1}});
        assert!(!annotate(&mut err, &report(), Some("n")));
        let mut arr = json!({"result": {"content": [{"type": "text", "text": "[1,2]"}]}});
        assert!(annotate(&mut arr, &report(), Some("n")), "a JSON array is free text to the agent");
        assert_eq!(arr["result"]["content"][0]["text"], "[1,2]");
    }

    #[test]
    fn framing() {
        let sse = "event: message\ndata: {\"id\":3,\"result\":{}}\n\n";
        let v = parse_rpc(sse, true).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(frame(&v, true), "event: message\ndata: {\"id\":3,\"result\":{}}\n\n");
        assert!(parse_rpc("nope", false).is_none());
        let call = json!({"method": "tools/call", "params": {"name": " t "}});
        assert_eq!(called_tool(&call), Some("t"));
        assert_eq!(called_tool(&json!({"method": "tools/list"})), None);
    }
}

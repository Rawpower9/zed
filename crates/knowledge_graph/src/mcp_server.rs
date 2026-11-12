//! A Model Context Protocol server that exposes a project's knowledge graph
//! over newline-delimited JSON-RPC, so that agents which do not run inside
//! Zed's own agent loop can use the same graph.

use crate::{
    AGENT_INSTRUCTIONS, QUERY_TOOL_NAME, QueryInput, Store, UPDATE_TOOL_NAME, UpdateInput,
};
use anyhow::{Context as _, Result};
use schemars::JsonSchema;
use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub const SERVER_NAME: &str = "zed-knowledge-graph";

const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
/// The server only uses `initialize`, `ping`, and the tool methods, which
/// have the same shape in each of these revisions.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", LATEST_PROTOCOL_VERSION];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serves requests read from `reader` until it reaches end of input.
pub fn serve(store: &Store, reader: impl BufRead, mut writer: impl Write) -> Result<()> {
    for line in reader.lines() {
        let line = line.context("reading request")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_message(store, &line) {
            serde_json::to_writer(&mut writer, &response).context("writing response")?;
            writer.write_all(b"\n").context("writing response")?;
            writer.flush().context("writing response")?;
        }
    }
    Ok(())
}

/// Handles one JSON-RPC message, returning the response to send, if any.
/// Notifications and responses from the client produce no reply.
pub fn handle_message(store: &Store, message: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(message) {
        Ok(message) => message,
        Err(error) => {
            return Some(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("invalid JSON: {error}"),
            ));
        }
    };
    let Some(request) = message.as_object() else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "expected a JSON-RPC request object".to_string(),
        ));
    };
    let id = request.get("id").filter(|id| !id.is_null()).cloned();
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        // A message without a method is a response to a request, and this
        // server never sends requests.
        return None;
    };
    // A request without an id is a notification, which must not be answered.
    let id = id?;
    let params = request.get("params").cloned().unwrap_or(Value::Null);

    let result = match method {
        "initialize" => Ok(initialize_result(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(store, &params),
        _ => Err((
            METHOD_NOT_FOUND,
            format!("method `{method}` is not supported"),
        )),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, message)) => error_response(id, code, message),
    })
}

fn initialize_result(params: &Value) -> Value {
    let requested_version = params.get("protocolVersion").and_then(Value::as_str);
    let protocol_version = requested_version
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": AGENT_INSTRUCTIONS,
    })
}

fn tool_definitions() -> Value {
    json!([
        tool_definition::<QueryInput>(QUERY_TOOL_NAME),
        tool_definition::<UpdateInput>(UPDATE_TOOL_NAME),
    ])
}

/// The tool description is the doc comment on its input type, the same
/// convention Zed's built-in agent tools use, so both surfaces describe the
/// tools identically.
fn tool_definition<Input: JsonSchema>(name: &str) -> Value {
    let mut input_schema = schemars::schema_for!(Input).to_value();
    let mut description = Value::String(String::new());
    if let Some(schema) = input_schema.as_object_mut() {
        schema.remove("$schema");
        schema.remove("title");
        if let Some(schema_description) = schema.remove("description") {
            description = schema_description;
        }
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn call_tool(store: &Store, params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| (INVALID_PARAMS, "missing tool name".to_string()))?;
    let arguments = match params.get("arguments") {
        Some(arguments) if !arguments.is_null() => arguments.clone(),
        _ => json!({}),
    };

    // Failures while running a tool are reported inside the result rather
    // than as protocol errors, so that the model sees them and can correct
    // its input.
    let outcome = match name {
        QUERY_TOOL_NAME => serde_json::from_value::<QueryInput>(arguments)
            .context("invalid arguments")
            .and_then(|input| store.query(&input)),
        UPDATE_TOOL_NAME => serde_json::from_value::<UpdateInput>(arguments)
            .context("invalid arguments")
            .and_then(|input| store.update(input)),
        _ => return Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
    };
    Ok(match outcome {
        Ok(text) => json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }),
        Err(error) => json!({
            "content": [{ "type": "text", "text": format!("{error:#}") }],
            "isError": true,
        }),
    })
}

fn error_response(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn store() -> (tempfile::TempDir, Store) {
        let project = tempfile::tempdir().expect("temporary directory");
        let store = Store::new(project.path());
        (project, store)
    }

    fn request(store: &Store, message: Value) -> Value {
        handle_message(store, &message.to_string()).expect("request should be answered")
    }

    fn tool_text(response: &Value) -> &str {
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool result has text content")
    }

    #[test]
    fn initialize_negotiates_the_protocol_version_and_sends_instructions() {
        let (_project, store) = store();
        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "0" },
                },
            }),
        );
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(response["result"]["serverInfo"]["name"], SERVER_NAME);
        assert_eq!(response["result"]["instructions"], AGENT_INSTRUCTIONS);

        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": "a",
                "method": "initialize",
                "params": { "protocolVersion": "2999-01-01" },
            }),
        );
        assert_eq!(response["id"], "a");
        assert_eq!(
            response["result"]["protocolVersion"],
            LATEST_PROTOCOL_VERSION
        );
    }

    #[test]
    fn lists_both_tools_with_object_schemas() {
        let (_project, store) = store();
        let response = request(
            &store,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        );
        let tools = response["result"]["tools"]
            .as_array()
            .expect("tools is an array");
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool["name"].as_str().expect("tool has a name"))
                .collect::<Vec<_>>(),
            vec![QUERY_TOOL_NAME, UPDATE_TOOL_NAME]
        );
        for tool in tools {
            let description = tool["description"]
                .as_str()
                .expect("tool has a description");
            assert!(description.contains("knowledge graph"), "{description}");
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert!(tool["inputSchema"].get("$schema").is_none());
            assert!(tool["inputSchema"].get("description").is_none());
            assert!(tool["inputSchema"]["properties"].is_object());
        }
        assert_eq!(
            tools[1]["inputSchema"]["properties"]["nodes"]["type"],
            "array"
        );
    }

    #[test]
    fn tools_update_and_query_the_graph() {
        let (_project, store) = store();
        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "tools/call",
                "params": {
                    "name": UPDATE_TOOL_NAME,
                    "arguments": {
                        "nodes": [{
                            "kind": "decision",
                            "title": "Use Redis for sessions",
                            "summary": "Sessions live in Redis so that any server can validate them.",
                        }],
                    },
                },
            }),
        );
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(tool_text(&response), "Created: use-redis-for-sessions.");

        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": { "name": QUERY_TOOL_NAME, "arguments": { "text": "redis" } },
            }),
        );
        assert_eq!(response["result"]["isError"], false);
        assert_eq!(
            tool_text(&response),
            "1 matching node:\n- use-redis-for-sessions [decision] Use Redis for sessions: \
             Sessions live in Redis so that any server can validate them."
        );

        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": { "name": QUERY_TOOL_NAME },
            }),
        );
        assert!(tool_text(&response).starts_with("1 node:"));
    }

    #[test]
    fn tool_failures_are_results_the_model_can_read() {
        let (_project, store) = store();
        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": { "name": UPDATE_TOOL_NAME, "arguments": { "delete": ["ghost"] } },
            }),
        );
        assert_eq!(response["result"]["isError"], true);
        assert_eq!(tool_text(&response), "cannot delete `ghost`: no such node");

        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": {
                    "name": UPDATE_TOOL_NAME,
                    "arguments": { "nodes": [{ "kind": "opinion", "title": "T", "summary": "S" }] },
                },
            }),
        );
        assert_eq!(response["result"]["isError"], true);
        assert!(tool_text(&response).starts_with("invalid arguments: "));
    }

    #[test]
    fn protocol_errors() {
        let (_project, store) = store();

        let response = handle_message(&store, "{not json").expect("parse errors are answered");
        assert_eq!(response["error"]["code"], PARSE_ERROR);
        assert_eq!(response["id"], Value::Null);

        let response = handle_message(&store, "[]").expect("invalid requests are answered");
        assert_eq!(response["error"]["code"], INVALID_REQUEST);

        let response = request(
            &store,
            json!({ "jsonrpc": "2.0", "id": 8, "method": "resources/list" }),
        );
        assert_eq!(response["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(response["id"], 8);

        let response = request(
            &store,
            json!({
                "jsonrpc": "2.0",
                "id": 9,
                "method": "tools/call",
                "params": { "name": "delete_everything" },
            }),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);

        let response = request(
            &store,
            json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/call", "params": {} }),
        );
        assert_eq!(response["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn notifications_and_responses_are_not_answered() {
        let (_project, store) = store();
        for message in [
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {} }),
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
        ] {
            assert_eq!(handle_message(&store, &message.to_string()), None);
        }
    }

    #[test]
    fn serve_answers_each_request_on_its_own_line() {
        let (_project, store) = store();
        let input = [
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
            String::new(),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }).to_string(),
        ]
        .join("\n");
        let mut output = Vec::new();
        serve(&store, input.as_bytes(), &mut output).expect("serve succeeds");

        let output = String::from_utf8(output).expect("output is UTF-8");
        let responses = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("each line is JSON"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(
            responses[0],
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} })
        );
        assert_eq!(responses[1]["id"], 2);
    }
}

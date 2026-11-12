//! The harness sits between an ACP client (Zed) and an ACP agent, forwarding
//! every message and intervening in two places: it attaches the knowledge
//! graph MCP server to each session, and it can hold back the end of a turn
//! until the project's verification commands pass.
//!
//! This module is the protocol state machine. It performs no I/O: each input
//! message produces a list of [`Effect`]s for the caller to carry out.

use crate::verification::VerificationFailure;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

pub const KNOWLEDGE_GRAPH_SERVER_NAME: &str = knowledge_graph::mcp_server::SERVER_NAME;

const SESSION_SETUP_METHODS: &[&str] = &[
    "session/new",
    "session/load",
    "session/resume",
    "session/fork",
];
/// Tool calls of these kinds may have changed the project. `execute` is
/// included because agents routinely edit files through shell commands, which
/// are reported as terminal use rather than as edits.
const FILE_CHANGING_TOOL_KINDS: &[&str] = &["edit", "delete", "move", "execute"];
const HARNESS_REQUEST_ID_PREFIX: &str = "zed-agent-harness-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// How to launch the knowledge graph MCP server, or `None` to leave
    /// sessions as the client configured them.
    pub knowledge_graph_server: Option<McpServerCommand>,
    /// Shell commands that must all succeed after a turn in which the agent
    /// edited files or ran commands.
    pub verify_commands: Vec<String>,
    /// How many times the agent is asked to fix a failing verification
    /// before the failure is handed back to the user.
    pub max_fix_attempts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerCommand {
    pub command: String,
    /// Arguments placed before the project root, which is appended last.
    pub arguments: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Send this line to the agent.
    ToAgent(String),
    /// Send this line to the client.
    ToClient(String),
    /// Run the verification commands in `cwd` and report back through
    /// [`Harness::on_verification_finished`].
    RunVerification { session_id: String, cwd: PathBuf },
    /// Stop a verification that is still running; its result is not wanted.
    CancelVerification { session_id: String },
    /// Something the person running the harness should know about, which is
    /// not part of the protocol stream.
    Log(String),
}

pub struct Harness {
    config: Config,
    protocol_version: Option<u64>,
    warned_about_protocol_version: bool,
    pending_client_requests: HashMap<String, PendingRequest>,
    /// Follow-up prompts the harness itself sent, keyed by request id.
    harness_requests: HashMap<String, String>,
    next_harness_request: u64,
    sessions: HashMap<String, Session>,
}

enum PendingRequest {
    Initialize,
    SessionSetup {
        cwd: PathBuf,
        session_id: Option<String>,
    },
    Prompt {
        session_id: String,
    },
}

struct Session {
    cwd: PathBuf,
    turn: Option<Turn>,
}

struct Turn {
    /// The id of the client's `session/prompt` request, which the harness
    /// answers once the turn is really over.
    client_request_id: Value,
    may_have_changed_files: bool,
    fix_attempts: u32,
    /// The agent's answer, kept while verification runs.
    held_response: Option<Value>,
}

impl Harness {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            protocol_version: None,
            warned_about_protocol_version: false,
            pending_client_requests: HashMap::new(),
            harness_requests: HashMap::new(),
            next_harness_request: 0,
            sessions: HashMap::new(),
        }
    }

    pub fn on_client_message(&mut self, line: &str) -> Vec<Effect> {
        let Ok(Value::Object(mut message)) = serde_json::from_str::<Value>(line) else {
            return vec![Effect::ToAgent(line.to_string())];
        };
        let Some(method) = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return vec![Effect::ToAgent(line.to_string())];
        };
        let request_id = message.get("id").filter(|id| !id.is_null()).cloned();

        if method == "session/cancel" {
            return self.on_cancel(&message, line);
        }
        let Some(request_id) = request_id else {
            return vec![Effect::ToAgent(line.to_string())];
        };
        let request_key = request_id.to_string();

        if method == "initialize" {
            self.pending_client_requests
                .insert(request_key, PendingRequest::Initialize);
            return vec![Effect::ToAgent(line.to_string())];
        }

        if SESSION_SETUP_METHODS.contains(&method.as_str()) {
            let Some(params) = message.get_mut("params").and_then(Value::as_object_mut) else {
                return vec![Effect::ToAgent(line.to_string())];
            };
            let Some(cwd) = params.get("cwd").and_then(Value::as_str).map(PathBuf::from) else {
                return vec![Effect::ToAgent(line.to_string())];
            };
            // `session/fork` names the session being copied; the new
            // session's id only arrives in the response.
            let session_id = (method != "session/fork")
                .then(|| params.get("sessionId").and_then(Value::as_str))
                .flatten()
                .map(str::to_string);
            let injected = self.inject_knowledge_graph_server(params, &cwd);
            self.pending_client_requests.insert(
                request_key,
                PendingRequest::SessionSetup { cwd, session_id },
            );
            return vec![Effect::ToAgent(if injected {
                Value::Object(message).to_string()
            } else {
                line.to_string()
            })];
        }

        if method == "session/prompt" && !self.config.verify_commands.is_empty() {
            return self.on_prompt(&message, request_id, line);
        }

        vec![Effect::ToAgent(line.to_string())]
    }

    pub fn on_agent_message(&mut self, line: &str) -> Vec<Effect> {
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(line) else {
            return vec![Effect::ToClient(line.to_string())];
        };

        if let Some(method) = message.get("method").and_then(Value::as_str) {
            if method == "session/update" {
                self.note_file_changes(&message);
            }
            return vec![Effect::ToClient(line.to_string())];
        }

        let Some(response_id) = message.get("id").filter(|id| !id.is_null()) else {
            return vec![Effect::ToClient(line.to_string())];
        };
        let response_key = response_id.to_string();

        if let Some(session_id) = response_id
            .as_str()
            .and_then(|id| self.harness_requests.remove(id))
        {
            return self.on_turn_ended(&session_id, Value::Object(message));
        }

        match self.pending_client_requests.remove(&response_key) {
            Some(PendingRequest::Initialize) => {
                self.protocol_version = message
                    .get("result")
                    .and_then(|result| result.get("protocolVersion"))
                    .and_then(Value::as_u64);
                vec![Effect::ToClient(line.to_string())]
            }
            Some(PendingRequest::SessionSetup { cwd, session_id }) => {
                if let Some(result) = message.get("result") {
                    let session_id = result
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(session_id);
                    if let Some(session_id) = session_id {
                        self.sessions
                            .insert(session_id, Session { cwd, turn: None });
                    }
                }
                vec![Effect::ToClient(line.to_string())]
            }
            Some(PendingRequest::Prompt { session_id }) => {
                self.on_turn_ended(&session_id, Value::Object(message))
            }
            None => vec![Effect::ToClient(line.to_string())],
        }
    }

    pub fn on_verification_finished(
        &mut self,
        session_id: &str,
        failure: Option<VerificationFailure>,
    ) -> Vec<Effect> {
        let max_fix_attempts = self.config.max_fix_attempts;
        let Some(session) = self.sessions.get_mut(session_id) else {
            return Vec::new();
        };
        // A turn without a held response is not waiting on verification:
        // the result belongs to a turn that was cancelled in the meantime.
        let Some(turn) = session
            .turn
            .as_mut()
            .filter(|turn| turn.held_response.is_some())
        else {
            return Vec::new();
        };

        let Some(failure) = failure else {
            let mut effects = vec![notice(session_id, "Verification passed.")];
            effects.extend(self.finish_turn(session_id));
            return effects;
        };

        if turn.fix_attempts >= max_fix_attempts {
            let text = format!(
                "Verification is still failing after {} fix attempt{}. `{}` {}.\n\n```\n{}\n```",
                turn.fix_attempts,
                if turn.fix_attempts == 1 { "" } else { "s" },
                failure.command,
                failure.status,
                failure.output,
            );
            let mut effects = vec![notice(session_id, &text)];
            effects.extend(self.finish_turn(session_id));
            return effects;
        }

        turn.fix_attempts += 1;
        turn.held_response = None;
        let fix_attempt = turn.fix_attempts;
        let request_id = format!("{HARNESS_REQUEST_ID_PREFIX}{}", self.next_harness_request);
        self.next_harness_request += 1;
        self.harness_requests
            .insert(request_id.clone(), session_id.to_string());

        let prompt = format!(
            "The harness ran the project's verification after your changes and it failed.\n\n\
             Command: `{}`\nResult: {}\n\nOutput:\n```\n{}\n```\n\n\
             Fix the cause of this failure. Do not weaken, skip, or delete the check.",
            failure.command, failure.status, failure.output,
        );
        vec![
            notice(
                session_id,
                &format!(
                    "Verification failed: `{}` {}. Asking the agent to fix it \
                     (attempt {fix_attempt} of {max_fix_attempts}).",
                    failure.command, failure.status,
                ),
            ),
            Effect::ToAgent(
                json!({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "method": "session/prompt",
                    "params": {
                        "sessionId": session_id,
                        "prompt": [{ "type": "text", "text": prompt }],
                    },
                })
                .to_string(),
            ),
        ]
    }

    fn inject_knowledge_graph_server(
        &self,
        params: &mut Map<String, Value>,
        cwd: &std::path::Path,
    ) -> bool {
        let Some(server) = &self.config.knowledge_graph_server else {
            return false;
        };
        let servers = params
            .entry("mcpServers")
            .or_insert_with(|| Value::Array(Vec::new()));
        if servers.is_null() {
            *servers = Value::Array(Vec::new());
        }
        let Some(servers) = servers.as_array_mut() else {
            return false;
        };
        let already_configured = servers.iter().any(|server| {
            server.get("name").and_then(Value::as_str) == Some(KNOWLEDGE_GRAPH_SERVER_NAME)
        });
        if already_configured {
            return false;
        }

        let mut arguments = server.arguments.clone();
        arguments.push(cwd.to_string_lossy().into_owned());
        let mut entry = Map::new();
        // Version 1 identifies a stdio server by the absence of `type`;
        // later versions require the tag.
        if self.protocol_version.is_some_and(|version| version >= 2) {
            entry.insert("type".into(), json!("stdio"));
        }
        entry.insert("name".into(), json!(KNOWLEDGE_GRAPH_SERVER_NAME));
        entry.insert("command".into(), json!(server.command));
        entry.insert("args".into(), json!(arguments));
        entry.insert("env".into(), json!([]));
        servers.push(Value::Object(entry));
        true
    }

    fn on_prompt(
        &mut self,
        message: &Map<String, Value>,
        request_id: Value,
        line: &str,
    ) -> Vec<Effect> {
        let forward = Effect::ToAgent(line.to_string());
        // From version 2 on, the end of a turn is a notification rather than
        // the response to `session/prompt`, which this gate does not handle.
        if self.protocol_version != Some(1) {
            if !self.warned_about_protocol_version {
                self.warned_about_protocol_version = true;
                return vec![
                    Effect::Log(format!(
                        "verification commands are only run with ACP protocol version 1, but \
                         the agent negotiated {}; turns will not be verified",
                        self.protocol_version
                            .map_or("no version".to_string(), |version| format!(
                                "version {version}"
                            )),
                    )),
                    forward,
                ];
            }
            return vec![forward];
        }
        let Some(session_id) = message
            .get("params")
            .and_then(|params| params.get("sessionId"))
            .and_then(Value::as_str)
        else {
            return vec![forward];
        };
        let Some(session) = self.sessions.get_mut(session_id) else {
            return vec![forward];
        };

        let mut effects = Vec::new();
        if let Some(turn) = &session.turn {
            if turn.held_response.is_none() {
                // The agent is still working on a turn the harness is
                // tracking. Leave that turn alone and let this prompt
                // through untracked rather than guess how the agent orders
                // the two.
                return vec![forward];
            }
            // The client moved on while verification was running, so that
            // turn's answer is due now.
            effects.push(Effect::CancelVerification {
                session_id: session_id.to_string(),
            });
            effects.extend(self.finish_turn(session_id));
        }

        if let Some(session) = self.sessions.get_mut(session_id) {
            session.turn = Some(Turn {
                client_request_id: request_id.clone(),
                may_have_changed_files: false,
                fix_attempts: 0,
                held_response: None,
            });
        }
        self.pending_client_requests.insert(
            request_id.to_string(),
            PendingRequest::Prompt {
                session_id: session_id.to_string(),
            },
        );
        effects.push(forward);
        effects
    }

    fn on_cancel(&mut self, message: &Map<String, Value>, line: &str) -> Vec<Effect> {
        let forward = Effect::ToAgent(line.to_string());
        let Some(session_id) = message
            .get("params")
            .and_then(|params| params.get("sessionId"))
            .and_then(Value::as_str)
        else {
            return vec![forward];
        };
        let is_verifying = self
            .sessions
            .get(session_id)
            .and_then(|session| session.turn.as_ref())
            .is_some_and(|turn| turn.held_response.is_some());
        if !is_verifying {
            // The agent is running, and will end the turn itself with a
            // `cancelled` stop reason.
            return vec![forward];
        }

        // The agent already finished; only the harness is still busy. The
        // protocol requires a cancelled turn to end with `cancelled`.
        if let Some(turn) = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.turn.as_mut())
            && let Some(result) = turn
                .held_response
                .as_mut()
                .and_then(|response| response.get_mut("result"))
                .and_then(Value::as_object_mut)
        {
            result.insert("stopReason".into(), json!("cancelled"));
        }
        let mut effects = vec![
            Effect::CancelVerification {
                session_id: session_id.to_string(),
            },
            forward,
        ];
        effects.extend(self.finish_turn(session_id));
        effects
    }

    fn note_file_changes(&mut self, message: &Map<String, Value>) {
        let Some(params) = message.get("params") else {
            return;
        };
        let Some(update) = params.get("update") else {
            return;
        };
        let is_tool_call = matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("tool_call" | "tool_call_update")
        );
        let changes_files = update
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|kind| FILE_CHANGING_TOOL_KINDS.contains(&kind));
        if !is_tool_call || !changes_files {
            return;
        }
        if let Some(turn) = params
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|session_id| self.sessions.get_mut(session_id))
            .and_then(|session| session.turn.as_mut())
        {
            turn.may_have_changed_files = true;
        }
    }

    fn on_turn_ended(&mut self, session_id: &str, response: Value) -> Vec<Effect> {
        let Some(session) = self.sessions.get_mut(session_id) else {
            return vec![Effect::ToClient(response.to_string())];
        };
        let Some(turn) = session.turn.as_mut() else {
            return vec![Effect::ToClient(response.to_string())];
        };

        let ended_normally = response
            .get("result")
            .and_then(|result| result.get("stopReason"))
            .and_then(Value::as_str)
            == Some("end_turn");
        // Once a fix was requested, the turn is verified again even if the
        // agent answered without touching files, so that the user is told
        // whether the failure is still there.
        let needs_verification =
            ended_normally && (turn.may_have_changed_files || turn.fix_attempts > 0);
        let cwd = session.cwd.clone();
        turn.held_response = Some(response);
        if !needs_verification {
            return self.finish_turn(session_id);
        }

        let commands = self
            .config
            .verify_commands
            .iter()
            .map(|command| format!("`{command}`"))
            .collect::<Vec<_>>()
            .join(", ");
        vec![
            notice(session_id, &format!("Running verification: {commands}")),
            Effect::RunVerification {
                session_id: session_id.to_string(),
                cwd,
            },
        ]
    }

    /// Answers the client's `session/prompt` with the response the harness
    /// has been holding, under the id the client used.
    fn finish_turn(&mut self, session_id: &str) -> Vec<Effect> {
        let Some(turn) = self
            .sessions
            .get_mut(session_id)
            .and_then(|session| session.turn.take())
        else {
            return Vec::new();
        };
        self.pending_client_requests
            .remove(&turn.client_request_id.to_string());
        let Some(mut response) = turn.held_response else {
            return Vec::new();
        };
        if let Some(response) = response.as_object_mut() {
            response.insert("id".into(), turn.client_request_id);
        }
        vec![Effect::ToClient(response.to_string())]
    }
}

/// A message from the harness itself, shown in the thread as agent text so
/// that the user can see why a turn has not ended yet.
fn notice(session_id: &str, text: &str) -> Effect {
    Effect::ToClient(
        json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": format!("\n\n> **Harness:** {text}\n\n") },
                },
            },
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn config(verify_commands: &[&str]) -> Config {
        Config {
            knowledge_graph_server: Some(McpServerCommand {
                command: "/bin/zed-agent-harness".into(),
                arguments: vec!["mcp".into(), "--project-root".into()],
            }),
            verify_commands: verify_commands.iter().map(|c| c.to_string()).collect(),
            max_fix_attempts: 2,
        }
    }

    fn parse(effect: &Effect) -> Value {
        match effect {
            Effect::ToAgent(line) | Effect::ToClient(line) => {
                serde_json::from_str(line).expect("effect carries JSON")
            }
            other => panic!("expected a message, got {other:?}"),
        }
    }

    fn notice_text(effect: &Effect) -> String {
        assert!(matches!(effect, Effect::ToClient(_)), "{effect:?}");
        let message = parse(effect);
        assert_eq!(message["method"], "session/update");
        assert_eq!(
            message["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        message["params"]["update"]["content"]["text"]
            .as_str()
            .expect("notice has text")
            .to_string()
    }

    fn failure() -> Option<VerificationFailure> {
        Some(VerificationFailure {
            command: "cargo check".into(),
            status: "exited with code 101".into(),
            output: "error[E0308]: mismatched types".into(),
        })
    }

    /// Runs `initialize` and `session/new`, leaving session `s1` in `/project`.
    fn started_harness(config: Config, protocol_version: u64) -> Harness {
        let mut harness = Harness::new(config);
        let initialize =
            json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} }).to_string();
        assert_eq!(
            harness.on_client_message(&initialize),
            vec![Effect::ToAgent(initialize)]
        );
        let initialized = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "result": { "protocolVersion": protocol_version },
        })
        .to_string();
        assert_eq!(
            harness.on_agent_message(&initialized),
            vec![Effect::ToClient(initialized)]
        );

        harness.on_client_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/new",
                "params": { "cwd": "/project", "mcpServers": [] },
            })
            .to_string(),
        );
        harness.on_agent_message(
            &json!({ "jsonrpc": "2.0", "id": 1, "result": { "sessionId": "s1" } }).to_string(),
        );
        harness
    }

    fn prompt(harness: &mut Harness, request_id: u64) -> Vec<Effect> {
        harness.on_client_message(
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "session/prompt",
                "params": { "sessionId": "s1", "prompt": [{ "type": "text", "text": "hi" }] },
            })
            .to_string(),
        )
    }

    fn edit_tool_call(harness: &mut Harness) {
        let update = json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": { "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "edit" },
            },
        })
        .to_string();
        assert_eq!(
            harness.on_agent_message(&update),
            vec![Effect::ToClient(update)]
        );
    }

    fn end_turn(harness: &mut Harness, request_id: Value, stop_reason: &str) -> Vec<Effect> {
        harness.on_agent_message(
            &json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": { "stopReason": stop_reason },
            })
            .to_string(),
        )
    }

    #[test]
    fn injects_the_knowledge_graph_server_in_the_negotiated_shape() {
        for (protocol_version, expected_entry) in [
            (
                1,
                json!({
                    "name": KNOWLEDGE_GRAPH_SERVER_NAME,
                    "command": "/bin/zed-agent-harness",
                    "args": ["mcp", "--project-root", "/work/tree"],
                    "env": [],
                }),
            ),
            (
                2,
                json!({
                    "type": "stdio",
                    "name": KNOWLEDGE_GRAPH_SERVER_NAME,
                    "command": "/bin/zed-agent-harness",
                    "args": ["mcp", "--project-root", "/work/tree"],
                    "env": [],
                }),
            ),
        ] {
            let mut harness = started_harness(config(&[]), protocol_version);
            let existing =
                json!({ "name": "other", "command": "/bin/other", "args": [], "env": [] });
            for (method, params) in [
                (
                    "session/new",
                    json!({ "cwd": "/work/tree", "mcpServers": [existing] }),
                ),
                (
                    "session/load",
                    json!({ "cwd": "/work/tree", "sessionId": "old", "mcpServers": [existing] }),
                ),
                (
                    "session/resume",
                    json!({ "cwd": "/work/tree", "sessionId": "old" }),
                ),
                (
                    "session/fork",
                    json!({ "cwd": "/work/tree", "sessionId": "old", "mcpServers": null }),
                ),
            ] {
                let effects = harness.on_client_message(
                    &json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params })
                        .to_string(),
                );
                assert_eq!(effects.len(), 1);
                assert!(matches!(effects[0], Effect::ToAgent(_)));
                let forwarded = parse(&effects[0]);
                assert_eq!(forwarded["id"], 7);
                assert_eq!(forwarded["method"], method);
                assert_eq!(forwarded["params"]["cwd"], "/work/tree");
                let servers = forwarded["params"]["mcpServers"]
                    .as_array()
                    .expect("mcpServers is an array");
                assert_eq!(servers.last(), Some(&expected_entry), "{method}");
                assert_eq!(
                    servers.len(),
                    if params["mcpServers"].is_array() {
                        2
                    } else {
                        1
                    }
                );
            }
        }
    }

    #[test]
    fn leaves_sessions_alone_when_injection_does_not_apply() {
        let already_present = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {
                "cwd": "/project",
                "mcpServers": [{ "name": KNOWLEDGE_GRAPH_SERVER_NAME, "command": "x", "args": [], "env": [] }],
            },
        })
        .to_string();
        let mut harness = started_harness(config(&[]), 1);
        assert_eq!(
            harness.on_client_message(&already_present),
            vec![Effect::ToAgent(already_present)]
        );

        let without_cwd =
            json!({ "jsonrpc": "2.0", "id": 3, "method": "session/new", "params": {} }).to_string();
        assert_eq!(
            harness.on_client_message(&without_cwd),
            vec![Effect::ToAgent(without_cwd)]
        );

        let mut disabled = started_harness(
            Config {
                knowledge_graph_server: None,
                ..config(&[])
            },
            1,
        );
        let new_session = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/new",
            "params": { "cwd": "/project", "mcpServers": [] },
        })
        .to_string();
        assert_eq!(
            disabled.on_client_message(&new_session),
            vec![Effect::ToAgent(new_session)]
        );
    }

    #[test]
    fn forwards_everything_else_byte_for_byte() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        for line in [
            "this is not json",
            "[1, 2, 3]",
            r#"{"jsonrpc":"2.0","method":"some/notification","params":{"big":12345678901234567890}}"#,
            r#"{"jsonrpc":"2.0","id":41,"method":"session/set_mode","params":{"sessionId":"s1"}}"#,
            r#"{"jsonrpc":"2.0","id":5,"result":{"outcome":{"outcome":"selected"}}}"#,
        ] {
            assert_eq!(
                harness.on_client_message(line),
                vec![Effect::ToAgent(line.to_string())]
            );
        }
        for line in [
            "agent wrote a log line to stdout",
            r#"{"jsonrpc":"2.0","id":5,"method":"session/request_permission","params":{"sessionId":"s1"}}"#,
            r#"{"jsonrpc":"2.0","id":41,"result":null}"#,
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}"#,
        ] {
            assert_eq!(
                harness.on_agent_message(line),
                vec![Effect::ToClient(line.to_string())]
            );
        }
    }

    #[test]
    fn without_verify_commands_prompts_pass_straight_through() {
        let mut harness = started_harness(config(&[]), 1);
        let effects = prompt(&mut harness, 2);
        assert!(matches!(effects.as_slice(), [Effect::ToAgent(_)]));
        edit_tool_call(&mut harness);
        let response = json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } })
            .to_string();
        assert_eq!(
            harness.on_agent_message(&response),
            vec![Effect::ToClient(response)]
        );
    }

    #[test]
    fn turns_that_changed_nothing_are_not_verified() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        let effects = end_turn(&mut harness, json!(2), "end_turn");
        assert_eq!(effects.len(), 1);
        assert_eq!(
            parse(&effects[0]),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } })
        );
    }

    #[test]
    fn only_tool_calls_that_may_change_files_trigger_verification() {
        for (kind, expect_verification) in [
            (Some("edit"), true),
            (Some("delete"), true),
            (Some("move"), true),
            (Some("execute"), true),
            (Some("read"), false),
            (Some("search"), false),
            (Some("fetch"), false),
            (Some("think"), false),
            (Some("other"), false),
            (None, false),
        ] {
            let mut harness = started_harness(config(&["cargo check"]), 1);
            prompt(&mut harness, 2);
            let mut update = json!({ "sessionUpdate": "tool_call", "toolCallId": "t1" });
            if let Some(kind) = kind {
                update["kind"] = json!(kind);
            }
            harness.on_agent_message(
                &json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": { "sessionId": "s1", "update": update },
                })
                .to_string(),
            );
            let effects = end_turn(&mut harness, json!(2), "end_turn");
            let verified = effects
                .iter()
                .any(|effect| matches!(effect, Effect::RunVerification { .. }));
            assert_eq!(verified, expect_verification, "{kind:?}");
        }
    }

    #[test]
    fn turns_that_did_not_end_normally_are_not_verified() {
        for stop_reason in ["cancelled", "max_tokens", "refusal", "max_turn_requests"] {
            let mut harness = started_harness(config(&["cargo check"]), 1);
            prompt(&mut harness, 2);
            edit_tool_call(&mut harness);
            let effects = end_turn(&mut harness, json!(2), stop_reason);
            assert_eq!(effects.len(), 1, "{stop_reason}");
            assert_eq!(parse(&effects[0])["result"]["stopReason"], stop_reason);
        }

        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        let effects = harness.on_agent_message(
            &json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32603, "message": "boom" } })
                .to_string(),
        );
        assert_eq!(effects.len(), 1);
        assert_eq!(parse(&effects[0])["error"]["message"], "boom");
    }

    #[test]
    fn passing_verification_releases_the_held_response() {
        let mut harness = started_harness(config(&["cargo check", "cargo test"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);

        let effects = end_turn(&mut harness, json!(2), "end_turn");
        assert_eq!(effects.len(), 2);
        assert_eq!(
            notice_text(&effects[0]),
            "\n\n> **Harness:** Running verification: `cargo check`, `cargo test`\n\n"
        );
        assert_eq!(
            effects[1],
            Effect::RunVerification {
                session_id: "s1".into(),
                cwd: PathBuf::from("/project"),
            }
        );

        let effects = harness.on_verification_finished("s1", None);
        assert_eq!(effects.len(), 2);
        assert!(notice_text(&effects[0]).contains("Verification passed."));
        assert_eq!(
            parse(&effects[1]),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } })
        );

        assert_eq!(harness.on_verification_finished("s1", None), Vec::new());
    }

    #[test]
    fn failing_verification_sends_the_agent_back_to_fix_it() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        end_turn(&mut harness, json!(2), "end_turn");

        let effects = harness.on_verification_finished("s1", failure());
        assert_eq!(effects.len(), 2);
        assert!(notice_text(&effects[0]).contains(
            "Verification failed: `cargo check` exited with code 101. Asking the agent to fix it \
             (attempt 1 of 2)."
        ));
        assert!(matches!(effects[1], Effect::ToAgent(_)));
        let follow_up = parse(&effects[1]);
        assert_eq!(follow_up["method"], "session/prompt");
        assert_eq!(follow_up["id"], "zed-agent-harness-0");
        assert_eq!(follow_up["params"]["sessionId"], "s1");
        let text = follow_up["params"]["prompt"][0]["text"]
            .as_str()
            .expect("prompt has text");
        assert!(text.contains("Command: `cargo check`"));
        assert!(text.contains("error[E0308]: mismatched types"));

        // The follow-up turn makes no edits, but is verified regardless.
        let effects = end_turn(&mut harness, json!("zed-agent-harness-0"), "end_turn");
        assert!(matches!(effects[1], Effect::RunVerification { .. }));

        let effects = harness.on_verification_finished("s1", None);
        assert_eq!(
            parse(&effects[1]),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } })
        );
    }

    #[test]
    fn gives_up_after_the_configured_number_of_fix_attempts() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        end_turn(&mut harness, json!(2), "end_turn");

        for attempt in 0..2 {
            let effects = harness.on_verification_finished("s1", failure());
            let follow_up = parse(&effects[1]);
            assert_eq!(follow_up["id"], format!("zed-agent-harness-{attempt}"));
            end_turn(&mut harness, follow_up["id"].clone(), "end_turn");
        }

        let effects = harness.on_verification_finished("s1", failure());
        assert_eq!(effects.len(), 2);
        let text = notice_text(&effects[0]);
        assert!(
            text.contains("Verification is still failing after 2 fix attempts."),
            "{text}"
        );
        assert!(text.contains("error[E0308]: mismatched types"));
        assert_eq!(
            parse(&effects[1]),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "end_turn" } })
        );
    }

    #[test]
    fn a_failed_fix_attempt_ends_the_turn_with_the_agents_answer() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        end_turn(&mut harness, json!(2), "end_turn");
        harness.on_verification_finished("s1", failure());

        let effects = harness.on_agent_message(
            &json!({
                "jsonrpc": "2.0",
                "id": "zed-agent-harness-0",
                "error": { "code": -32603, "message": "rate limited" },
            })
            .to_string(),
        );
        assert_eq!(effects.len(), 1);
        assert_eq!(
            parse(&effects[0]),
            json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32603, "message": "rate limited" } })
        );
    }

    #[test]
    fn cancelling_during_verification_ends_the_turn_as_cancelled() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        end_turn(&mut harness, json!(2), "end_turn");

        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "s1" },
        })
        .to_string();
        let effects = harness.on_client_message(&cancel);
        assert_eq!(effects.len(), 3);
        assert_eq!(
            effects[0],
            Effect::CancelVerification {
                session_id: "s1".into()
            }
        );
        assert_eq!(effects[1], Effect::ToAgent(cancel));
        assert_eq!(
            parse(&effects[2]),
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "stopReason": "cancelled" } })
        );

        assert_eq!(
            harness.on_verification_finished("s1", failure()),
            Vec::new()
        );
    }

    #[test]
    fn cancelling_while_the_agent_runs_is_left_to_the_agent() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);

        let cancel = json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": "s1" },
        })
        .to_string();
        assert_eq!(
            harness.on_client_message(&cancel),
            vec![Effect::ToAgent(cancel)]
        );
        let effects = end_turn(&mut harness, json!(2), "cancelled");
        assert_eq!(effects.len(), 1);
        assert_eq!(parse(&effects[0])["result"]["stopReason"], "cancelled");
    }

    #[test]
    fn a_new_prompt_during_verification_releases_the_previous_turn_first() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        prompt(&mut harness, 2);
        edit_tool_call(&mut harness);
        end_turn(&mut harness, json!(2), "end_turn");

        let effects = prompt(&mut harness, 3);
        assert_eq!(effects.len(), 3);
        assert_eq!(
            effects[0],
            Effect::CancelVerification {
                session_id: "s1".into()
            }
        );
        assert_eq!(parse(&effects[1])["id"], 2);
        assert!(matches!(effects[2], Effect::ToAgent(_)));
        assert_eq!(parse(&effects[2])["id"], 3);

        assert_eq!(
            harness.on_verification_finished("s1", failure()),
            Vec::new()
        );
        let effects = end_turn(&mut harness, json!(3), "end_turn");
        assert_eq!(effects.len(), 1);
        assert_eq!(parse(&effects[0])["id"], 3);
    }

    #[test]
    fn sessions_restored_by_id_are_verified_too() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        harness.on_client_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/load",
                "params": { "cwd": "/other", "sessionId": "restored", "mcpServers": [] },
            })
            .to_string(),
        );
        harness.on_agent_message(&json!({ "jsonrpc": "2.0", "id": 5, "result": {} }).to_string());

        harness.on_client_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "session/prompt",
                "params": { "sessionId": "restored", "prompt": [] },
            })
            .to_string(),
        );
        harness.on_agent_message(
            &json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": "restored",
                    "update": { "sessionUpdate": "tool_call_update", "toolCallId": "t", "kind": "delete" },
                },
            })
            .to_string(),
        );
        let effects = end_turn(&mut harness, json!(6), "end_turn");
        assert_eq!(
            effects[1],
            Effect::RunVerification {
                session_id: "restored".into(),
                cwd: PathBuf::from("/other"),
            }
        );
    }

    #[test]
    fn a_failed_session_setup_is_not_tracked() {
        let mut harness = started_harness(config(&["cargo check"]), 1);
        harness.on_client_message(
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "session/load",
                "params": { "cwd": "/other", "sessionId": "missing", "mcpServers": [] },
            })
            .to_string(),
        );
        harness.on_agent_message(
            &json!({ "jsonrpc": "2.0", "id": 5, "error": { "code": -32002, "message": "not found" } })
                .to_string(),
        );
        let prompt = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "session/prompt",
            "params": { "sessionId": "missing", "prompt": [] },
        })
        .to_string();
        assert_eq!(
            harness.on_client_message(&prompt),
            vec![Effect::ToAgent(prompt)]
        );
    }

    #[test]
    fn verification_is_skipped_with_a_warning_on_newer_protocol_versions() {
        let mut harness = started_harness(config(&["cargo check"]), 2);
        let effects = prompt(&mut harness, 2);
        assert_eq!(effects.len(), 2);
        assert!(matches!(&effects[0], Effect::Log(message) if message.contains("version 2")));
        assert!(matches!(effects[1], Effect::ToAgent(_)));

        let effects = prompt(&mut harness, 3);
        assert!(matches!(effects.as_slice(), [Effect::ToAgent(_)]));

        let response =
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "messageId": "m1" } }).to_string();
        assert_eq!(
            harness.on_agent_message(&response),
            vec![Effect::ToClient(response)]
        );
    }
}

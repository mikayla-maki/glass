//! Minimal MCP server for validating claude-sdk-rs integration.
//!
//! This binary implements just enough of the MCP protocol (JSON-RPC over stdio)
//! to test that Claude Code can discover and call tools through a Glass-provided
//! MCP server.
//!
//! Tools provided:
//! - `echo`: Returns its input, proving the MCP round-trip works.
//! - `greet`: Returns a greeting, used to test tool filtering via allowed_tools.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    // Log to stderr so we don't corrupt the JSON-RPC stream on stdout.
    eprintln!("[echo-mcp] Server starting");

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[echo-mcp] stdin read error: {e}");
                break;
            }
        };

        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[echo-mcp] JSON parse error: {e}");
                continue;
            }
        };

        let method = request["method"].as_str().unwrap_or("");
        let id = &request["id"];

        eprintln!("[echo-mcp] ← {method} (id={id})");

        // Notifications have no `id` and expect no response.
        if id.is_null() {
            match method {
                "notifications/initialized" => {
                    eprintln!("[echo-mcp] Client initialized notification received");
                }
                _ => {
                    eprintln!("[echo-mcp] Unknown notification: {method}");
                }
            }
            continue;
        }

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "tools": {}
                    },
                    "serverInfo": {
                        "name": "echo-mcp",
                        "version": "0.1.0"
                    }
                }
            }),

            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo back the input message. Use this to confirm MCP round-trip works.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": {
                                        "type": "string",
                                        "description": "The message to echo back"
                                    }
                                },
                                "required": ["message"]
                            }
                        },
                        {
                            "name": "greet",
                            "description": "Return a greeting for the given name.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "name": {
                                        "type": "string",
                                        "description": "Name to greet"
                                    }
                                },
                                "required": ["name"]
                            }
                        }
                    ]
                }
            }),

            "tools/call" => handle_tool_call(id, &request["params"]),

            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32601,
                    "message": format!("Method not found: {method}")
                }
            }),
        };

        let response_str = serde_json::to_string(&response).expect("failed to serialize response");
        eprintln!(
            "[echo-mcp] → {method} response ({} bytes)",
            response_str.len()
        );
        writeln!(stdout, "{response_str}").expect("failed to write to stdout");
        stdout.flush().expect("failed to flush stdout");
    }

    eprintln!("[echo-mcp] Server shutting down");
}

fn handle_tool_call(id: &Value, params: &Value) -> Value {
    let tool_name = params["name"].as_str().unwrap_or("unknown");
    let args = &params["arguments"];

    eprintln!("[echo-mcp] Tool call: {tool_name} args={args}");

    match tool_name {
        "echo" => {
            let message = args["message"].as_str().unwrap_or("<no message>");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": format!("ECHO: {message}")
                    }]
                }
            })
        }
        "greet" => {
            let name = args["name"].as_str().unwrap_or("stranger");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{
                        "type": "text",
                        "text": format!("Hello, {name}! Greetings from echo-mcp.")
                    }]
                }
            })
        }
        _ => {
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": format!("Unknown tool: {tool_name}")
                }
            })
        }
    }
}

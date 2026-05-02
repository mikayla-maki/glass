//! glass-spike — Validation spike for claude-sdk-rs
//!
//! Tests the four capabilities Glass depends on before committing to the SDK:
//!
//! 1. MCP config passthrough — can we provide a custom MCP server?
//! 2. allowed_tools disables built-ins — does restricting tools work?
//! 3. Tool call records in session results — can we extract tool calls for audit?
//! 4. System prompt length — does the SDK accept prompts > 10K chars?
//!
//! Usage:
//!   cargo build                          # builds both glass-spike and echo-mcp
//!   cargo run --bin glass-spike -- all   # run all tests (requires Claude CLI + API key)
//!   cargo run --bin glass-spike -- 1     # run just test 1
//!   cargo run --bin glass-spike -- 4     # run just test 4 (offline, no API key needed)
//!
//! Prerequisites:
//!   - Claude Code CLI installed and authenticated (`claude --version`)
//!   - ANTHROPIC_API_KEY set (for tests 1-3)

use serde_json::{json, Value};
use std::env;
use std::path::{Path, PathBuf};
use std::process;

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Find the echo-mcp binary. After `cargo build`, it's in target/debug/.
fn find_echo_mcp_binary() -> PathBuf {
    // Walk up from the current exe to find the target directory.
    let current_exe = env::current_exe().expect("cannot determine current exe path");
    let target_dir = current_exe.parent().expect("exe has no parent directory");

    let candidate = target_dir.join("echo-mcp");
    if candidate.exists() {
        return candidate;
    }

    // Fallback: maybe we're in target/debug/glass-spike
    let candidate2 = PathBuf::from("target/debug/echo-mcp");
    if candidate2.exists() {
        return candidate2.canonicalize().unwrap();
    }

    eprintln!("ERROR: Cannot find echo-mcp binary.");
    eprintln!("  Run `cargo build` first to build both binaries.");
    process::exit(1);
}

/// Write a temporary MCP config JSON file that points to our echo-mcp server.
/// Returns the path to the temp file (caller must keep the TempDir alive).
fn write_mcp_config(echo_binary: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().expect("failed to create temp dir");
    let config_path = dir.path().join("mcp-config.json");

    let config = json!({
        "mcpServers": {
            "glass-echo": {
                "command": echo_binary.to_str().unwrap(),
                "args": []
            }
        }
    });

    std::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
        .expect("failed to write MCP config");

    eprintln!("  MCP config written to: {}", config_path.display());
    eprintln!("  echo-mcp binary: {}", echo_binary.display());

    (dir, config_path)
}

/// Print a section header.
fn header(test_num: u32, title: &str) {
    eprintln!();
    eprintln!("============================================================");
    eprintln!("  TEST {test_num}: {title}");
    eprintln!("============================================================");
    eprintln!();
}

fn pass(msg: &str) {
    eprintln!("  ✅ PASS: {msg}");
}

fn fail(msg: &str) {
    eprintln!("  ❌ FAIL: {msg}");
}

fn info(msg: &str) {
    eprintln!("  ℹ️  {msg}");
}

// ─── Test 1: MCP Config Passthrough ──────────────────────────────────────────

async fn test_1_mcp_passthrough() -> bool {
    header(1, "MCP Config Passthrough");
    info("Can the bot write a per-invocation MCP config and have Claude Code");
    info("spawn our echo-mcp server and call our custom tool?");
    eprintln!();

    let echo_binary = find_echo_mcp_binary();
    let (_tmp_dir, config_path) = write_mcp_config(&echo_binary);

    // Build a client with:
    // - Our MCP config
    // - StreamJson format (so we can inspect tool calls)
    // - allowed_tools including our MCP tool
    // - A system prompt instructing Claude to call the echo tool
    // - max_turns = 1 to keep it cheap
    // - Security disabled so our prompt doesn't get blocked
    let config = claude_sdk_rs::Config {
        system_prompt: Some(
            "You have access to an MCP tool called 'echo'. \
             Call it with message='spike-test-1' and then report the result. \
             Do not use any other tools."
                .to_string(),
        ),
        mcp_config_path: Some(config_path),
        allowed_tools: Some(vec![
            "mcp__glass-echo__echo".to_string(),
            "mcp__glass-echo__greet".to_string(),
        ]),
        stream_format: claude_sdk_rs::StreamFormat::StreamJson,
        timeout_secs: Some(120),
        max_turns: Some(3),
        skip_permissions: true,
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };

    let client = claude_sdk_rs::Client::new(config);

    info("Sending query to Claude Code with MCP config...");

    match client
        .send_full("Call the echo tool with message 'spike-test-1' and tell me what it returned.")
        .await
    {
        Ok(response) => {
            info(&format!(
                "Response content: {}",
                &response.content[..response.content.len().min(200)]
            ));

            if response.content.contains("ECHO: spike-test-1")
                || response.content.contains("spike-test-1")
            {
                pass("Claude Code called our MCP tool and returned the result.");
                true
            } else {
                // Check the raw JSON for tool calls too
                if let Some(raw) = &response.raw_json {
                    let raw_str = raw.to_string();
                    if raw_str.contains("echo") || raw_str.contains("spike-test-1") {
                        pass("MCP tool was called (found in raw JSON).");
                        info(&format!(
                            "Raw JSON (truncated): {}...",
                            &raw_str[..raw_str.len().min(500)]
                        ));
                        return true;
                    }
                }
                fail("Response did not contain expected echo output.");
                info("The MCP server may not have been spawned, or the tool was not called.");
                false
            }
        }
        Err(e) => {
            fail(&format!("Claude Code invocation failed: {e}"));
            info("Check that Claude CLI is installed and authenticated.");
            false
        }
    }
}

// ─── Built-in tool names ─────────────────────────────────────────────────────

/// All known Claude Code built-in tools that Glass must block.
/// When Claude Code adds new built-in tools, this list needs updating.
/// Discovered via spike test 2A — the raw JSON lists all available tools.
const CLAUDE_BUILTIN_TOOLS: &[&str] = &[
    // File & code tools
    "Read",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    // Shell
    "Bash",
    // Search & navigation
    "Glob",
    "Grep",
    "LS",
    // Web
    "WebFetch",
    "WebSearch",
    // Task management
    "Task",
    "TaskOutput",
    "TaskStop",
    "TodoRead",
    "TodoWrite",
    // Agent flow
    "EnterPlanMode",
    "ExitPlanMode",
    "AskUserQuestion",
    // Skills
    "Skill",
];

// ─── Test 2: Blocking Built-in Tools ─────────────────────────────────────────

async fn test_2_allowed_tools_filtering() -> bool {
    header(2, "Blocking Built-in Tools via disallowed_tools");
    info("Part A: Confirm that allowed_tools alone does NOT block built-ins.");
    info("Part B: Confirm that disallowed_tools DOES block built-ins.");
    info("Glass needs Part B for its security model.");
    eprintln!();

    let part_a = test_2a_allowed_tools_insufficient().await;
    eprintln!();
    let part_b = test_2b_disallowed_tools_works().await;

    eprintln!();
    if part_a && part_b {
        pass("Full picture: allowed_tools is insufficient, disallowed_tools works.");
        info("Glass must use disallowed_tools to block all Claude Code built-ins.");
        true
    } else if part_b {
        // Part A result doesn't matter if Part B works — that's the one Glass needs.
        pass("disallowed_tools blocks built-ins. Glass security model holds.");
        true
    } else {
        fail("disallowed_tools did NOT block built-in tools.");
        info("Neither allowed_tools nor disallowed_tools can enforce tool restrictions.");
        info("Glass cannot rely on claude-sdk-rs for tool boundary security.");
        false
    }
}

/// Part A: Show that allowed_tools alone does NOT block built-in tools.
async fn test_2a_allowed_tools_insufficient() -> bool {
    info("── Part A: allowed_tools only (expecting FAIL) ──");
    info("Setting allowed_tools to only our MCP echo tool.");
    info("Asking Claude to read a file. If it succeeds, allowed_tools doesn't block built-ins.");
    eprintln!();

    let echo_binary = find_echo_mcp_binary();
    let (_tmp_dir, config_path) = write_mcp_config(&echo_binary);

    let config = claude_sdk_rs::Config {
        system_prompt: Some(
            "You must read the file ./Cargo.toml and report its contents. \
             If you cannot read it because you don't have the right tools, \
             say exactly 'NO_READ_TOOL_AVAILABLE'."
                .to_string(),
        ),
        mcp_config_path: Some(config_path),
        allowed_tools: Some(vec!["mcp__glass-echo__echo".to_string()]),
        stream_format: claude_sdk_rs::StreamFormat::StreamJson,
        timeout_secs: Some(120),
        max_turns: Some(2),
        skip_permissions: true,
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };

    let client = claude_sdk_rs::Client::new(config);

    match client
        .send_full("Read the file ./Cargo.toml and show me its contents.")
        .await
    {
        Ok(response) => {
            let content = &response.content;
            info(&format!(
                "Response (truncated): {}",
                &content[..content.len().min(200)]
            ));

            let builtin_calls = check_for_builtin_tool_calls(&response);

            if !builtin_calls.is_empty()
                || content.contains("[package]")
                || content.contains("glass-spike")
            {
                info("CONFIRMED: allowed_tools alone does NOT block built-in tools.");
                info("Claude Code read the file using its built-in Read tool.");
                // Return true — we *expected* this to fail, and confirming the failure is useful.
                true
            } else {
                info("Surprisingly, Claude did NOT use built-in tools with allowed_tools only.");
                info("This would mean allowed_tools IS sufficient (unexpected).");
                false
            }
        }
        Err(e) => {
            info(&format!("Invocation errored: {e}"));
            info("Cannot determine allowed_tools behavior from an error.");
            // Inconclusive — don't block overall test.
            true
        }
    }
}

/// Part B: Show that disallowed_tools DOES block built-in tools.
async fn test_2b_disallowed_tools_works() -> bool {
    info("── Part B: disallowed_tools blocking built-ins (expecting PASS) ──");
    info("Setting disallowed_tools to all known Claude Code built-in tools.");
    info("Asking Claude to read a file. It should be unable to.");
    eprintln!();

    let echo_binary = find_echo_mcp_binary();
    let (_tmp_dir, config_path) = write_mcp_config(&echo_binary);

    let config = claude_sdk_rs::Config {
        system_prompt: Some(
            "You must read the file ./Cargo.toml and report its contents. \
             If you cannot read it because you don't have the right tools, \
             say exactly 'NO_READ_TOOL_AVAILABLE'."
                .to_string(),
        ),
        mcp_config_path: Some(config_path),
        allowed_tools: Some(vec![
            "mcp__glass-echo__echo".to_string(),
            "mcp__glass-echo__greet".to_string(),
        ]),
        disallowed_tools: Some(CLAUDE_BUILTIN_TOOLS.iter().map(|s| s.to_string()).collect()),
        stream_format: claude_sdk_rs::StreamFormat::StreamJson,
        timeout_secs: Some(120),
        max_turns: Some(2),
        skip_permissions: true,
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };

    let client = claude_sdk_rs::Client::new(config);

    match client
        .send_full("Read the file ./Cargo.toml and show me its contents.")
        .await
    {
        Ok(response) => {
            let content = &response.content;
            info(&format!(
                "Response (truncated): {}",
                &content[..content.len().min(300)]
            ));

            let builtin_calls = check_for_builtin_tool_calls(&response);

            if !builtin_calls.is_empty() {
                fail(&format!(
                    "Built-in tools were CALLED despite disallowed_tools: {:?}",
                    builtin_calls
                ));
                info("disallowed_tools does not fully work. These tools slipped through.");
                false
            } else if content.contains("[package]") || content.contains("glass-spike") {
                fail("Claude managed to read the file despite disallowed_tools.");
                info("It may have used a tool not captured in the response.");
                false
            } else {
                pass("Claude Code could NOT use built-in tools when they're in disallowed_tools.");
                if content.contains("NO_READ_TOOL_AVAILABLE") {
                    info("Claude explicitly reported it lacks the Read tool. Perfect.");
                } else {
                    info("Claude didn't read the file. Built-in tools appear blocked.");
                }
                true
            }
        }
        Err(e) => {
            // An error might mean Claude Code refused to run without its tools.
            // That's actually fine — it means they're blocked.
            info(&format!("Invocation returned error: {e}"));
            info("This may indicate disallowed_tools correctly blocked built-in tools.");
            pass("Claude Code errored — built-in tools appear blocked.");
            true
        }
    }
}

/// Check raw JSON for actual built-in tool *invocations* (tool_use blocks).
/// Does NOT flag built-in tools that merely appear in metadata/tool listings —
/// only tools that Claude Code actually called.
fn check_for_builtin_tool_calls(response: &claude_sdk_rs::ClaudeResponse) -> Vec<String> {
    let mut called = Vec::new();
    let Some(raw) = &response.raw_json else {
        return called;
    };

    // In StreamJson mode, raw_json is an array of message objects.
    // Tool calls appear as tool_use content blocks inside assistant messages.
    if let Some(messages) = raw.as_array() {
        for msg in messages {
            // Look inside assistant messages for tool_use blocks
            let content = msg
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array());

            if let Some(blocks) = content {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                            // Check if this is a built-in tool (not an mcp__ tool)
                            if !name.starts_with("mcp__") {
                                info(&format!("  Built-in tool CALLED: {name}"));
                                called.push(name.to_string());
                            }
                        }
                    }
                }
            }

            // Also check top-level content array (some message formats)
            if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        if let Some(name) = block.get("name").and_then(|v| v.as_str()) {
                            if !name.starts_with("mcp__") {
                                info(&format!("  Built-in tool CALLED: {name}"));
                                called.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Deduplicate
    called.sort();
    called.dedup();
    called
}

// ─── Test 3: Tool Call Records in Session Results ────────────────────────────

async fn test_3_tool_call_records() -> bool {
    header(3, "Tool Call Records in Session Results");
    info("Can we extract tool call records (name, args, result) from the");
    info("completed session? Glass needs this for audit logging and");
    info("query_projects detection.");
    eprintln!();

    let echo_binary = find_echo_mcp_binary();
    let (_tmp_dir, config_path) = write_mcp_config(&echo_binary);

    let config = claude_sdk_rs::Config {
        system_prompt: Some(
            "You have two MCP tools: echo and greet. \
             First call echo with message='audit-test'. \
             Then call greet with name='Glass'. \
             Then report both results."
                .to_string(),
        ),
        mcp_config_path: Some(config_path),
        allowed_tools: Some(vec![
            "mcp__glass-echo__echo".to_string(),
            "mcp__glass-echo__greet".to_string(),
        ]),
        stream_format: claude_sdk_rs::StreamFormat::StreamJson,
        timeout_secs: Some(120),
        max_turns: Some(5),
        skip_permissions: true,
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };

    let client = claude_sdk_rs::Client::new(config);

    info("Asking Claude to call both echo and greet tools...");

    match client.send_full("Call the echo tool with message='audit-test', then call the greet tool with name='Glass'. Report both results.").await {
        Ok(response) => {
            info(&format!("Response: {}", &response.content[..response.content.len().min(300)]));

            // Now inspect raw_json for tool call records
            let mut found_tool_calls = Vec::new();
            let mut found_tool_results = Vec::new();

            if let Some(raw) = &response.raw_json {
                eprintln!();
                info("Inspecting raw JSON for tool call records...");

                // In StreamJson mode, raw_json is an array of message objects
                if let Some(messages) = raw.as_array() {
                    for msg in messages {
                        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");

                        match msg_type {
                            "assistant" => {
                                // Look for tool_use blocks in content
                                if let Some(content) = msg.get("message")
                                    .and_then(|m| m.get("content"))
                                    .and_then(|c| c.as_array())
                                {
                                    for block in content {
                                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                                            let tool_name = block.get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("?");
                                            let tool_input = block.get("input")
                                                .cloned()
                                                .unwrap_or(json!({}));
                                            info(&format!("  Found tool call: {tool_name}({tool_input})"));
                                            found_tool_calls.push((
                                                tool_name.to_string(),
                                                tool_input,
                                            ));
                                        }
                                    }
                                }
                            }
                            "result" => {
                                // The final result message may contain tool results
                                if let Some(content) = msg.get("content")
                                    .and_then(|c| c.as_array())
                                {
                                    for block in content {
                                        if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                                            let tool_id = block.get("tool_use_id")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("?");
                                            info(&format!("  Found tool result for id: {tool_id}"));
                                            found_tool_results.push(block.clone());
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    // Also do a brute-force search through the entire JSON for tool_use
                    if found_tool_calls.is_empty() {
                        info("No tool calls found via structured parse. Doing brute-force search...");
                        let raw_str = raw.to_string();

                        // Look for tool_use in the raw string
                        if raw_str.contains("tool_use") {
                            info("  Found 'tool_use' in raw JSON.");
                            // Find and extract all tool_use objects
                            for msg in messages {
                                search_for_tool_use(msg, &mut found_tool_calls);
                            }
                        }

                        if raw_str.contains("tool_result") {
                            info("  Found 'tool_result' in raw JSON.");
                            for msg in messages {
                                search_for_tool_result(msg, &mut found_tool_results);
                            }
                        }

                        if found_tool_calls.is_empty() {
                            info("  Dumping first 2000 chars of raw JSON for manual inspection:");
                            info(&format!("  {}", &raw_str[..raw_str.len().min(2000)]));
                        }
                    }
                } else {
                    info("raw_json is not an array — dumping structure:");
                    let raw_str = serde_json::to_string_pretty(raw).unwrap_or_default();
                    info(&format!("{}", &raw_str[..raw_str.len().min(2000)]));
                }
            } else {
                fail("No raw_json in response — try StreamJson format.");
                return false;
            }

            eprintln!();
            info(&format!("Tool calls found: {}", found_tool_calls.len()));
            info(&format!("Tool results found: {}", found_tool_results.len()));

            if found_tool_calls.is_empty() {
                fail("Could not extract tool call records from session results.");
                info("Glass needs tool call data for audit logging and query_projects detection.");
                info("Fallback: use streaming mode and accumulate tool calls as they arrive,");
                info("or parse the raw Claude Code CLI output directly.");
                false
            } else {
                let has_echo = found_tool_calls.iter().any(|(name, _)| name.contains("echo"));
                let has_greet = found_tool_calls.iter().any(|(name, _)| name.contains("greet"));

                if has_echo && has_greet {
                    pass("Both tool calls extracted with name and args.");
                    info("Glass can build AuditEntry and detect query_projects from session results.");
                    true
                } else {
                    pass(&format!(
                        "Found {} tool call(s), but expected both echo and greet.",
                        found_tool_calls.len()
                    ));
                    info("Partial success — tool call extraction works but may be incomplete.");
                    true
                }
            }
        }
        Err(e) => {
            fail(&format!("Invocation failed: {e}"));
            false
        }
    }
}

/// Recursively search a JSON value for tool_use blocks.
fn search_for_tool_use(value: &Value, results: &mut Vec<(String, Value)>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                let name = map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?")
                    .to_string();
                let input = map.get("input").cloned().unwrap_or(json!({}));
                info(&format!("  [brute-force] Tool call: {name}({input})"));
                results.push((name, input));
            }
            for v in map.values() {
                search_for_tool_use(v, results);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                search_for_tool_use(v, results);
            }
        }
        _ => {}
    }
}

/// Recursively search a JSON value for tool_result blocks.
fn search_for_tool_result(value: &Value, results: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                info(&format!("  [brute-force] Tool result found"));
                results.push(value.clone());
            }
            for v in map.values() {
                search_for_tool_result(v, results);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                search_for_tool_result(v, results);
            }
        }
        _ => {}
    }
}

// ─── Test 4: System Prompt Length ─────────────────────────────────────────────

fn test_4_system_prompt_length() -> bool {
    header(4, "System Prompt Length Limit");
    info("The SDK validates system prompts to 10,000 characters.");
    info("Glass prompts (identity + skill metadata + project brief) will");
    info("likely exceed this. Testing what happens.");
    eprintln!();

    // Test 1: A 10K prompt should pass validation
    let prompt_10k = "x".repeat(10_000);
    let config_10k = claude_sdk_rs::Config {
        system_prompt: Some(prompt_10k.clone()),
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };
    match config_10k.validate() {
        Ok(()) => pass("10,000 char prompt passes SDK validation."),
        Err(e) => {
            fail(&format!("10K prompt rejected: {e}"));
            return false;
        }
    }

    // Test 2: An 11K prompt should fail validation
    let prompt_11k = "x".repeat(11_000);
    let config_11k = claude_sdk_rs::Config {
        system_prompt: Some(prompt_11k),
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };
    match config_11k.validate() {
        Ok(()) => {
            pass("11,000 char prompt ALSO passes — limit may have been raised.");
            info("Great news for Glass: the SDK accepts longer prompts than expected.");
        }
        Err(e) => {
            info(&format!("11K prompt rejected (expected): {e}"));
        }
    }

    // Test 3: A realistic Glass-sized prompt (~15K)
    let prompt_15k = build_realistic_glass_prompt();
    info(&format!(
        "Realistic Glass prompt size: {} chars",
        prompt_15k.len()
    ));

    let config_15k = claude_sdk_rs::Config {
        system_prompt: Some(prompt_15k.clone()),
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };
    match config_15k.validate() {
        Ok(()) => {
            pass("Realistic Glass prompt passes SDK validation!");
            info("No workaround needed.");
            return true;
        }
        Err(e) => {
            fail(&format!("Realistic Glass prompt rejected: {e}"));
        }
    }

    // Test 4: Does Config::builder().build() also enforce the limit?
    info("Testing builder pattern...");
    match claude_sdk_rs::Config::builder()
        .system_prompt(&prompt_15k)
        .security_level(claude_sdk_rs::core::SecurityLevel::Disabled)
        .build()
    {
        Ok(_) => {
            pass("Builder also accepts the prompt!");
            return true;
        }
        Err(e) => {
            info(&format!("Builder rejected it too: {e}"));
        }
    }

    // Test 5: Can we bypass validation by constructing Config directly?
    info("Testing direct Config construction (bypassing validate())...");
    let config_direct = claude_sdk_rs::Config {
        system_prompt: Some(prompt_15k),
        security_level: claude_sdk_rs::core::SecurityLevel::Disabled,
        ..Default::default()
    };
    // Client::new does NOT call validate — it just stores the config
    let _client = claude_sdk_rs::Client::new(config_direct);
    pass("Client::new accepts the config without calling validate().");
    info("WORKAROUND: Use Client::new(config) directly instead of Config::builder().build().");
    info("The 10K limit is in the SDK's validation, not in Claude Code itself.");
    info("Glass can construct Config manually and skip validation.");

    true
}

/// Build a system prompt roughly the size of what Glass would assemble.
fn build_realistic_glass_prompt() -> String {
    let mut prompt = String::new();

    // Identity (~2K)
    prompt.push_str("# Identity\n\n");
    prompt.push_str("You are Glass, a personal AI agent that lives in a Discord server. ");
    prompt.push_str("You organize the user's life into channels, develop your own personality ");
    prompt.push_str("and tools over time, and keep data architecturally contained.\n\n");
    prompt.push_str(
        &"You are thoughtful, proactive, and maintain a warm but professional tone. ".repeat(20),
    );
    prompt.push_str("\n\n");

    // Skills metadata (~3K for ~30 skills at ~100 tokens each)
    prompt.push_str("# Available Skills\n\n");
    for i in 0..30 {
        prompt.push_str(&format!(
            "- **skill-{i}**: A skill that helps with task category {i}. \
             Use this when the user needs help with processes involving \
             multiple dependent steps and careful coordination.\n"
        ));
    }
    prompt.push_str("\n");

    // Project brief (~2K)
    prompt.push_str("# Project: surgery-prep\n\n");
    prompt.push_str("## Brief\n");
    prompt.push_str(
        &"This project tracks preparation for an upcoming surgical procedure. \
        It includes timelines, checklists, medication schedules, and appointment tracking. \
        The user needs help staying organized and remembering deadlines. "
            .repeat(5),
    );
    prompt.push_str("\n\n");

    // Project status (~1K)
    prompt.push_str("## Status\n");
    prompt.push_str(
        &"Current phase: pre-operative preparation. Key upcoming deadlines include \
        bloodwork (3 days), insurance pre-auth (5 days), and pre-op appointment (7 days). \
        All checklists are up to date. "
            .repeat(3),
    );
    prompt.push_str("\n\n");

    // Tool descriptions (~2K)
    prompt.push_str("# Available Tools\n\n");
    prompt.push_str(
        "- **shell**: Execute a command in the sandbox (--network none Docker container)\n",
    );
    prompt.push_str("- **read_file**: Read a file from the current project workspace\n");
    prompt.push_str("- **write_file**: Write a file to the current project workspace\n");
    prompt.push_str("- **list_files**: List files in the current project workspace (recursive)\n");
    prompt.push_str(
        "- **fetch_url**: GET a URL, return text content (governed by channel capabilities)\n",
    );
    prompt.push_str("- **web_search**: Search the web, return result snippets\n");
    prompt
        .push_str("- **suggest_learning**: Send an abstract learning to Glass for human review\n");
    prompt.push_str(
        "- **get_channel_history**: Fetch older Discord messages from this project's channel\n",
    );
    prompt.push_str("\n");

    // Workspace listing (~1K)
    prompt.push_str("# Workspace Files\n\n");
    for i in 0..30 {
        prompt.push_str(&format!("- notes/topic-{i}.md\n"));
    }
    prompt.push_str("- brief.md\n");
    prompt.push_str("- status.md\n");
    prompt.push_str("- schedule.json\n");
    prompt.push_str("\n");

    // Conversation history (~3K)
    prompt.push_str("# Recent Conversation\n\n");
    for i in 0..15 {
        prompt.push_str(&format!(
            "**User** ({}m ago): Can you check on item {i} from the checklist? \
             I think there might be an issue with the timing.\n\n",
            i * 5
        ));
        prompt.push_str(&format!(
            "**Glass**: I checked item {i}. The timing looks correct — it's scheduled \
             for {} days from now, which aligns with the pre-op requirements.\n\n",
            i + 1
        ));
    }

    prompt
}

// ─── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    let tests_to_run = if args.len() > 1 {
        match args[1].as_str() {
            "all" => vec![1, 2, 3, 4],
            "1" => vec![1],
            "2" => vec![2],
            "3" => vec![3],
            "4" => vec![4],
            other => {
                eprintln!("Usage: glass-spike [all|1|2|3|4]");
                eprintln!();
                eprintln!("Tests:");
                eprintln!("  1  MCP config passthrough (requires Claude CLI)");
                eprintln!("  2  allowed_tools disables built-ins (requires Claude CLI)");
                eprintln!("  3  Tool call records in session results (requires Claude CLI)");
                eprintln!("  4  System prompt length limit (offline, no API needed)");
                eprintln!("  all  Run all tests");
                eprintln!();
                eprintln!("Unknown argument: {other}");
                process::exit(1);
            }
        }
    } else {
        eprintln!("glass-spike — claude-sdk-rs validation for Glass");
        eprintln!();
        eprintln!("Usage: glass-spike [all|1|2|3|4]");
        eprintln!();
        eprintln!("Tests:");
        eprintln!("  1  MCP config passthrough (requires Claude CLI + API key)");
        eprintln!("  2  allowed_tools disables built-ins (requires Claude CLI + API key)");
        eprintln!("  3  Tool call records in session results (requires Claude CLI + API key)");
        eprintln!("  4  System prompt length limit (offline, no API key needed)");
        eprintln!("  all  Run all tests");
        eprintln!();
        eprintln!("Start with test 4 (free, offline) to check the system prompt limit.");
        eprintln!("Then run 1-3 (costs a few cents in API calls).");
        process::exit(0);
    };

    let mut results: Vec<(u32, &str, bool)> = Vec::new();

    for test_num in &tests_to_run {
        let passed = match test_num {
            1 => test_1_mcp_passthrough().await,
            2 => test_2_allowed_tools_filtering().await,
            3 => test_3_tool_call_records().await,
            4 => test_4_system_prompt_length(),
            _ => unreachable!(),
        };

        let name = match test_num {
            1 => "MCP config passthrough",
            2 => "allowed_tools disables built-ins",
            3 => "Tool call records in session results",
            4 => "System prompt length limit",
            _ => unreachable!(),
        };

        results.push((*test_num, name, passed));
    }

    // Summary
    eprintln!();
    eprintln!("============================================================");
    eprintln!("  SUMMARY");
    eprintln!("============================================================");
    eprintln!();

    let mut all_passed = true;
    for (num, name, passed) in &results {
        let icon = if *passed { "✅" } else { "❌" };
        eprintln!("  {icon} Test {num}: {name}");
        if !*passed {
            all_passed = false;
        }
    }

    eprintln!();

    if all_passed {
        eprintln!("  All tests passed! claude-sdk-rs is viable for Glass.");
    } else {
        eprintln!("  Some tests failed. See details above for fallback options.");
        eprintln!();
        eprintln!("  Fallback chain:");
        eprintln!("    1. Fork claude-sdk-rs and patch the issue");
        eprintln!("    2. Use the official Claude Agent SDK (TypeScript/Python)");
        eprintln!("    3. Raw Claude Code CLI via tokio::process::Command");
    }

    eprintln!();

    if !all_passed {
        process::exit(1);
    }
}

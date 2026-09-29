// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests of the HTTP endpoint with fake CLIs written as shell scripts.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use switchyard_cli_bridge::cli::Programs;
use switchyard_cli_bridge::{Config, router};
use tempfile::TempDir;
use tower::ServiceExt as _;

// Tests run one at a time. Writing an executable while another test thread forks a CLI can
// make the exec fail with "text file busy".
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Writes a fake CLI that records its arguments, stdin, and `--prompt-file` contents next to
/// itself, then prints `stdout` and exits with `exit_code`.
fn fake_cli(dir: &Path, name: &str, stdout: &str, exit_code: i32) -> PathBuf {
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > \"$0.args\"\n\
         cat > \"$0.stdin\"\n\
         while [ $# -gt 0 ]; do\n\
           if [ \"$1\" = --prompt-file ]; then cp \"$2\" \"$0.prompt\"; fi\n\
           shift\n\
         done\n\
         echo 'progress on stderr' >&2\n\
         cat <<'EOF'\n{stdout}\nEOF\n\
         exit {exit_code}\n"
    );
    fs::write(&path, script).expect("write fake CLI");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .expect("make fake CLI executable");
    path
}

fn config(dir: &TempDir, programs: Programs) -> Config {
    Config {
        programs,
        workdir: dir.path().to_path_buf(),
        timeout: Duration::from_secs(10),
    }
}

async fn post(config: Config, body: Value) -> (StatusCode, String) {
    let request = Request::post("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("request");
    let response = router(config).oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        String::from_utf8(bytes.to_vec()).expect("utf-8 body"),
    )
}

fn recorded(program: &Path, suffix: &str) -> String {
    fs::read_to_string(format!("{}.{suffix}", program.display())).expect("recorded file")
}

#[tokio::test]
async fn claude_answers_with_text_and_usage() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let claude = fake_cli(
        dir.path(),
        "claude",
        r#"{"type":"result","is_error":false,"result":"Hello from Claude","usage":{"input_tokens":12,"output_tokens":3}}"#,
        0,
    );
    let programs = Programs {
        claude: claude.clone(),
        ..Programs::default()
    };

    let (status, body) = post(
        config(&dir, programs),
        json!({"model": "claude/opus", "messages": [{"role": "user", "content": "Say hello"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["model"], "claude/opus");
    assert_eq!(
        body["choices"][0]["message"]["content"],
        "Hello from Claude"
    );
    assert_eq!(body["choices"][0]["finish_reason"], "stop");
    assert_eq!(body["usage"]["total_tokens"], 15);

    let args = recorded(&claude, "args");
    assert!(
        args.starts_with("-p\n--output-format\njson\n--tools\n\n"),
        "{args}"
    );
    assert!(args.ends_with("--model\nopus\n"), "{args}");
    assert!(recorded(&claude, "stdin").contains("<user>\nSay hello\n</user>"));
}

#[tokio::test]
async fn codex_reads_the_prompt_from_stdin() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let codex = fake_cli(dir.path(), "codex", "Hello from Codex", 0);
    let programs = Programs {
        codex: codex.clone(),
        ..Programs::default()
    };

    let (status, body) = post(
        config(&dir, programs),
        json!({"model": "codex", "messages": [{"role": "user", "content": "Say hello"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from Codex");
    let args = recorded(&codex, "args");
    assert!(args.starts_with("exec\n"), "{args}");
    assert!(args.ends_with("\n-\n"), "{args}");
    assert!(!args.contains("--model"), "{args}");
    assert!(recorded(&codex, "stdin").contains("Say hello"));
}

#[tokio::test]
async fn grok_reads_the_prompt_from_a_file() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let grok = fake_cli(
        dir.path(),
        "grok",
        r#"{"text":"Hello from Grok","stopReason":"end_turn","usage":{"input_tokens":4,"output_tokens":2}}"#,
        0,
    );
    let programs = Programs {
        grok: grok.clone(),
        ..Programs::default()
    };

    let (status, body) = post(
        config(&dir, programs),
        json!({"model": "grok/grok-4.6", "messages": [{"role": "user", "content": "Say hello"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(body["choices"][0]["message"]["content"], "Hello from Grok");
    assert!(recorded(&grok, "args").ends_with("--model\ngrok-4.6\n"));
    assert!(recorded(&grok, "prompt").contains("<user>\nSay hello\n</user>"));
}

#[tokio::test]
async fn tool_calls_are_returned_as_structured_calls() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let answer = json!({"tool_calls": [{"name": "get_weather", "arguments": {"city": "Paris"}}]});
    let claude_output = json!({"is_error": false, "result": answer.to_string()});
    let claude = fake_cli(dir.path(), "claude", &claude_output.to_string(), 0);
    let programs = Programs {
        claude: claude.clone(),
        ..Programs::default()
    };
    let request = json!({
        "model": "claude",
        "messages": [{"role": "user", "content": "Weather in Paris?"}],
        "tools": [{"type": "function", "function": {
            "name": "get_weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }}]
    });

    let (status, body) = post(config(&dir, programs.clone()), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let body: Value = serde_json::from_str(&body).expect("json");
    let choice = &body["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(choice["message"]["content"], Value::Null);
    let call = &choice["message"]["tool_calls"][0];
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["name"], "get_weather");
    let arguments: Value = serde_json::from_str(
        call["function"]["arguments"]
            .as_str()
            .expect("arguments string"),
    )
    .expect("arguments json");
    assert_eq!(arguments, json!({"city": "Paris"}));
    assert!(recorded(&claude, "stdin").contains("# Tools"));

    // The same answer, streamed.
    let mut request = request;
    request["stream"] = json!(true);
    request["stream_options"] = json!({"include_usage": true});
    let (status, body) = post(config(&dir, programs), request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events: Vec<&str> = body
        .split("\n\n")
        .filter_map(|event| event.strip_prefix("data: "))
        .collect();
    assert_eq!(events.last(), Some(&"[DONE]"));
    let chunks: Vec<Value> = events[..events.len() - 1]
        .iter()
        .map(|event| serde_json::from_str(event).expect("chunk json"))
        .collect();
    let delta = &chunks[0]["choices"][0]["delta"];
    assert_eq!(delta["tool_calls"][0]["index"], 0);
    assert_eq!(delta["tool_calls"][0]["function"]["name"], "get_weather");
    assert_eq!(chunks[1]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(chunks[2]["choices"], json!([]));
    assert!(chunks[2]["usage"].is_object());
}

#[tokio::test]
async fn cli_failures_become_gateway_errors() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let codex = fake_cli(dir.path(), "codex", "Not logged in", 1);
    let programs = Programs {
        codex,
        claude: dir.path().join("missing-claude"),
        ..Programs::default()
    };
    let hello =
        |model: &str| json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});

    let (status, body) = post(config(&dir, programs.clone()), hello("codex")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("progress on stderr"), "{body}");

    let (status, body) = post(config(&dir, programs.clone()), hello("claude")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("could not start"), "{body}");

    let (status, body) = post(config(&dir, programs), hello("gpt-4o")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("unknown model"), "{body}");
}

#[tokio::test]
async fn slow_cli_calls_time_out() {
    let _serial = SERIAL.lock().await;
    let dir = TempDir::new().expect("temp dir");
    let grok = dir.path().join("grok");
    fs::write(&grok, "#!/bin/sh\nexec sleep 5\n").expect("write fake CLI");
    fs::set_permissions(&grok, fs::Permissions::from_mode(0o755)).expect("make executable");
    let config = Config {
        timeout: Duration::from_millis(200),
        ..config(
            &dir,
            Programs {
                grok,
                ..Programs::default()
            },
        )
    };

    let (status, body) = post(
        config,
        json!({"model": "grok", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(body.contains("did not finish"), "{body}");
}

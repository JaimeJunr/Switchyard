// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! A local OpenAI Chat Completions endpoint served by the Claude Code, Codex, and Grok CLIs.
//!
//! Switchyard reaches it like any other `openai_chat` upstream. The model id picks the CLI:
//! `claude`, `codex`, or `grok`, optionally followed by `/<model>` to pass `--model`.
//! Each request runs the CLI once, with the user's existing CLI login.

pub mod cli;
pub mod prompt;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::cli::{Cli, Programs, Usage};
use crate::prompt::{ChatRequest, Reply};

/// Settings shared by every request.
#[derive(Clone, Debug)]
pub struct Config {
    pub programs: Programs,
    /// Directory the CLIs run in.
    pub workdir: PathBuf,
    /// Longest time one CLI call may take.
    pub timeout: Duration,
}

/// Builds the HTTP routes.
pub fn router(config: Config) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(Arc::new(config))
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn models() -> Json<Value> {
    let data: Vec<Value> = Cli::ALL
        .iter()
        .map(
            |cli| json!({"id": cli.name(), "object": "model", "owned_by": "switchyard-cli-bridge"}),
        )
        .collect();
    Json(json!({"object": "list", "data": data}))
}

async fn chat_completions(State(config): State<Arc<Config>>, body: Bytes) -> Response {
    let request: ChatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                format!("invalid request body: {error}"),
            );
        }
    };
    let Some((cli, model)) = Cli::parse_model(&request.model) else {
        return error_response(
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            format!(
                "unknown model {:?}: use claude, codex, or grok, optionally followed by /<model>",
                request.model
            ),
        );
    };

    let started = Instant::now();
    let prompt = prompt::render(&request);
    let result = cli::run(
        cli,
        config.programs.get(cli),
        model,
        &prompt,
        &config.workdir,
        config.timeout,
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis();

    let output = match result {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(model = %request.model, elapsed_ms, %error, "CLI call failed");
            let status =
                StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            return error_response(status, "cli_error", error.to_string());
        }
    };
    tracing::info!(model = %request.model, elapsed_ms, "CLI call finished");

    let tool_names: Vec<&str> = request
        .offered_tools()
        .into_iter()
        .map(|tool| tool.name.as_str())
        .collect();
    let completion = Completion {
        id: completion_id(),
        created: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        model: request.model.clone(),
        reply: prompt::parse_reply(&output.text, &tool_names),
        usage: output.usage,
    };

    if request.is_stream() {
        let events = completion.stream_events(request.includes_stream_usage());
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from(events))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
    } else {
        Json(completion.to_json()).into_response()
    }
}

/// A finished answer, ready to encode as a Chat Completions response.
struct Completion {
    id: String,
    created: u64,
    model: String,
    reply: Reply,
    usage: Usage,
}

impl Completion {
    fn to_json(&self) -> Value {
        let mut message = json!({"role": "assistant", "content": self.text()});
        if let Some(tool_calls) = self.tool_calls_json(false) {
            message["tool_calls"] = tool_calls;
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "message": message, "finish_reason": self.finish_reason()}],
            "usage": self.usage_json(),
        })
    }

    /// The whole answer as Server-Sent Events. The CLI has already finished, so the answer
    /// is sent in a few large chunks rather than token by token.
    fn stream_events(&self, include_usage: bool) -> String {
        let mut delta = json!({"role": "assistant", "content": self.text().unwrap_or_default()});
        if let Some(tool_calls) = self.tool_calls_json(true) {
            delta["tool_calls"] = tool_calls;
        }
        let mut chunks = vec![
            self.chunk(json!([{"index": 0, "delta": delta, "finish_reason": null}])),
            self.chunk(json!([{"index": 0, "delta": {}, "finish_reason": self.finish_reason()}])),
        ];
        if include_usage {
            let mut usage_chunk = self.chunk(json!([]));
            usage_chunk["usage"] = self.usage_json();
            chunks.push(usage_chunk);
        }

        let mut events = String::new();
        for chunk in chunks {
            events.push_str(&format!("data: {chunk}\n\n"));
        }
        events.push_str("data: [DONE]\n\n");
        events
    }

    fn chunk(&self, choices: Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": choices,
        })
    }

    fn text(&self) -> Option<&str> {
        match &self.reply {
            Reply::Text(text) => Some(text),
            Reply::ToolCalls { text, .. } => text.as_deref(),
        }
    }

    fn finish_reason(&self) -> &'static str {
        match self.reply {
            Reply::Text(_) => "stop",
            Reply::ToolCalls { .. } => "tool_calls",
        }
    }

    /// Tool calls in Chat Completions form. Stream deltas also carry each call's `index`.
    fn tool_calls_json(&self, streaming: bool) -> Option<Value> {
        let Reply::ToolCalls { calls, .. } = &self.reply else {
            return None;
        };
        let calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(index, call)| {
                let mut json = json!({
                    "id": format!("{}_{index}", self.id.replacen("chatcmpl", "call", 1)),
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments.to_string()},
                });
                if streaming {
                    json["index"] = json!(index);
                }
                json
            })
            .collect();
        Some(Value::Array(calls))
    }

    fn usage_json(&self) -> Value {
        json!({
            "prompt_tokens": self.usage.input_tokens,
            "completion_tokens": self.usage.output_tokens,
            "total_tokens": self.usage.input_tokens + self.usage.output_tokens,
        })
    }
}

fn completion_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("chatcmpl-{nanos:x}{count:x}")
}

fn error_response(status: StatusCode, kind: &str, message: String) -> Response {
    let body = json!({"error": {"message": message, "type": kind, "code": null}});
    (status, Json(body)).into_response()
}

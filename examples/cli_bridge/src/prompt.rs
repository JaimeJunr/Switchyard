// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Turns a Chat Completions request into one text prompt, and a CLI's answer back into
//! assistant text or tool calls.
//!
//! The CLIs accept plain text, not structured messages. The conversation, the offered tools,
//! and any required output format are written into one prompt. Tool calls are emulated: the
//! prompt asks the model to answer with a small JSON object when it wants to call tools, and
//! [`parse_reply`] turns that object back into structured calls.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const PREAMBLE: &str = "\
You are the language model behind a chat completion API. The conversation so far is below. \
Write the next assistant message.
Do not use any tools of your own. Do not read or change files and do not run commands. \
Use only the information in this prompt.
";

const TOOL_CALL_FORMAT: &str =
    r#"{"tool_calls":[{"name":"<tool name>","arguments":{<arguments as a JSON object>}}]}"#;

/// The parts of a Chat Completions request the bridge reads. Other fields are ignored.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub response_format: Option<Value>,
    #[serde(default)]
    pub stream: Option<bool>,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Debug, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default)]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct MessageToolCall {
    #[serde(default)]
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Debug, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub struct Tool {
    #[serde(default)]
    pub function: Option<FunctionDefinition>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct FunctionDefinition {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
}

impl ChatRequest {
    /// The function tools the model may call in this reply. Empty when `tool_choice` is `none`.
    pub fn offered_tools(&self) -> Vec<&FunctionDefinition> {
        if self.tool_choice.as_ref().and_then(Value::as_str) == Some("none") {
            return Vec::new();
        }
        self.tools
            .iter()
            .flatten()
            .filter_map(|tool| tool.function.as_ref())
            .collect()
    }

    /// Whether the caller asked for a streamed response.
    pub fn is_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }

    /// Whether a streamed response should end with a usage chunk.
    pub fn includes_stream_usage(&self) -> bool {
        self.stream_options
            .as_ref()
            .and_then(|options| options.include_usage)
            .unwrap_or(false)
    }
}

/// A tool call the model asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

/// The model's answer, after tool-call detection.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Text(String),
    ToolCalls {
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
}

/// Writes the whole request as one prompt.
pub fn render(request: &ChatRequest) -> String {
    let mut prompt = String::from(PREAMBLE);

    // Leading system and developer messages are instructions. Later ones stay in the
    // conversation, in order.
    let first_turn = request
        .messages
        .iter()
        .position(|message| !is_instruction(&message.role))
        .unwrap_or(request.messages.len());
    let (instructions, conversation) = request.messages.split_at(first_turn);

    if !instructions.is_empty() {
        prompt.push_str("\n# Instructions\n\n");
        let texts: Vec<String> = instructions
            .iter()
            .map(|message| content_text(&message.content))
            .collect();
        prompt.push_str(&texts.join("\n\n"));
        prompt.push('\n');
    }

    let tools = request.offered_tools();
    if !tools.is_empty() {
        prompt.push_str("\n# Tools\n\n");
        prompt.push_str(
            "You can call the tools listed below. To call tools, reply with only one JSON \
             object in this form and nothing else:\n",
        );
        prompt.push_str(TOOL_CALL_FORMAT);
        prompt.push_str(
            "\nList several entries to call several tools at once. \
             To answer without a tool, reply with plain text.\n",
        );
        if let Some(requirement) = tool_requirement(request.tool_choice.as_ref()) {
            prompt.push_str(&requirement);
            prompt.push('\n');
        }
        prompt.push_str("\nAvailable tools, one JSON definition per line:\n");
        for tool in tools {
            prompt.push_str(&json!(tool).to_string());
            prompt.push('\n');
        }
    }

    if let Some(format) = output_format(request.response_format.as_ref()) {
        prompt.push_str("\n# Output format\n\n");
        prompt.push_str(&format);
        prompt.push('\n');
    }

    prompt.push_str("\n# Conversation\n\n");
    for message in conversation {
        prompt.push_str(&render_message(message));
        prompt.push('\n');
    }

    prompt.push_str("\n# Your reply\n\nWrite only the next assistant message.\n");
    prompt
}

/// Reads tool calls out of the model's answer when it used the JSON form from the prompt.
///
/// Calls are accepted only for names in `tool_names`. Any text before the JSON object is kept
/// as message content. An answer that does not hold a valid call is returned as plain text.
pub fn parse_reply(answer: &str, tool_names: &[&str]) -> Reply {
    let answer = answer.trim();
    if tool_names.is_empty() {
        return Reply::Text(answer.to_string());
    }
    let Some(key) = answer.find("\"tool_calls\"") else {
        return Reply::Text(answer.to_string());
    };
    let Some(start) = answer[..key].rfind('{') else {
        return Reply::Text(answer.to_string());
    };
    let Some(Ok(object)) = serde_json::Deserializer::from_str(&answer[start..])
        .into_iter::<Value>()
        .next()
    else {
        return Reply::Text(answer.to_string());
    };
    let Some(calls) = read_tool_calls(&object, tool_names) else {
        return Reply::Text(answer.to_string());
    };

    // Models often wrap the object in a Markdown code fence.
    let text = answer[..start]
        .trim_end()
        .trim_end_matches("```json")
        .trim_end_matches("```")
        .trim();
    Reply::ToolCalls {
        text: (!text.is_empty()).then(|| text.to_string()),
        calls,
    }
}

fn read_tool_calls(object: &Value, tool_names: &[&str]) -> Option<Vec<ToolCall>> {
    let entries = object.get("tool_calls")?.as_array()?;
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .map(|entry| {
            let name = entry.get("name")?.as_str()?;
            if !tool_names.contains(&name) {
                return None;
            }
            let arguments = match entry.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(Value::String(text)) => serde_json::from_str(text).ok()?,
                Some(value) => value.clone(),
            };
            arguments.is_object().then(|| ToolCall {
                name: name.to_string(),
                arguments,
            })
        })
        .collect()
}

fn is_instruction(role: &str) -> bool {
    role == "system" || role == "developer"
}

fn tool_requirement(tool_choice: Option<&Value>) -> Option<String> {
    match tool_choice? {
        Value::String(choice) if choice == "required" => {
            Some("You must call at least one tool in this reply.".to_string())
        }
        Value::Object(choice) => {
            let name = choice.get("function")?.get("name")?.as_str()?;
            Some(format!("You must call the tool {name:?} in this reply."))
        }
        _ => None,
    }
}

fn output_format(response_format: Option<&Value>) -> Option<String> {
    let response_format = response_format?;
    let only_json = "Reply with only one valid JSON object. Do not add Markdown or any other text.";
    match response_format.get("type")?.as_str()? {
        "json_object" => Some(only_json.to_string()),
        "json_schema" => {
            let schema = response_format
                .get("json_schema")
                .and_then(|json_schema| json_schema.get("schema"))
                .map_or_else(String::new, Value::to_string);
            Some(format!(
                "{only_json}\nThe object must match this JSON Schema:\n{schema}"
            ))
        }
        _ => None,
    }
}

fn render_message(message: &Message) -> String {
    let text = content_text(&message.content);
    match message.role.as_str() {
        "assistant" => {
            let mut body = text;
            if let Some(calls) = message
                .tool_calls
                .as_ref()
                .filter(|calls| !calls.is_empty())
            {
                let calls: Vec<Value> = calls
                    .iter()
                    .map(|call| {
                        let arguments = serde_json::from_str(&call.function.arguments)
                            .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
                        json!({"id": call.id, "name": call.function.name, "arguments": arguments})
                    })
                    .collect();
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(&json!({"tool_calls": calls}).to_string());
            }
            format!("<assistant>\n{body}\n</assistant>")
        }
        "tool" => {
            let id = message.tool_call_id.as_deref().unwrap_or_default();
            format!("<tool_result tool_call_id={id:?}>\n{text}\n</tool_result>")
        }
        role => format!("<{role}>\n{text}\n</{role}>"),
    }
}

/// The readable text of a message's `content`, which is a string or a list of parts.
fn content_text(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().map(part_text).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

fn part_text(part: &Value) -> String {
    let field = |name: &str| {
        part.get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    match part.get("type").and_then(Value::as_str) {
        Some("text") => field("text"),
        Some("refusal") => field("refusal"),
        Some(kind) => format!("[{kind} omitted]"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> ChatRequest {
        serde_json::from_value(value).expect("valid request")
    }

    #[test]
    fn renders_instructions_conversation_and_tool_history() {
        let prompt = render(&request(json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [
                    {"type": "text", "text": "Weather in Paris?"},
                    {"type": "image_url", "image_url": {"url": "data:,"}}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "18C"}
            ]
        })));

        assert!(prompt.contains("# Instructions\n\nBe brief.\n"));
        assert!(prompt.contains("<user>\nWeather in Paris?\n[image_url omitted]\n</user>"));
        assert!(prompt.contains(
            r#"{"tool_calls":[{"id":"call_1","name":"get_weather","arguments":{"city":"Paris"}}]}"#
        ));
        assert!(prompt.contains("<tool_result tool_call_id=\"call_1\">\n18C\n</tool_result>"));
        assert!(!prompt.contains("# Tools"));
    }

    #[test]
    fn keeps_later_system_messages_in_order() {
        let prompt = render(&request(json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "system", "content": "reminder"},
                {"role": "user", "content": "second"}
            ]
        })));

        assert!(!prompt.contains("# Instructions"));
        let first = prompt.find("first").expect("first turn");
        let reminder = prompt
            .find("<system>\nreminder\n</system>")
            .expect("reminder");
        let second = prompt.find("second").expect("second turn");
        assert!(first < reminder && reminder < second);
    }

    #[test]
    fn describes_tools_and_required_choice() {
        let prompt = render(&request(json!({
            "model": "codex",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "Look up weather",
                "parameters": {"type": "object"}
            }}],
            "tool_choice": {"type": "function", "function": {"name": "get_weather"}}
        })));

        assert!(prompt.contains(TOOL_CALL_FORMAT));
        assert!(prompt.contains("You must call the tool \"get_weather\" in this reply."));
        assert!(prompt.contains(
            r#"{"name":"get_weather","description":"Look up weather","parameters":{"type":"object"}}"#
        ));
    }

    #[test]
    fn tool_choice_none_offers_no_tools() {
        let request = request(json!({
            "model": "codex",
            "messages": [],
            "tools": [{"type": "function", "function": {"name": "get_weather"}}],
            "tool_choice": "none"
        }));

        assert!(request.offered_tools().is_empty());
        assert!(!render(&request).contains("# Tools"));
    }

    #[test]
    fn describes_json_schema_output() {
        let prompt = render(&request(json!({
            "model": "grok",
            "messages": [],
            "response_format": {"type": "json_schema", "json_schema": {
                "name": "Verdict", "schema": {"type": "object"}
            }}
        })));

        assert!(prompt.contains("Reply with only one valid JSON object."));
        assert!(prompt.contains("The object must match this JSON Schema:\n{\"type\":\"object\"}"));
    }

    #[test]
    fn reads_tool_calls() {
        let reply = parse_reply(
            r#"{"tool_calls":[{"name":"get_weather","arguments":{"city":"Paris"}}]}"#,
            &["get_weather"],
        );
        assert_eq!(
            reply,
            Reply::ToolCalls {
                text: None,
                calls: vec![ToolCall {
                    name: "get_weather".to_string(),
                    arguments: json!({"city": "Paris"}),
                }],
            }
        );
    }

    #[test]
    fn keeps_text_before_a_fenced_tool_call() {
        let reply = parse_reply(
            "I will check.\n```json\n{\"tool_calls\":[{\"name\":\"ls\",\"arguments\":\"{}\"}]}\n```",
            &["ls"],
        );
        assert_eq!(
            reply,
            Reply::ToolCalls {
                text: Some("I will check.".to_string()),
                calls: vec![ToolCall {
                    name: "ls".to_string(),
                    arguments: json!({}),
                }],
            }
        );
    }

    #[test]
    fn unknown_tools_and_plain_answers_stay_text() {
        let unknown = r#"{"tool_calls":[{"name":"rm","arguments":{}}]}"#;
        assert_eq!(
            parse_reply(unknown, &["ls"]),
            Reply::Text(unknown.to_string())
        );
        assert_eq!(
            parse_reply("  Hello.  ", &["ls"]),
            Reply::Text("Hello.".to_string())
        );
        assert_eq!(parse_reply(unknown, &[]), Reply::Text(unknown.to_string()));
    }
}

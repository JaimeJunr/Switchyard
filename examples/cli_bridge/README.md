<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# Switchyard CLI bridge

**English** | [Português (Brasil)](README.pt-BR.md)

> **Experimental integration:** The bridge and its behavior may change without notice.

The CLI bridge lets Switchyard use the Claude Code, Codex, and Grok command-line tools as
models. It uses the login you already have in each CLI. You do not need a provider API key.

## How it works

1. The bridge is a small local server. It speaks the OpenAI Chat Completions API.
2. Switchyard calls the bridge like any other `openai_chat` upstream.
3. For each request, the bridge runs one CLI in non-interactive mode and returns its answer.

The model ID picks the CLI:

| Model ID | CLI | Command |
| --- | --- | --- |
| `claude` or `claude/<model>` | Claude Code | `claude -p` |
| `codex` or `codex/<model>` | Codex CLI | `codex exec` |
| `grok` or `grok/<model>` | Grok CLI (xAI Grok Build) | `grok --prompt-file` |

The part after the slash is passed to the CLI as `--model`, for example `claude/opus` or
`codex/gpt-5.5`. Without it, the CLI uses its default model.

The bridge is its own Cargo workspace. It does not change the root `Cargo.toml`, the root
`Cargo.lock`, or any Switchyard crate.

## Requirements

- Rust and Cargo. The repository's `rust-toolchain.toml` selects the version.
- At least one of these CLIs, installed and logged in:

| CLI | Log in | Check |
| --- | --- | --- |
| [Claude Code](https://code.claude.com/docs) | Run `claude` once and follow the login steps. | `claude -p "Say OK"` |
| [Codex CLI](https://developers.openai.com/codex/cli) | `codex login` | `codex exec --skip-git-repo-check "Say OK"` |
| [Grok CLI](https://docs.x.ai/build/cli/headless-scripting) | `grok login`, or set `XAI_API_KEY` | `grok -p "Say OK"` |

## Quick start

Start the bridge. It listens on `127.0.0.1:4100`:

```bash
cargo run --release --manifest-path examples/cli_bridge/Cargo.toml
```

Test it alone from another terminal:

```bash
curl http://127.0.0.1:4100/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"claude/haiku","messages":[{"role":"user","content":"Say hello"}]}'
```

Start Switchyard with the example routes in [`routes.toml`](routes.toml):

```bash
switchyard-server --config examples/cli_bridge/routes.toml --dry-run
switchyard-server --config examples/cli_bridge/routes.toml --port 4000
```

Send a request through Switchyard:

```bash
curl http://127.0.0.1:4000/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"switchyard","messages":[{"role":"user","content":"Say hello"}]}'
```

The example file has two routes:

| Route `id` | Type | Targets |
| --- | --- | --- |
| `switchyard` | Auto | Codex first. Claude Opus when tool results show trouble. |
| `switchyard-task` | Task (`llm_classifier`) | Claude Haiku judges the task. Grok for easy tasks, Claude Opus for hard ones. |

Edit the targets to use the CLIs and models you have. You can also mix bridge targets with
normal API targets in the same file.

## Use an agent as the client

Switchyard accepts OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages requests.
Point your agent at Switchyard and use a route `id` as the model name.

Start the bridge from a shell where these settings are not active. Otherwise the CLI that
the bridge starts would also call Switchyard, in a loop.

Claude Code:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:4000 \
ANTHROPIC_MODEL=switchyard \
ANTHROPIC_DEFAULT_HAIKU_MODEL=switchyard \
claude
```

Claude Code may warn that it does not know the model `switchyard`. The warning does not stop it.

Codex CLI, with a profile in `~/.codex/config.toml`:

```toml
[profiles.switchyard]
model = "switchyard"
model_provider = "switchyard"

[model_providers.switchyard]
name = "Switchyard"
base_url = "http://127.0.0.1:4000/v1"
wire_api = "responses"
```

```bash
codex --profile switchyard
```

## Limits

- The CLIs return text, not structured tool calls. The bridge lists the request's tools in the
  prompt and asks the model to answer with a small JSON object when it wants a tool. The bridge
  turns that object into normal tool calls. If a model does not follow the format, its answer
  comes back as plain text.
- Images and files in messages are replaced with a short note. The model does not see them.
- A streamed request gets the whole answer at once, after the CLI finishes.
- Each request starts a new CLI process. Expect a few extra seconds per call.
- `max_tokens`, `temperature`, and other sampling settings are ignored.
- Claude Code and Grok report token usage. Codex does not, so the bridge reports zero for it.
- CLI flags change over time. If a CLI rejects a flag, update `arguments` in
  [`src/cli.rs`](src/cli.rs).
- Each CLI's terms of use still apply. Check that your plan allows this use.

## Security

- The bridge listens on `127.0.0.1` by default. Do not expose it to a network. Anyone who can
  reach it can use your CLI logins.
- The CLIs run in a new, empty temporary directory, or in `--workdir`.
- Claude Code runs with its built-in tools and MCP servers turned off. Codex runs in a read-only
  sandbox. Grok runs with its default permissions. The prompt also tells the model not to use
  tools of its own.

## Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `--host` | `127.0.0.1` | Address to listen on. |
| `--port` | `4100` | Port to listen on. |
| `--workdir` | new temporary directory | Directory the CLIs run in. |
| `--timeout-secs` | `600` | Longest time one CLI call may run. The bridge then stops it and returns HTTP 504. |
| `--claude-bin` | `claude` | Claude Code executable. |
| `--codex-bin` | `codex` | Codex CLI executable. |
| `--grok-bin` | `grok` | Grok CLI executable. |

Set `RUST_LOG=debug` for more log detail. Logs never include prompt text.

## Commands the bridge runs

```text
claude -p --output-format json --tools "" --strict-mcp-config --no-session-persistence \
  --system-prompt <short fixed prompt> [--model <model>]            # prompt on stdin
codex exec --skip-git-repo-check --ephemeral --sandbox read-only --color never \
  [--model <model>] -                                               # prompt on stdin
grok --prompt-file <temporary file> --output-format json [--model <model>]
```

A CLI that fails or cannot start returns HTTP 502 with the end of its error output. An unknown
model ID returns HTTP 404.

## Endpoints

| Method and path | Purpose |
| --- | --- |
| `POST /v1/chat/completions` | Chat Completions, buffered or streamed. |
| `GET /v1/models` | Lists `claude`, `codex`, and `grok`. |
| `GET /health` | Returns `{"status":"ok"}`. |

## Development

```bash
cd examples/cli_bridge
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests use fake CLIs written as shell scripts. They need no login and no network.

// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Runs one prompt through the Claude Code, Codex, or Grok CLI in non-interactive mode.
//!
//! Each call starts a new process with the user's existing CLI login. The CLI's own tools are
//! turned off or limited where the CLI allows it, because the bridge only wants model text.

use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

// Replaces Claude Code's coding-agent system prompt, so the model answers as a plain model.
const CLAUDE_SYSTEM_PROMPT: &str = "You are a language model that answers requests sent \
through an API. Follow the instructions in the user message exactly.";

// Error text returned to callers is cut to this many characters.
const ERROR_TAIL_CHARS: usize = 2000;

/// A supported CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cli {
    Claude,
    Codex,
    Grok,
}

impl Cli {
    pub const ALL: [Cli; 3] = [Cli::Claude, Cli::Codex, Cli::Grok];

    /// The model id prefix that selects this CLI.
    pub fn name(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
            Cli::Grok => "grok",
        }
    }

    /// Splits a model id such as `claude/opus` into the CLI and the model passed to `--model`.
    ///
    /// A bare CLI name, such as `codex`, uses that CLI's default model.
    pub fn parse_model(model: &str) -> Option<(Cli, Option<&str>)> {
        let (name, cli_model) = match model.split_once('/') {
            Some((name, cli_model)) => (name, Some(cli_model).filter(|m| !m.is_empty())),
            None => (model, None),
        };
        let cli = Cli::ALL.into_iter().find(|cli| cli.name() == name)?;
        Some((cli, cli_model))
    }
}

/// The executable used for each CLI.
#[derive(Clone, Debug)]
pub struct Programs {
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub grok: PathBuf,
}

impl Default for Programs {
    fn default() -> Self {
        Self {
            claude: "claude".into(),
            codex: "codex".into(),
            grok: "grok".into(),
        }
    }
}

impl Programs {
    pub fn get(&self, cli: Cli) -> &Path {
        match cli {
            Cli::Claude => &self.claude,
            Cli::Codex => &self.codex,
            Cli::Grok => &self.grok,
        }
    }
}

/// Token counts reported by the CLI. Zero when the CLI does not report them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The model's final answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub text: String,
    pub usage: Usage,
}

/// Why a CLI call produced no answer.
#[derive(Debug)]
pub enum CliError {
    Start { program: String, source: io::Error },
    Failed { program: String, message: String },
    Timeout { program: String, after: Duration },
}

impl CliError {
    /// The HTTP status the bridge returns for this error.
    pub fn status(&self) -> u16 {
        match self {
            CliError::Start { .. } | CliError::Failed { .. } => 502,
            CliError::Timeout { .. } => 504,
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CliError::Start { program, source } => write!(
                f,
                "could not start {program}: {source}. Check that it is installed and on PATH."
            ),
            CliError::Failed { program, message } => write!(f, "{program} failed: {message}"),
            CliError::Timeout { program, after } => {
                write!(
                    f,
                    "{program} did not finish within {} seconds",
                    after.as_secs()
                )
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Runs `prompt` through `cli` inside `workdir` and returns the final answer.
///
/// The process is killed if it runs longer than `timeout`.
pub async fn run(
    cli: Cli,
    program: &Path,
    model: Option<&str>,
    prompt: &str,
    workdir: &Path,
    timeout: Duration,
) -> Result<Output, CliError> {
    let program_name = program.display().to_string();
    let start_error = |source| CliError::Start {
        program: program_name.clone(),
        source,
    };

    // Grok reads its prompt from a file. Claude Code and Codex read it from stdin, which has
    // no argument length limit.
    let prompt_file = match cli {
        Cli::Grok => Some(write_prompt_file(prompt).map_err(start_error)?),
        Cli::Claude | Cli::Codex => None,
    };

    let mut command = Command::new(program);
    command
        .args(arguments(
            cli,
            model,
            prompt_file.as_ref().map(|file| file.path()),
        ))
        .current_dir(workdir)
        .stdin(if prompt_file.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(start_error)?;

    let stdin = child.stdin.take();
    let feed = async move {
        if let Some(mut stdin) = stdin {
            // A CLI that exits early closes the pipe. Its exit status reports the real error.
            let _ = stdin.write_all(prompt.as_bytes()).await;
        }
    };
    let finished = tokio::time::timeout(timeout, async {
        tokio::join!(feed, child.wait_with_output()).1
    })
    .await;

    let output = match finished {
        Err(_) => {
            return Err(CliError::Timeout {
                program: program_name,
                after: timeout,
            });
        }
        Ok(result) => result.map_err(start_error)?,
    };

    if !output.status.success() {
        let details = if output.stderr.iter().all(u8::is_ascii_whitespace) {
            &output.stdout
        } else {
            &output.stderr
        };
        return Err(CliError::Failed {
            program: program_name,
            message: format!("{}: {}", output.status, tail(details)),
        });
    }

    parse_output(cli, &output.stdout).map_err(|message| CliError::Failed {
        program: program_name,
        message,
    })
}

fn arguments(cli: Cli, model: Option<&str>, prompt_file: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = match cli {
        Cli::Claude => [
            "-p",
            "--output-format",
            "json",
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--system-prompt",
            CLAUDE_SYSTEM_PROMPT,
        ]
        .map(String::from)
        .into(),
        Cli::Codex => [
            "exec",
            "--skip-git-repo-check",
            "--ephemeral",
            "--sandbox",
            "read-only",
            "--color",
            "never",
        ]
        .map(String::from)
        .into(),
        Cli::Grok => vec![
            "--prompt-file".to_string(),
            prompt_file.map_or_else(String::new, |path| path.display().to_string()),
            "--output-format".to_string(),
            "json".to_string(),
        ],
    };
    if let Some(model) = model {
        args.extend(["--model".to_string(), model.to_string()]);
    }
    if cli == Cli::Codex {
        // Read the prompt from stdin.
        args.push("-".to_string());
    }
    args
}

fn write_prompt_file(prompt: &str) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(prompt.as_bytes())?;
    file.flush()?;
    Ok(file)
}

/// `claude -p --output-format json` prints one result object.
#[derive(Deserialize)]
struct ClaudeResult {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: String,
    #[serde(default)]
    usage: ClaudeUsage,
}

#[derive(Default, Deserialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// `grok --output-format json` prints one object with the final text.
#[derive(Deserialize)]
struct GrokResult {
    text: String,
    #[serde(default)]
    usage: GrokUsage,
}

#[derive(Default, Deserialize)]
struct GrokUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

fn parse_output(cli: Cli, stdout: &[u8]) -> Result<Output, String> {
    let invalid =
        |error: serde_json::Error| format!("unexpected output ({error}): {}", tail(stdout));
    match cli {
        Cli::Claude => {
            let result: ClaudeResult = serde_json::from_slice(stdout).map_err(invalid)?;
            if result.is_error {
                return Err(result.result);
            }
            let usage = result.usage;
            Ok(Output {
                text: result.result,
                usage: Usage {
                    input_tokens: usage.input_tokens
                        + usage.cache_creation_input_tokens
                        + usage.cache_read_input_tokens,
                    output_tokens: usage.output_tokens,
                },
            })
        }
        // `codex exec` prints only the final agent message on stdout. Progress goes to stderr.
        Cli::Codex => Ok(Output {
            text: String::from_utf8_lossy(stdout).trim().to_string(),
            usage: Usage::default(),
        }),
        Cli::Grok => {
            let result: GrokResult = serde_json::from_slice(stdout).map_err(invalid)?;
            Ok(Output {
                text: result.text,
                usage: Usage {
                    input_tokens: result.usage.input_tokens,
                    output_tokens: result.usage.output_tokens,
                },
            })
        }
    }
}

/// The last part of a process's output, for error messages.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let skip = text.chars().count().saturating_sub(ERROR_TAIL_CHARS);
    text.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_ids() {
        assert_eq!(Cli::parse_model("claude"), Some((Cli::Claude, None)));
        assert_eq!(
            Cli::parse_model("claude/opus"),
            Some((Cli::Claude, Some("opus")))
        );
        assert_eq!(
            Cli::parse_model("codex/gpt-5.5"),
            Some((Cli::Codex, Some("gpt-5.5")))
        );
        assert_eq!(Cli::parse_model("grok/"), Some((Cli::Grok, None)));
        assert_eq!(Cli::parse_model("gpt-4o"), None);
        assert_eq!(Cli::parse_model("openai/gpt-4o"), None);
    }

    #[test]
    fn builds_arguments_for_each_cli() {
        let claude = arguments(Cli::Claude, Some("opus"), None);
        assert_eq!(
            claude[..5],
            ["-p", "--output-format", "json", "--tools", ""]
        );
        assert_eq!(claude[claude.len() - 2..], ["--model", "opus"]);

        let codex = arguments(Cli::Codex, None, None);
        assert_eq!(codex.first().map(String::as_str), Some("exec"));
        assert_eq!(codex.last().map(String::as_str), Some("-"));
        assert!(!codex.contains(&"--model".to_string()));

        let grok = arguments(Cli::Grok, Some("grok-4.6"), Some(Path::new("/tmp/p")));
        assert_eq!(
            grok,
            [
                "--prompt-file",
                "/tmp/p",
                "--output-format",
                "json",
                "--model",
                "grok-4.6"
            ]
        );
    }

    #[test]
    fn reads_each_output_format() {
        let claude = parse_output(
            Cli::Claude,
            br#"{"type":"result","is_error":false,"result":"hi","usage":{"input_tokens":2,"cache_read_input_tokens":8,"output_tokens":1}}"#,
        );
        assert_eq!(
            claude,
            Ok(Output {
                text: "hi".to_string(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 1
                },
            })
        );

        let claude_error = parse_output(
            Cli::Claude,
            br#"{"type":"result","is_error":true,"result":"Not logged in"}"#,
        );
        assert_eq!(claude_error, Err("Not logged in".to_string()));

        let codex = parse_output(Cli::Codex, b"hello\n");
        assert_eq!(codex.map(|output| output.text), Ok("hello".to_string()));

        let grok = parse_output(
            Cli::Grok,
            br#"{"text":"hey","stopReason":"end_turn","usage":{"input_tokens":5,"output_tokens":2}}"#,
        );
        assert_eq!(
            grok.map(|output| output.usage),
            Ok(Usage {
                input_tokens: 5,
                output_tokens: 2
            })
        );

        assert!(parse_output(Cli::Grok, b"not json").is_err());
    }

    #[test]
    fn tail_keeps_the_end() {
        let long = "x".repeat(ERROR_TAIL_CHARS) + "end";
        let kept = tail(long.as_bytes());
        assert_eq!(kept.chars().count(), ERROR_TAIL_CHARS);
        assert!(kept.ends_with("end"));
    }
}

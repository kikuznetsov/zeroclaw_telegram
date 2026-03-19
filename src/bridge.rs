use crate::chat_state::{ChatHistoryEntry, RememberedFact};
use crate::prompt::build_bridge_prompt;
use crate::AppState;
use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::process::Stdio;
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration, Instant};

const STATUS_UPDATE_INTERVAL_MS: u64 = 800;

#[derive(Clone, Copy)]
pub(crate) enum PromptMode {
    Bridge,
    Raw,
}

pub(crate) struct ZeroclawRunResult {
    pub(crate) output: String,
    pub(crate) tool_iterations: usize,
    pub(crate) telemetry_observed: bool,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct StreamEvent {
    kind: StreamKind,
    line: String,
}

pub(crate) async fn run_zeroclaw(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    prompt: &str,
    state: &AppState,
    prompt_mode: PromptMode,
) -> Result<ZeroclawRunResult> {
    let (history, facts) = match prompt_mode {
        PromptMode::Bridge => (
            state.chat_store.recent_history(chat_id).await,
            state.chat_store.remembered_facts(chat_id).await,
        ),
        PromptMode::Raw => (Vec::new(), Vec::new()),
    };
    let prepared_prompt = prepare_zeroclaw_prompt(prompt, prompt_mode, &history, &facts);
    let mut cmd = Command::new(&state.zeroclaw_bin);
    cmd.arg("agent")
        .arg("-m")
        .arg(&prepared_prompt)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("ZEROCLAW_OBSERVABILITY_BACKEND", "log")
        .env(
            "RUST_LOG",
            "zeroclaw::agent=info,zeroclaw::tools=info,error",
        )
        .env("CLICOLOR", "0")
        .env("NO_COLOR", "1");

    let mut child = cmd.spawn().context("Failed to spawn zeroclaw")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture zeroclaw stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture zeroclaw stderr"))?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_stream(stdout, StreamKind::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_stream(stderr, StreamKind::Stderr, tx.clone()));
    drop(tx);

    let status = timeout(
        Duration::from_secs(state.zeroclaw_timeout_sec),
        async {
            let mut tool_iterations = 0usize;
            let mut telemetry_observed = false;
            let mut last_status_update =
                Instant::now() - Duration::from_millis(STATUS_UPDATE_INTERVAL_MS);

            let exit_status = loop {
                tokio::select! {
                    Some(event) = rx.recv() => {
                        if is_zeroclaw_telemetry_line(&event) {
                            telemetry_observed = true;
                        }

                        if should_count_tool_iteration(&event) {
                            tool_iterations += 1;
                            if last_status_update.elapsed() >= Duration::from_millis(STATUS_UPDATE_INTERVAL_MS) {
                                if let Err(err) = update_thinking_status(
                                    bot,
                                    chat_id,
                                    status_msg_id,
                                    tool_iterations,
                                )
                                .await
                                {
                                    eprintln!("failed to update thinking status: {err:#}");
                                }
                                last_status_update = Instant::now();
                            }
                        }
                    }
                    wait_result = child.wait() => break wait_result.context("Failed to wait for zeroclaw")?,
                }
            };

            while let Some(event) = rx.recv().await {
                if is_zeroclaw_telemetry_line(&event) {
                    telemetry_observed = true;
                }

                if should_count_tool_iteration(&event) {
                    tool_iterations += 1;
                }
            }

            Ok::<_, anyhow::Error>((exit_status, tool_iterations, telemetry_observed))
        },
    )
    .await;

    let (exit_status, tool_iterations, telemetry_observed) = match status {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(anyhow!("ZeroClaw timed out"));
        }
    };

    let stdout_raw = stdout_task
        .await
        .context("Failed to join zeroclaw stdout task")??;
    let stderr_raw = stderr_task
        .await
        .context("Failed to join zeroclaw stderr task")??;

    let stdout = clean_zeroclaw_output(&stdout_raw);
    let stderr = strip_ansi(&stderr_raw).trim().to_string();

    if !exit_status.success() {
        let mut msg = format!("[zeroclaw exit={}]", exit_status);
        if !stderr.is_empty() {
            msg.push_str("\n\nstderr:\n");
            msg.push_str(&stderr);
        }
        if !stdout.is_empty() {
            msg.push_str("\n\nstdout:\n");
            msg.push_str(&stdout);
        }
        return Ok(ZeroclawRunResult {
            output: msg,
            tool_iterations,
            telemetry_observed,
        });
    }

    if stdout.trim().is_empty() {
        Ok(ZeroclawRunResult {
            output: "(no output)".to_string(),
            tool_iterations,
            telemetry_observed,
        })
    } else {
        Ok(ZeroclawRunResult {
            output: stdout.trim().to_string(),
            tool_iterations,
            telemetry_observed,
        })
    }
}

pub(crate) fn thinking_status_text(_tool_iterations: usize) -> String {
    "🧠 ZeroClaw is thinking...".to_string()
}

pub(crate) fn finished_status_text(_tool_iterations: usize, _telemetry_observed: bool) -> String {
    "✅ ZeroClaw finished.".to_string()
}

async fn read_stream<R>(
    reader: R,
    kind: StreamKind,
    tx: mpsc::UnboundedSender<StreamEvent>,
) -> Result<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    let mut collected = String::new();

    while let Some(line) = lines.next_line().await? {
        collected.push_str(&line);
        collected.push('\n');

        let _ = tx.send(StreamEvent {
            kind,
            line: line.clone(),
        });
    }

    Ok(collected)
}

fn prepare_zeroclaw_prompt(
    prompt: &str,
    prompt_mode: PromptMode,
    history: &[ChatHistoryEntry],
    facts: &[RememberedFact],
) -> String {
    match prompt_mode {
        PromptMode::Raw => prompt.to_string(),
        PromptMode::Bridge => build_bridge_prompt(prompt, history, facts),
    }
}

async fn update_thinking_status(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    tool_iterations: usize,
) -> Result<()> {
    bot.edit_message_text(
        chat_id,
        status_msg_id,
        thinking_status_text(tool_iterations),
    )
    .await?;

    Ok(())
}

fn should_count_tool_iteration(event: &StreamEvent) -> bool {
    let line = strip_ansi(&event.line);

    match event.kind {
        StreamKind::Stdout | StreamKind::Stderr => {
            if !line.contains("zeroclaw::tools::") {
                return false;
            }

            if line.contains("tool_execution{") || line.contains("tool_execute{") {
                return true;
            }

            let has_tool_label = line.contains(r#"tool=""#) || line.contains(" tool=");
            let has_completion_marker = line.contains("status=")
                || line.contains("success=")
                || line.contains("exit_code=")
                || line.contains(" complete")
                || line.contains(" finished");

            has_tool_label && has_completion_marker
        }
    }
}

fn is_zeroclaw_telemetry_line(event: &StreamEvent) -> bool {
    let line = strip_ansi(&event.line);

    match event.kind {
        StreamKind::Stdout | StreamKind::Stderr => line.contains("zeroclaw::"),
    }
}

fn strip_ansi(s: &str) -> String {
    let re = Regex::new(r"\x1B\[[0-9;]*[A-Za-z]").unwrap();
    re.replace_all(s, "").to_string()
}

fn clean_zeroclaw_output(s: &str) -> String {
    let s = strip_ansi(s);

    let mut cleaned = Vec::new();

    for line in s.lines() {
        let line = line.trim_end();

        if line.contains(" zeroclaw::")
            || line.contains(" INFO ")
            || line.contains(" WARN ")
            || line.contains(" ERROR ")
            || line.starts_with("202")
        {
            continue;
        }

        if !line.trim().is_empty() {
            cleaned.push(line);
        }
    }

    cleaned.join("\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_state::{ChatHistoryEntry, ChatHistoryRole, RememberedFact};

    #[test]
    fn detects_tool_iteration_log_lines() {
        let event = StreamEvent {
            kind: StreamKind::Stderr,
            line: r#"2025-01-15T10:23:46.789Z  INFO zeroclaw::tools::shell: tool_execution{tool="shell" command="ls -la"} status=success stdout_bytes=1024"#.to_string(),
        };

        assert!(should_count_tool_iteration(&event));
    }

    #[test]
    fn ignores_non_tool_log_lines() {
        let event = StreamEvent {
            kind: StreamKind::Stderr,
            line: r#"2025-01-15T10:23:45.456Z  INFO zeroclaw::providers::reliable: provider_call{provider="anthropic" model="claude-sonnet-4"} request_sent"#.to_string(),
        };

        assert!(!should_count_tool_iteration(&event));
    }

    #[test]
    fn detects_completion_style_tool_log_lines() {
        let event = StreamEvent {
            kind: StreamKind::Stderr,
            line: r#"2025-01-15T10:23:46.789Z  INFO zeroclaw::tools::shell: complete tool="shell" status=success stdout_bytes=1024"#.to_string(),
        };

        assert!(should_count_tool_iteration(&event));
    }

    #[test]
    fn finished_status_is_plain_without_telemetry() {
        assert_eq!(finished_status_text(0, false), "✅ ZeroClaw finished.");
    }

    #[test]
    fn finished_status_is_plain_with_telemetry() {
        assert_eq!(finished_status_text(2, true), "✅ ZeroClaw finished.");
    }

    #[test]
    fn raw_prompt_mode_skips_bridge_wrapper() {
        assert_eq!(
            prepare_zeroclaw_prompt("hello", PromptMode::Raw, &[], &[]),
            "hello".to_string()
        );
    }

    #[test]
    fn bridge_prompt_includes_recent_chat_history() {
        let history = vec![
            ChatHistoryEntry {
                role: ChatHistoryRole::User,
                text: "previous user message".to_string(),
            },
            ChatHistoryEntry {
                role: ChatHistoryRole::Assistant,
                text: "previous assistant message".to_string(),
            },
        ];

        let facts = vec![RememberedFact {
            key: "favorite color".to_string(),
            value: "red".to_string(),
        }];

        let prompt =
            prepare_zeroclaw_prompt("current request", PromptMode::Bridge, &history, &facts);

        assert!(prompt.contains("Remembered user facts"));
        assert!(prompt.contains("- favorite color: red"));
        assert!(prompt.contains("newer messages appear later and are more relevant"));
        assert!(prompt.contains("prefer the most recent user-provided fact"));
        assert!(prompt.contains("[user]\nprevious user message"));
        assert!(prompt.contains("[assistant]\nprevious assistant message"));
        assert!(prompt.contains("User request:\ncurrent request"));
    }
}

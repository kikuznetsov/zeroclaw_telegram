use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::env;
use std::process::Stdio;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{MessageId, ParseMode};
use teloxide::utils::command::BotCommands;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration, Instant};

const TELEGRAM_CHUNK: usize = 3500;
const DEFAULT_ZEROCLAW_BIN: &str = "/home/konst/zeroclaw";
const DEFAULT_TIMEOUT_SEC: u64 = 240;
const STATUS_UPDATE_INTERVAL_MS: u64 = 800;

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct StreamEvent {
    kind: StreamKind,
    line: String,
}

#[derive(Clone)]
struct AppState {
    allowed_user_id: i64,
    zeroclaw_bin: String,
    zeroclaw_timeout_sec: u64,
    /// Serialize requests because the target host is resource-constrained.
    run_lock: Arc<Mutex<()>>,
}

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Commands:")]
enum Cmd {
    #[command(description = "health check")]
    Ping,
    #[command(description = "show your Telegram user id")]
    Id,
    #[command(description = "send prompt directly to zeroclaw")]
    Raw(String),
    #[command(description = "ask zeroclaw explicitly")]
    Ask(String),
    #[command(description = "run shell command directly")]
    Sh(String),
    #[command(description = "run date")]
    Date,
    #[command(description = "show memory usage")]
    Free,
    #[command(description = "show uptime")]
    Uptime,
    #[command(description = "show zeroclaw service status")]
    Status,
    #[command(description = "show this help")]
    Help,
}

#[tokio::main]
async fn main() -> Result<()> {
    let bot_token = env::var("TG_BOT_TOKEN").context("TG_BOT_TOKEN is not set")?;
    let allowed_user_id: i64 = env::var("TG_ALLOWED_USER_ID")
        .context("TG_ALLOWED_USER_ID is not set")?
        .parse()
        .context("TG_ALLOWED_USER_ID must be integer")?;

    let zeroclaw_bin =
        env::var("ZEROCLAW_BIN").unwrap_or_else(|_| DEFAULT_ZEROCLAW_BIN.to_string());

    let zeroclaw_timeout_sec: u64 = env::var("ZEROCLAW_TIMEOUT_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SEC);

    let bot = Bot::new(bot_token);

    let state = AppState {
        allowed_user_id,
        zeroclaw_bin,
        zeroclaw_timeout_sec,
        run_lock: Arc::new(Mutex::new(())),
    };

    println!("Telegram ZeroClaw bridge started.");

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let state = state.clone();
        async move {
            if let Err(err) = handle_message(bot, msg, state).await {
                eprintln!("handler error: {err:#}");
            }
            respond(())
        }
    })
    .await;

    Ok(())
}

async fn handle_message(bot: Bot, msg: Message, state: AppState) -> Result<()> {
    let user_id = msg
        .from
        .as_ref()
        .map(|u| u.id.0 as i64)
        .ok_or_else(|| anyhow!("message has no from user"))?;

    if user_id != state.allowed_user_id {
        eprintln!(
            "unauthorized access attempt: user_id={}, chat_id={}",
            user_id, msg.chat.id
        );
        bot.send_message(msg.chat.id, "Unauthorized.").await?;
        return Ok(());
    }

    let text = msg.text().unwrap_or("").trim();
    if text.is_empty() {
        bot.send_message(msg.chat.id, "Empty message.").await?;
        return Ok(());
    }

    if let Ok(cmd) = Cmd::parse(text, "bot") {
        match cmd {
            Cmd::Ping => {
                bot.send_message(msg.chat.id, "pong").await?;
                return Ok(());
            }
            Cmd::Id => {
                bot.send_message(msg.chat.id, format!("Your Telegram user ID: {user_id}"))
                    .await?;
                return Ok(());
            }
            Cmd::Help => {
                bot.send_message(msg.chat.id, Cmd::descriptions().to_string())
                    .await?;
                return Ok(());
            }
            Cmd::Raw(prompt) => {
                run_and_reply(&bot, msg.chat.id, &prompt, &state).await?;
                return Ok(());
            }
            Cmd::Ask(prompt) => {
                run_and_reply(&bot, msg.chat.id, &prompt, &state).await?;
                return Ok(());
            }
            Cmd::Sh(command) => {
                run_shell_and_reply(&bot, msg.chat.id, &command, &state).await?;
                return Ok(());
            }
            Cmd::Date => {
                run_shell_and_reply(&bot, msg.chat.id, "date", &state).await?;
                return Ok(());
            }
            Cmd::Free => {
                run_shell_and_reply(&bot, msg.chat.id, "free -h", &state).await?;
                return Ok(());
            }
            Cmd::Uptime => {
                run_shell_and_reply(&bot, msg.chat.id, "uptime", &state).await?;
                return Ok(());
            }
            Cmd::Status => {
                run_shell_and_reply(
                    &bot,
                    msg.chat.id,
                    "systemctl --user status zeroclaw -n 40 --no-pager",
                    &state,
                )
                .await?;
                return Ok(());
            }
        }
    }

    run_and_reply(&bot, msg.chat.id, text, &state).await
}

async fn send_status(bot: &Bot, chat_id: ChatId, text: &str) -> Result<Message> {
    let msg = bot.send_message(chat_id, text).await?;
    Ok(msg)
}

async fn run_and_reply(bot: &Bot, chat_id: ChatId, prompt: &str, state: &AppState) -> Result<()> {
    let _guard = state.run_lock.lock().await;

    let status_msg = send_status(bot, chat_id, &thinking_status_text(0)).await?;

    let output = run_zeroclaw(bot, chat_id, status_msg.id, prompt, state)
        .await
        .unwrap_or_else(|e| format!("❌ Error:\n{e:#}"));

    bot.edit_message_text(chat_id, status_msg.id, "✅ ZeroClaw finished.")
        .await?;

    for chunk in split_text(&output, TELEGRAM_CHUNK) {
        bot.send_message(chat_id, chunk).await?;
    }

    Ok(())
}

fn interactive_command_hint(command: &str) -> Option<&'static str> {
    let trimmed = command.trim();
    let first = trimmed.split_whitespace().next().unwrap_or("");

    match first {
        "top" => Some("Interactive command detected: `top`. Use `top -b -n 1 | head -n 20` instead."),
        "htop" => Some("Interactive command detected: `htop`. Use `ps aux --sort=-%cpu | head -n 15` or `top -b -n 1 | head -n 20` instead."),
        "less" => Some("Interactive command detected: `less`. Use `tail -n 50 <file>` or `head -n 50 <file>` instead."),
        "more" => Some("Interactive command detected: `more`. Use `tail -n 50 <file>` or `head -n 50 <file>` instead."),
        "nano" => Some("Interactive command detected: `nano`. Edit files over SSH or use non-interactive file commands instead."),
        "vim" => Some("Interactive command detected: `vim`. Edit files over SSH or use non-interactive file commands instead."),
        "vi" => Some("Interactive command detected: `vi`. Edit files over SSH or use non-interactive file commands instead."),
        "watch" => Some("Interactive command detected: `watch`. Run the target command directly once instead."),
        _ => {
            if trimmed.contains("tail -f") {
                Some("Interactive/follow mode detected: `tail -f`. Use `tail -n 50 <file>` instead.")
            } else {
                None
            }
        }
    }
}

async fn run_shell_and_reply(
    bot: &Bot,
    chat_id: ChatId,
    command: &str,
    state: &AppState,
) -> Result<()> {
    let _guard = state.run_lock.lock().await;

    if let Some(hint) = interactive_command_hint(command) {
        bot.send_message(chat_id, format!("⚠️ {}", hint)).await?;
        return Ok(());
    }

    let status_msg = bot
        .send_message(
            chat_id,
            format!("⏳ Running shell...\n<code>{}</code>", html_escape(command)),
        )
        .parse_mode(ParseMode::Html)
        .await?;

    let output = run_shell(command, state)
        .await
        .unwrap_or_else(|e| format!("❌ Error:\n{e:#}"));

    bot.edit_message_text(chat_id, status_msg.id, "✅ Shell finished.")
        .await?;

    for chunk in split_text(&output, TELEGRAM_CHUNK) {
        bot.send_message(chat_id, format!("<pre>{}</pre>", html_escape(&chunk)))
            .parse_mode(ParseMode::Html)
            .await?;
    }

    Ok(())
}

async fn run_zeroclaw(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    prompt: &str,
    state: &AppState,
) -> Result<String> {
    let mut cmd = Command::new(&state.zeroclaw_bin);
    cmd.arg("agent")
        .arg("-m")
        .arg(prompt)
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
            let mut last_status_update = Instant::now() - Duration::from_millis(STATUS_UPDATE_INTERVAL_MS);

            let exit_status = loop {
                tokio::select! {
                    Some(event) = rx.recv() => {
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

            while rx.recv().await.is_some() {}

            Ok::<_, anyhow::Error>(exit_status)
        },
    )
    .await;

    let output = match status {
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

    if !output.success() {
        let mut msg = format!("[zeroclaw exit={}]", output);
        if !stderr.is_empty() {
            msg.push_str("\n\nstderr:\n");
            msg.push_str(&stderr);
        }
        if !stdout.is_empty() {
            msg.push_str("\n\nstdout:\n");
            msg.push_str(&stdout);
        }
        return Ok(msg);
    }

    if stdout.trim().is_empty() {
        Ok("(no output)".to_string())
    } else {
        Ok(stdout.trim().to_string())
    }
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

fn thinking_status_text(tool_iterations: usize) -> String {
    format!(
        "🧠 ZeroClaw is thinking...\n🔧 Tool iterations: {}",
        tool_iterations
    )
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
            line.contains("zeroclaw::tools::")
                && (line.contains("tool_execution{") || line.contains("tool_execute{"))
                && (line.contains("status=")
                    || line.contains("success=")
                    || line.contains("exit_code="))
        }
    }
}

async fn run_shell(command: &str, state: &AppState) -> Result<String> {
    let mut cmd = Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CLICOLOR", "0")
        .env("NO_COLOR", "1");

    let fut = cmd.output();

    let output = timeout(Duration::from_secs(state.zeroclaw_timeout_sec), fut)
        .await
        .context("Shell command timed out")?
        .context("Failed to spawn shell")?;

    let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout))
        .trim()
        .to_string();
    let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr))
        .trim()
        .to_string();

    let mut combined = String::new();

    if !stdout.is_empty() {
        combined.push_str(&stdout);
    }

    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str("stderr:\n");
        combined.push_str(&stderr);
    }

    if !output.status.success() {
        if !combined.is_empty() {
            combined.push_str("\n\n");
        }
        combined.push_str(&format!("[exit: {}]", output.status));
    }

    if combined.trim().is_empty() {
        Ok("(no output)".to_string())
    } else {
        Ok(combined.trim().to_string())
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

fn split_text(text: &str, chunk_size: usize) -> Vec<String> {
    if text.is_empty() {
        return vec!["(empty reply)".to_string()];
    }

    let mut out = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if current.len() + ch.len_utf8() > chunk_size {
            out.push(current);
            current = String::new();
        }
        current.push(ch);
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

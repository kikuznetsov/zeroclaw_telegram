mod attachments;
mod bridge;
mod chat_state;
mod prompt;
mod shell;

use anyhow::{anyhow, Context, Result};
use attachments::{
    download_message_attachments, extract_existing_file_paths_from_text,
    extract_outgoing_attachments, maybe_add_delivery_warning, resolve_existing_local_path,
    send_local_attachment, try_handle_direct_send_request, OutgoingAttachmentKind,
};
use bridge::{
    finished_status_text, run_zeroclaw, thinking_status_text, PromptMode, ZeroclawRunResult,
};
use chat_state::{ChatHistoryRole, ChatStore};
use prompt::{
    build_assistant_history_message, build_attachment_history_message, build_attachment_prompt,
};
use shell::{
    build_cat_command, build_ls_command, interactive_command_hint, is_exact_command, run_shell,
};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use teloxide::utils::command::BotCommands;
use tokio::sync::Mutex;

const TELEGRAM_CHUNK: usize = 3500;
const DEFAULT_ZEROCLAW_BIN: &str = "/home/konst/zeroclaw";
const DEFAULT_ZEROCLAW_WORKSPACE_SUFFIX: &str = ".zeroclaw/workspace";
const DEFAULT_CHAT_STORE_FILENAME: &str = ".chat_store.json";
const DEFAULT_TIMEOUT_SEC: u64 = 240;

#[derive(Clone)]
struct AppState {
    allowed_user_id: i64,
    zeroclaw_bin: String,
    zeroclaw_workspace_dir: Option<PathBuf>,
    zeroclaw_timeout_sec: u64,
    /// Serialize requests because the target host is resource-constrained.
    run_lock: Arc<Mutex<()>>,
    chat_store: ChatStore,
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
    #[command(description = "list files with ls -la [path]")]
    Ls(String),
    #[command(description = "print a local file with cat <path>")]
    Cat(String),
    #[command(description = "run date")]
    Date,
    #[command(description = "show memory usage")]
    Free,
    #[command(description = "show uptime")]
    Uptime,
    #[command(description = "show zeroclaw service status")]
    Status,
    #[command(description = "send a local file into Telegram")]
    Sendfile(String),
    #[command(description = "send a local image into Telegram as photo")]
    Sendphoto(String),
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
    let zeroclaw_workspace_dir = env::var_os("ZEROCLAW_WORKSPACE_DIR")
        .map(PathBuf::from)
        .or_else(default_zeroclaw_workspace_dir);
    let chat_store_path = env::var_os("CHAT_STORE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(default_chat_store_path);

    let zeroclaw_timeout_sec: u64 = env::var("ZEROCLAW_TIMEOUT_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SEC);

    let bot = Bot::new(bot_token);

    let state = AppState {
        allowed_user_id,
        zeroclaw_bin,
        zeroclaw_workspace_dir,
        zeroclaw_timeout_sec,
        run_lock: Arc::new(Mutex::new(())),
        chat_store: ChatStore::open(chat_store_path).await?,
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

    if !text.is_empty() {
        if is_exact_command(text, "ls") {
            run_shell_and_reply(&bot, msg.chat.id, &build_ls_command("."), &state).await?;
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
                    run_and_reply(&bot, msg.chat.id, &prompt, None, &state, PromptMode::Raw)
                        .await?;
                    return Ok(());
                }
                Cmd::Ask(prompt) => {
                    let history_message = prompt.clone();
                    run_and_reply(
                        &bot,
                        msg.chat.id,
                        &prompt,
                        Some(history_message),
                        &state,
                        PromptMode::Bridge,
                    )
                    .await?;
                    return Ok(());
                }
                Cmd::Sh(command) => {
                    run_shell_and_reply(&bot, msg.chat.id, &command, &state).await?;
                    return Ok(());
                }
                Cmd::Ls(path) => {
                    let path = path.trim();
                    let command = build_ls_command(if path.is_empty() { "." } else { path });
                    run_shell_and_reply(&bot, msg.chat.id, &command, &state).await?;
                    return Ok(());
                }
                Cmd::Cat(path) => {
                    let command = build_cat_command(&path);
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
                Cmd::Sendfile(path) => {
                    let sent_path = send_local_attachment(
                        &bot,
                        msg.chat.id,
                        OutgoingAttachmentKind::Document,
                        &path,
                        None,
                        &state,
                    )
                    .await?;
                    state
                        .chat_store
                        .set_recent_paths(msg.chat.id, vec![sent_path])
                        .await?;
                    return Ok(());
                }
                Cmd::Sendphoto(path) => {
                    let sent_path = send_local_attachment(
                        &bot,
                        msg.chat.id,
                        OutgoingAttachmentKind::Photo,
                        &path,
                        None,
                        &state,
                    )
                    .await?;
                    state
                        .chat_store
                        .set_recent_paths(msg.chat.id, vec![sent_path])
                        .await?;
                    return Ok(());
                }
            }
        }

        if try_handle_direct_send_request(&bot, msg.chat.id, text, &state).await? {
            return Ok(());
        }

        return run_and_reply(
            &bot,
            msg.chat.id,
            text,
            Some(text.to_string()),
            &state,
            PromptMode::Bridge,
        )
        .await;
    }

    let attachments = download_message_attachments(&bot, &msg).await?;
    if !attachments.is_empty() {
        let caption = msg.caption().unwrap_or("").trim();
        let prompt = build_attachment_prompt(msg.caption().unwrap_or("").trim(), &attachments);
        let history_message = Some(build_attachment_history_message(caption, attachments.len()));
        return run_and_reply(
            &bot,
            msg.chat.id,
            &prompt,
            history_message,
            &state,
            PromptMode::Bridge,
        )
        .await;
    }

    bot.send_message(msg.chat.id, "Empty message.").await?;
    Ok(())
}

async fn send_status(bot: &Bot, chat_id: ChatId, text: &str) -> Result<Message> {
    let msg = bot.send_message(chat_id, text).await?;
    Ok(msg)
}

async fn run_and_reply(
    bot: &Bot,
    chat_id: ChatId,
    prompt: &str,
    history_user_message: Option<String>,
    state: &AppState,
    prompt_mode: PromptMode,
) -> Result<()> {
    let _guard = state.run_lock.lock().await;

    let status_msg = send_status(bot, chat_id, &thinking_status_text(0)).await?;

    let result = match run_zeroclaw(bot, chat_id, status_msg.id, prompt, state, prompt_mode).await {
        Ok(result) => result,
        Err(err) => ZeroclawRunResult {
            output: format!("❌ Error:\n{err:#}"),
            tool_iterations: 0,
            telemetry_observed: false,
        },
    };

    bot.edit_message_text(
        chat_id,
        status_msg.id,
        finished_status_text(result.tool_iterations, result.telemetry_observed),
    )
    .await?;

    let (text_output, outgoing_attachments) = extract_outgoing_attachments(&result.output);
    let text_output = maybe_add_delivery_warning(text_output, outgoing_attachments.len());
    let mut successful_attachment_count = 0usize;

    if !text_output.trim().is_empty() {
        for chunk in split_text(&text_output, TELEGRAM_CHUNK) {
            if chunk.trim().is_empty() {
                continue;
            }
            bot.send_message(chat_id, chunk).await?;
        }
    }

    let mut referenced_paths = extract_existing_file_paths_from_text(&result.output, state);
    for attachment in &outgoing_attachments {
        if let Ok(path) = resolve_existing_local_path(&attachment.path.display().to_string(), state)
        {
            referenced_paths.push(path);
        }
    }
    if !referenced_paths.is_empty() {
        state
            .chat_store
            .set_recent_paths(chat_id, referenced_paths)
            .await?;
    }

    for attachment in outgoing_attachments {
        if let Err(err) = send_local_attachment(
            bot,
            chat_id,
            attachment.kind,
            &attachment.path.display().to_string(),
            attachment.caption.as_deref(),
            state,
        )
        .await
        {
            bot.send_message(
                chat_id,
                format!(
                    "❌ Failed to send Telegram attachment `{}`:\n{err:#}",
                    attachment.path.display()
                ),
            )
            .await?;
        } else {
            successful_attachment_count += 1;
        }
    }

    if matches!(prompt_mode, PromptMode::Bridge) {
        if let Some(user_message) = history_user_message {
            state
                .chat_store
                .push_message(chat_id, ChatHistoryRole::User, &user_message)
                .await?;
        }

        if let Some(assistant_message) =
            build_assistant_history_message(&text_output, successful_attachment_count)
        {
            state
                .chat_store
                .push_message(chat_id, ChatHistoryRole::Assistant, &assistant_message)
                .await?;
        }
    }

    Ok(())
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

    let output = run_shell(command, state.zeroclaw_timeout_sec)
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

fn default_zeroclaw_workspace_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_ZEROCLAW_WORKSPACE_SUFFIX))
}

fn default_chat_store_path() -> PathBuf {
    env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(DEFAULT_CHAT_STORE_FILENAME)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

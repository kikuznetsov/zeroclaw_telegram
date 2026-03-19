use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use teloxide::net::Download;
use teloxide::payloads::{SendDocumentSetters, SendPhotoSetters};
use teloxide::prelude::*;
use teloxide::types::{InputFile, MessageId, ParseMode};
use teloxide::utils::command::BotCommands;
use tokio::fs::{self, File as TokioFile};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{timeout, Duration, Instant};

const TELEGRAM_CHUNK: usize = 3500;
const DEFAULT_ZEROCLAW_BIN: &str = "/home/konst/zeroclaw";
const DEFAULT_ZEROCLAW_WORKSPACE_SUFFIX: &str = ".zeroclaw/workspace";
const DEFAULT_TIMEOUT_SEC: u64 = 240;
const STATUS_UPDATE_INTERVAL_MS: u64 = 800;
const TELEGRAM_UPLOADS_DIR: &str = ".telegram_uploads";
const TELEGRAM_CAPTION_LIMIT: usize = 1024;
const MAX_DIRECT_SEND_MATCHES: usize = 5;

const BRIDGE_PROTOCOL_INSTRUCTIONS: &str = r#"Telegram bridge capability note:
- If you want to send a real file into the Telegram chat, output exactly one of these marker lines on its own line:
  [[telegram_document:relative/or/absolute/path|optional caption]]
  [[telegram_photo:relative/or/absolute/path|optional caption]]
- Only use these markers when the file already exists on the local machine.
- Do not say a file was sent unless you emitted one of those marker lines.
"#;

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct StreamEvent {
    kind: StreamKind,
    line: String,
}

struct ZeroclawRunResult {
    output: String,
    tool_iterations: usize,
    telemetry_observed: bool,
}

struct DownloadedAttachment {
    kind: &'static str,
    path: PathBuf,
    original_name: Option<String>,
    mime_type: Option<String>,
}

#[derive(Clone, Copy)]
enum PromptMode {
    Bridge,
    Raw,
}

#[derive(Clone, Copy)]
enum OutgoingAttachmentKind {
    Document,
    Photo,
}

struct OutgoingAttachment {
    kind: OutgoingAttachmentKind,
    path: PathBuf,
    caption: Option<String>,
}

#[derive(Default)]
struct ChatContext {
    last_referenced_paths: Vec<PathBuf>,
}

#[derive(Clone)]
struct AppState {
    allowed_user_id: i64,
    zeroclaw_bin: String,
    zeroclaw_workspace_dir: Option<PathBuf>,
    zeroclaw_timeout_sec: u64,
    /// Serialize requests because the target host is resource-constrained.
    run_lock: Arc<Mutex<()>>,
    chat_contexts: Arc<Mutex<HashMap<i64, ChatContext>>>,
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
        chat_contexts: Arc::new(Mutex::new(HashMap::new())),
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
                    run_and_reply(&bot, msg.chat.id, &prompt, &state, PromptMode::Raw).await?;
                    return Ok(());
                }
                Cmd::Ask(prompt) => {
                    run_and_reply(&bot, msg.chat.id, &prompt, &state, PromptMode::Bridge).await?;
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
                    set_last_referenced_paths(&state, msg.chat.id, vec![sent_path]).await;
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
                    set_last_referenced_paths(&state, msg.chat.id, vec![sent_path]).await;
                    return Ok(());
                }
            }
        }

        if try_handle_direct_send_request(&bot, msg.chat.id, text, &state).await? {
            return Ok(());
        }

        return run_and_reply(&bot, msg.chat.id, text, &state, PromptMode::Bridge).await;
    }

    let attachments = download_message_attachments(&bot, &msg).await?;
    if !attachments.is_empty() {
        let prompt = build_attachment_prompt(msg.caption().unwrap_or("").trim(), &attachments);
        return run_and_reply(&bot, msg.chat.id, &prompt, &state, PromptMode::Bridge).await;
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
        set_last_referenced_paths(state, chat_id, referenced_paths).await;
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
        }
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

async fn download_message_attachments(
    bot: &Bot,
    msg: &Message,
) -> Result<Vec<DownloadedAttachment>> {
    let mut attachments = Vec::new();
    let mut upload_dir = None;

    if let Some(document) = msg.document() {
        let dir = ensure_message_upload_dir(msg, &mut upload_dir).await?;
        let telegram_file = bot
            .get_file(document.file.id.clone())
            .await
            .context("Failed to fetch Telegram document metadata")?;
        let original_name = document.file_name.clone();
        let file_name = original_name
            .as_deref()
            .map(sanitize_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| fallback_attachment_name("document", &telegram_file.path));
        let destination = dir.join(file_name);

        download_telegram_file(bot, &telegram_file.path, &destination).await?;

        attachments.push(DownloadedAttachment {
            kind: "document",
            path: destination,
            original_name,
            mime_type: document.mime_type.as_ref().map(|mime| mime.to_string()),
        });
    }

    if let Some(photos) = msg.photo() {
        if let Some(photo) = photos.iter().max_by_key(|photo| {
            (
                u64::from(photo.width) * u64::from(photo.height),
                u64::from(photo.file.size),
            )
        }) {
            let dir = ensure_message_upload_dir(msg, &mut upload_dir).await?;
            let telegram_file = bot
                .get_file(photo.file.id.clone())
                .await
                .context("Failed to fetch Telegram photo metadata")?;
            let file_name = fallback_attachment_name("photo", &telegram_file.path);
            let destination = dir.join(file_name);

            download_telegram_file(bot, &telegram_file.path, &destination).await?;

            attachments.push(DownloadedAttachment {
                kind: "image",
                path: destination,
                original_name: None,
                mime_type: Some(detect_mime_from_path(&telegram_file.path)),
            });
        }
    }

    Ok(attachments)
}

async fn ensure_message_upload_dir(
    msg: &Message,
    upload_dir: &mut Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(existing) = upload_dir {
        return Ok(existing.clone());
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before UNIX_EPOCH")?
        .as_millis();
    let dir = env::current_dir()
        .context("Failed to resolve current working directory")?
        .join(TELEGRAM_UPLOADS_DIR)
        .join(format!("chat_{}", msg.chat.id.0))
        .join(format!("msg_{}_{}", msg.id.0, now_ms));

    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("Failed to create upload directory {}", dir.display()))?;

    *upload_dir = Some(dir.clone());
    Ok(dir)
}

async fn download_telegram_file(bot: &Bot, telegram_path: &str, destination: &Path) -> Result<()> {
    let mut dst = TokioFile::create(destination)
        .await
        .with_context(|| format!("Failed to create {}", destination.display()))?;

    bot.download_file(telegram_path, &mut dst)
        .await
        .with_context(|| {
            format!(
                "Failed to download Telegram file to {}",
                destination.display()
            )
        })?;

    Ok(())
}

fn build_attachment_prompt(caption: &str, attachments: &[DownloadedAttachment]) -> String {
    let mut prompt = String::new();

    prompt.push_str("The user sent a Telegram message with local attachments.\n");

    if caption.is_empty() {
        prompt.push_str("The message has no caption.\n");
    } else {
        prompt.push_str("User caption:\n");
        prompt.push_str(caption);
        prompt.push_str("\n");
    }

    prompt.push_str("\nDownloaded attachments:\n");

    for attachment in attachments {
        prompt.push_str("- ");
        prompt.push_str(attachment.kind);
        prompt.push_str(": ");
        prompt.push_str(&attachment.path.display().to_string());

        if let Some(original_name) = &attachment.original_name {
            prompt.push_str(" (original name: ");
            prompt.push_str(original_name);
            prompt.push(')');
        }

        if let Some(mime_type) = &attachment.mime_type {
            prompt.push_str(" [mime: ");
            prompt.push_str(mime_type);
            prompt.push(']');
        }

        prompt.push('\n');
    }

    prompt
        .push_str("\nUse these local file paths directly if you need to inspect the attachments. ");
    prompt.push_str(
        "If the user did not include a caption, inspect the attachments and respond helpfully.",
    );

    prompt
}

fn build_zeroclaw_prompt(prompt: &str, prompt_mode: PromptMode) -> String {
    match prompt_mode {
        PromptMode::Raw => prompt.to_string(),
        PromptMode::Bridge => format!(
            "{BRIDGE_PROTOCOL_INSTRUCTIONS}\nPrefer absolute file paths when you reference or send local files.\n\nUser request:\n{prompt}"
        ),
    }
}

fn fallback_attachment_name(prefix: &str, telegram_path: &str) -> String {
    let extension = Path::new(telegram_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("bin");

    format!("{}.{}", prefix, extension)
}

fn sanitize_filename(name: &str) -> String {
    let base_name = Path::new(name)
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or(name);

    let sanitized: String = base_name
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => ch,
            _ => '_',
        })
        .collect();

    let sanitized = sanitized.trim_matches('_').trim_matches('.');
    if sanitized.is_empty() {
        "attachment".to_string()
    } else {
        sanitized.to_string()
    }
}

fn detect_mime_from_path(path: &str) -> String {
    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg".to_string(),
        Some("png") => "image/png".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("gif") => "image/gif".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

async fn try_handle_direct_send_request(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    state: &AppState,
) -> Result<bool> {
    if let Some(requested_path) = extract_requested_attachment_path(text) {
        let requested_path = requested_path.trim();
        if requested_path.is_empty() {
            return Ok(false);
        }

        let preferred_kind = infer_attachment_kind(text, requested_path);

        if let Ok(resolved) = resolve_existing_local_path(requested_path, state) {
            let sent_path = send_local_attachment(
                bot,
                chat_id,
                preferred_kind,
                &resolved.display().to_string(),
                None,
                state,
            )
            .await?;
            set_last_referenced_paths(state, chat_id, vec![sent_path]).await;
            return Ok(true);
        }

        let matches = find_workspace_files_by_name(requested_path, state)
            .with_context(|| format!("Failed to search workspace for `{requested_path}`"))?;

        return match matches.as_slice() {
            [] => Ok(false),
            [path] => {
                let sent_path = send_local_attachment(
                    bot,
                    chat_id,
                    preferred_kind,
                    &path.display().to_string(),
                    None,
                    state,
                )
                .await?;
                set_last_referenced_paths(state, chat_id, vec![sent_path]).await;
                Ok(true)
            }
            _ => {
                set_last_referenced_paths(state, chat_id, matches.clone()).await;

                let mut msg = format!(
                    "Multiple files match `{}`. Use `/sendfile <path>` or `/sendphoto <path>` with an exact path.\n",
                    requested_path
                );

                for path in matches.iter().take(MAX_DIRECT_SEND_MATCHES) {
                    msg.push_str("- ");
                    msg.push_str(&path.display().to_string());
                    msg.push('\n');
                }

                bot.send_message(chat_id, msg.trim_end().to_string())
                    .await?;
                Ok(true)
            }
        };
    }

    if !is_recent_file_followup_request(text) {
        return Ok(false);
    }

    let referenced_paths = get_last_referenced_paths(state, chat_id).await;
    match referenced_paths.as_slice() {
        [] => Ok(false),
        [path] => {
            let preferred_kind = infer_attachment_kind(text, &path.display().to_string());
            let sent_path = send_local_attachment(
                bot,
                chat_id,
                preferred_kind,
                &path.display().to_string(),
                None,
                state,
            )
            .await?;
            set_last_referenced_paths(state, chat_id, vec![sent_path]).await;
            Ok(true)
        }
        _ => {
            let mut msg = String::from(
                "I found multiple recent file paths. Use `/sendfile <path>` or `/sendphoto <path>` with an exact path.\n",
            );
            for path in referenced_paths.iter().take(MAX_DIRECT_SEND_MATCHES) {
                msg.push_str("- ");
                msg.push_str(&path.display().to_string());
                msg.push('\n');
            }
            bot.send_message(chat_id, msg.trim_end().to_string())
                .await?;
            Ok(true)
        }
    }
}

fn extract_requested_attachment_path(text: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?i)\b(?:send|upload)\b[^\n]*?((?:/|~\/|\.\.?/)?(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.[A-Za-z0-9]+)"#,
    )
    .unwrap();

    re.captures(text)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_string())
}

fn is_recent_file_followup_request(text: &str) -> bool {
    let text_lower = text.to_ascii_lowercase();

    (text_lower.contains("send") || text_lower.contains("upload"))
        && (text_lower.contains("located file")
            || text_lower.contains("found file")
            || text_lower.contains("file you found")
            || text_lower.contains("located document")
            || text_lower.contains("found document")
            || text_lower.contains("located image")
            || text_lower.contains("found image"))
}

fn infer_attachment_kind(text: &str, path: &str) -> OutgoingAttachmentKind {
    let text_lower = text.to_ascii_lowercase();
    let ext = Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    let is_image_ext = matches!(
        ext.as_deref(),
        Some("png") | Some("jpg") | Some("jpeg") | Some("webp") | Some("gif") | Some("bmp")
    );

    if text_lower.contains("/sendphoto")
        || text_lower.contains("photo")
        || text_lower.contains("image")
        || is_image_ext
    {
        OutgoingAttachmentKind::Photo
    } else {
        OutgoingAttachmentKind::Document
    }
}

fn find_workspace_files_by_name(name_or_path: &str, state: &AppState) -> Result<Vec<PathBuf>> {
    let target_name = Path::new(name_or_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(name_or_path);

    let mut matches = Vec::new();
    for root in workspace_search_roots(state)? {
        if !root.is_dir() {
            continue;
        }
        walk_workspace_for_name(&root, target_name, &mut matches)?;
    }
    dedupe_paths(&mut matches);
    Ok(matches)
}

fn walk_workspace_for_name(
    dir: &Path,
    target_name: &str,
    matches: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("Failed to read directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            if should_skip_workspace_dir(&path) {
                continue;
            }
            walk_workspace_for_name(&path, target_name, matches)?;
        } else if file_type.is_file()
            && path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.eq_ignore_ascii_case(target_name))
                .unwrap_or(false)
        {
            matches.push(path);
        }
    }

    Ok(())
}

fn should_skip_workspace_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git") | Some("target") | Some(".telegram_uploads")
    )
}

async fn run_zeroclaw(
    bot: &Bot,
    chat_id: ChatId,
    status_msg_id: MessageId,
    prompt: &str,
    state: &AppState,
    prompt_mode: PromptMode,
) -> Result<ZeroclawRunResult> {
    let prepared_prompt = build_zeroclaw_prompt(prompt, prompt_mode);
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
            let mut last_status_update = Instant::now() - Duration::from_millis(STATUS_UPDATE_INTERVAL_MS);

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

fn thinking_status_text(_tool_iterations: usize) -> String {
    "🧠 ZeroClaw is thinking...".to_string()
}

fn finished_status_text(_tool_iterations: usize, _telemetry_observed: bool) -> String {
    "✅ ZeroClaw finished.".to_string()
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

fn extract_outgoing_attachments(output: &str) -> (String, Vec<OutgoingAttachment>) {
    let re = Regex::new(r"(?m)^\[\[telegram_(document|photo):([^\]\|]+?)(?:\|([^\]]*))?\]\]\s*$")
        .unwrap();

    let mut attachments = Vec::new();

    for capture in re.captures_iter(output) {
        let kind = match capture.get(1).map(|m| m.as_str()) {
            Some("document") => OutgoingAttachmentKind::Document,
            Some("photo") => OutgoingAttachmentKind::Photo,
            _ => continue,
        };

        let path = capture
            .get(2)
            .map(|m| PathBuf::from(m.as_str().trim()))
            .unwrap_or_default();
        let caption = capture
            .get(3)
            .map(|m| m.as_str().trim().to_string())
            .filter(|caption| !caption.is_empty());

        attachments.push(OutgoingAttachment {
            kind,
            path,
            caption,
        });
    }

    let cleaned = re.replace_all(output, "").trim().to_string();
    (cleaned, attachments)
}

fn maybe_add_delivery_warning(text_output: String, attachment_count: usize) -> String {
    if attachment_count > 0 || !appears_to_claim_delivery(&text_output) {
        return text_output;
    }

    let mut text_output = text_output;
    if !text_output.trim().is_empty() {
        text_output.push_str("\n\n");
    }
    text_output
        .push_str("Bridge note: ZeroClaw did not emit a Telegram upload marker, so no file or photo was actually sent.");
    text_output
}

fn appears_to_claim_delivery(text: &str) -> bool {
    let text_lower = text.to_ascii_lowercase();

    let mentions_attachment =
        text_lower.contains("file") || text_lower.contains("photo") || text_lower.contains("image");
    let mentions_delivery = text_lower.contains("sent")
        || text_lower.contains("uploaded")
        || text_lower.contains("you should now see")
        || text_lower.contains("let me send it");

    mentions_attachment && mentions_delivery
}

async fn send_local_attachment(
    bot: &Bot,
    chat_id: ChatId,
    kind: OutgoingAttachmentKind,
    path_str: &str,
    caption: Option<&str>,
    state: &AppState,
) -> Result<PathBuf> {
    let path = resolve_existing_local_path(path_str, state)?;
    let metadata = fs::metadata(&path)
        .await
        .with_context(|| format!("Failed to stat {}", path.display()))?;

    if !metadata.is_file() {
        return Err(anyhow!("Path is not a regular file: {}", path.display()));
    }

    let input = InputFile::file(path.clone());
    let caption = caption.map(truncate_telegram_caption);

    match kind {
        OutgoingAttachmentKind::Document => {
            let request = bot.send_document(chat_id, input);
            if let Some(caption) = caption {
                request.caption(caption).await?;
            } else {
                request.await?;
            }
        }
        OutgoingAttachmentKind::Photo => {
            let request = bot.send_photo(chat_id, input);
            if let Some(caption) = caption {
                request.caption(caption).await?;
            } else {
                request.await?;
            }
        }
    }

    Ok(path)
}

fn resolve_existing_local_path(path_str: &str, state: &AppState) -> Result<PathBuf> {
    let candidates = candidate_local_paths(path_str, state)?;

    for candidate in &candidates {
        if std::fs::metadata(candidate)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            return Ok(candidate.clone());
        }
    }

    Err(anyhow!(
        "Failed to locate local file `{}`. Checked: {}",
        normalize_path_str(path_str)?,
        candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn candidate_local_paths(path_str: &str, state: &AppState) -> Result<Vec<PathBuf>> {
    let trimmed = normalize_path_str(path_str)?;
    let path = Path::new(trimmed);
    let mut candidates = Vec::new();

    if let Some(home_relative) = trimmed.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            candidates.push(PathBuf::from(home).join(home_relative));
        } else {
            candidates.push(PathBuf::from(trimmed));
        }
    } else if path.is_absolute() {
        candidates.push(path.to_path_buf());
    } else {
        let cwd = env::current_dir().context("Failed to resolve current working directory")?;
        candidates.push(cwd.join(path));

        if let Some(workspace_dir) = &state.zeroclaw_workspace_dir {
            let workspace_candidate = workspace_dir.join(path);
            if workspace_candidate != candidates[0] {
                candidates.push(workspace_candidate);
            }
        }
    }

    dedupe_paths(&mut candidates);
    Ok(candidates)
}

fn normalize_path_str(path_str: &str) -> Result<&str> {
    let trimmed = path_str
        .trim()
        .trim_matches(|ch| matches!(ch, '`' | '"' | '\''));
    if trimmed.is_empty() {
        return Err(anyhow!("Attachment path is empty"));
    }
    Ok(trimmed)
}

fn workspace_search_roots(state: &AppState) -> Result<Vec<PathBuf>> {
    let mut roots =
        vec![env::current_dir().context("Failed to resolve current working directory")?];
    if let Some(workspace_dir) = &state.zeroclaw_workspace_dir {
        roots.push(workspace_dir.clone());
    }
    dedupe_paths(&mut roots);
    Ok(roots)
}

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut deduped = Vec::with_capacity(paths.len());
    for path in paths.drain(..) {
        if deduped.iter().any(|existing| existing == &path) {
            continue;
        }
        deduped.push(path);
    }
    *paths = deduped;
}

fn extract_existing_file_paths_from_text(text: &str, state: &AppState) -> Vec<PathBuf> {
    let re = Regex::new(
        r#"(?x)
        (?P<path>
            /[A-Za-z0-9._/\-]+
            |
            ~/[A-Za-z0-9._/\-]+
            |
            \.\.?/[A-Za-z0-9._/\-]+
        )
    "#,
    )
    .unwrap();

    let mut paths = Vec::new();
    for captures in re.captures_iter(text) {
        let Some(path) = captures.name("path") else {
            continue;
        };
        if let Ok(resolved) = resolve_existing_local_path(path.as_str(), state) {
            paths.push(resolved);
        }
    }
    dedupe_paths(&mut paths);
    paths
}

fn default_zeroclaw_workspace_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_ZEROCLAW_WORKSPACE_SUFFIX))
}

async fn set_last_referenced_paths(state: &AppState, chat_id: ChatId, mut paths: Vec<PathBuf>) {
    dedupe_paths(&mut paths);
    paths.truncate(MAX_DIRECT_SEND_MATCHES);

    let mut contexts = state.chat_contexts.lock().await;
    let context = contexts.entry(chat_id.0).or_default();
    context.last_referenced_paths = paths;
}

async fn get_last_referenced_paths(state: &AppState, chat_id: ChatId) -> Vec<PathBuf> {
    let contexts = state.chat_contexts.lock().await;
    contexts
        .get(&chat_id.0)
        .map(|context| context.last_referenced_paths.clone())
        .unwrap_or_default()
}

fn truncate_telegram_caption(caption: &str) -> String {
    caption.chars().take(TELEGRAM_CAPTION_LIMIT).collect()
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

    #[test]
    fn detects_completion_style_tool_log_lines() {
        let event = StreamEvent {
            kind: StreamKind::Stderr,
            line: r#"2025-01-15T10:23:46.789Z  INFO zeroclaw::tools::shell: complete tool="shell" status=success stdout_bytes=1024"#.to_string(),
        };

        assert!(should_count_tool_iteration(&event));
    }

    #[test]
    fn sanitizes_attachment_filenames() {
        assert_eq!(
            sanitize_filename("../../weird name?.png"),
            "weird_name_.png"
        );
    }

    #[test]
    fn builds_prompt_with_attachment_paths() {
        let attachments = vec![DownloadedAttachment {
            kind: "image",
            path: PathBuf::from("/tmp/example.png"),
            original_name: None,
            mime_type: Some("image/png".to_string()),
        }];

        let prompt = build_attachment_prompt("describe it", &attachments);

        assert!(prompt.contains("describe it"));
        assert!(prompt.contains("/tmp/example.png"));
        assert!(prompt.contains("image/png"));
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
    fn extracts_outgoing_attachment_markers() {
        let output = "Here is the image.\n[[telegram_photo:images/mountains.png|mountains]]";

        let (text, attachments) = extract_outgoing_attachments(output);

        assert_eq!(text, "Here is the image.");
        assert_eq!(attachments.len(), 1);
        assert!(matches!(attachments[0].kind, OutgoingAttachmentKind::Photo));
        assert_eq!(attachments[0].path, PathBuf::from("images/mountains.png"));
        assert_eq!(attachments[0].caption.as_deref(), Some("mountains"));
    }

    #[test]
    fn raw_prompt_mode_skips_bridge_wrapper() {
        assert_eq!(
            build_zeroclaw_prompt("hello", PromptMode::Raw),
            "hello".to_string()
        );
    }

    #[test]
    fn extracts_requested_attachment_path_from_natural_language() {
        assert_eq!(
            extract_requested_attachment_path("send me mountains.png by using /sendphoto"),
            Some("mountains.png".to_string())
        );
    }

    #[test]
    fn extracts_absolute_requested_attachment_path_from_natural_language() {
        assert_eq!(
            extract_requested_attachment_path(
                "send /home/konst/.zeroclaw/workspace/meeting_elena_16h00.ics"
            ),
            Some("/home/konst/.zeroclaw/workspace/meeting_elena_16h00.ics".to_string())
        );
    }

    #[test]
    fn detects_recent_file_followup_requests() {
        assert!(is_recent_file_followup_request("send me located file"));
        assert!(is_recent_file_followup_request("upload the file you found"));
        assert!(!is_recent_file_followup_request(
            "send me a summary instead"
        ));
    }

    #[test]
    fn resolves_relative_attachment_path_from_workspace_dir() {
        let test_root = env::temp_dir().join(format!(
            "tg-zeroclaw-bridge-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let workspace_dir = test_root.join(".zeroclaw").join("workspace");
        let file_path = workspace_dir.join("meeting_elena_16h00.ics");

        std::fs::create_dir_all(&workspace_dir).unwrap();
        std::fs::write(&file_path, "BEGIN:VCALENDAR\nEND:VCALENDAR\n").unwrap();

        let state = AppState {
            allowed_user_id: 0,
            zeroclaw_bin: DEFAULT_ZEROCLAW_BIN.to_string(),
            zeroclaw_workspace_dir: Some(workspace_dir),
            zeroclaw_timeout_sec: DEFAULT_TIMEOUT_SEC,
            run_lock: Arc::new(Mutex::new(())),
            chat_contexts: Arc::new(Mutex::new(HashMap::new())),
        };

        let resolved = resolve_existing_local_path("meeting_elena_16h00.ics", &state).unwrap();
        assert_eq!(resolved, file_path);

        std::fs::remove_dir_all(&test_root).unwrap();
    }

    #[test]
    fn adds_warning_for_ungrounded_delivery_claim() {
        let warned =
            maybe_add_delivery_warning("Perfect! I sent you the image in Telegram.".to_string(), 0);

        assert!(warned.contains("Bridge note: ZeroClaw did not emit a Telegram upload marker"));
    }
}

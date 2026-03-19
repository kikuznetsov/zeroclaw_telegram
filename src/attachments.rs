use crate::AppState;
use anyhow::{anyhow, Context, Result};
use regex::Regex;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use teloxide::net::Download;
use teloxide::payloads::{SendDocumentSetters, SendPhotoSetters};
use teloxide::prelude::*;
use teloxide::types::InputFile;
use tokio::fs::{self, File as TokioFile};

const TELEGRAM_UPLOADS_DIR: &str = ".telegram_uploads";
const TELEGRAM_CAPTION_LIMIT: usize = 1024;
const MAX_DIRECT_SEND_MATCHES: usize = 5;

pub(crate) struct DownloadedAttachment {
    pub(crate) kind: &'static str,
    pub(crate) path: PathBuf,
    pub(crate) original_name: Option<String>,
    pub(crate) mime_type: Option<String>,
}

#[derive(Clone, Copy)]
pub(crate) enum OutgoingAttachmentKind {
    Document,
    Photo,
}

pub(crate) struct OutgoingAttachment {
    pub(crate) kind: OutgoingAttachmentKind,
    pub(crate) path: PathBuf,
    pub(crate) caption: Option<String>,
}

pub(crate) async fn download_message_attachments(
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

pub(crate) async fn try_handle_direct_send_request(
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
            state
                .chat_store
                .set_recent_paths(chat_id, vec![sent_path])
                .await?;
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
                state
                    .chat_store
                    .set_recent_paths(chat_id, vec![sent_path])
                    .await?;
                Ok(true)
            }
            _ => {
                state
                    .chat_store
                    .set_recent_paths(chat_id, matches.clone())
                    .await?;

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

    let referenced_paths = state.chat_store.recent_paths(chat_id).await;
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
            state
                .chat_store
                .set_recent_paths(chat_id, vec![sent_path])
                .await?;
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

pub(crate) fn extract_requested_attachment_path(text: &str) -> Option<String> {
    let re = Regex::new(
        r#"(?i)\b(?:send|upload)\b[^\n]*?((?:/|~\/|\.\.?/)?(?:[A-Za-z0-9_.-]+/)*[A-Za-z0-9_.-]+\.[A-Za-z0-9]+)"#,
    )
    .unwrap();

    re.captures(text)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_string())
}

pub(crate) fn is_recent_file_followup_request(text: &str) -> bool {
    let text_lower = text.to_ascii_lowercase();
    let text_lower = text_lower.trim();

    (text_lower.contains("send") || text_lower.contains("upload"))
        && (text_lower.contains("located file")
            || text_lower.contains("found file")
            || text_lower.contains("file you found")
            || text_lower.contains("located document")
            || text_lower.contains("found document")
            || text_lower.contains("located image")
            || text_lower.contains("found image")
            || text_lower.contains("send it")
            || text_lower.contains("upload it")
            || text_lower.contains("send this")
            || text_lower.contains("upload this")
            || text_lower.contains("send that")
            || text_lower.contains("upload that"))
}

pub(crate) fn extract_outgoing_attachments(output: &str) -> (String, Vec<OutgoingAttachment>) {
    let re = Regex::new(
        r"(?m)^\s*(?:\*\*|__|`)?\s*\[\[telegram_(document|photo):([^\]\|]+?)(?:\|([^\]]*))?\]\]\s*(?:\*\*|__|`)?\s*$",
    )
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

pub(crate) fn maybe_add_delivery_warning(text_output: String, attachment_count: usize) -> String {
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

pub(crate) async fn send_local_attachment(
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

pub(crate) fn resolve_existing_local_path(path_str: &str, state: &AppState) -> Result<PathBuf> {
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

pub(crate) fn extract_existing_file_paths_from_text(text: &str, state: &AppState) -> Vec<PathBuf> {
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

pub(crate) fn sanitize_filename(name: &str) -> String {
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

fn fallback_attachment_name(prefix: &str, telegram_path: &str) -> String {
    let extension = Path::new(telegram_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("bin");

    format!("{}.{}", prefix, extension)
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

fn truncate_telegram_caption(caption: &str) -> String {
    caption.chars().take(TELEGRAM_CAPTION_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_state::ChatStore;
    use crate::{DEFAULT_TIMEOUT_SEC, DEFAULT_ZEROCLAW_BIN};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn test_app_state(workspace_dir: Option<PathBuf>) -> AppState {
        let store_path = std::env::temp_dir().join(format!(
            "tg-zeroclaw-bridge-attachments-test-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AppState {
            allowed_user_id: 0,
            zeroclaw_bin: DEFAULT_ZEROCLAW_BIN.to_string(),
            zeroclaw_workspace_dir: workspace_dir,
            zeroclaw_timeout_sec: DEFAULT_TIMEOUT_SEC,
            run_lock: Arc::new(Mutex::new(())),
            chat_store: ChatStore::empty_for_tests(store_path),
        }
    }

    #[test]
    fn sanitizes_attachment_filenames() {
        assert_eq!(
            sanitize_filename("../../weird name?.png"),
            "weird_name_.png"
        );
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
    fn extracts_markdown_wrapped_outgoing_attachment_markers() {
        let output =
            "Here is the file.\n**[[telegram_document:/home/konst/.zeroclaw/workspace/volcano_modelling.ics|Calendar file]]**";

        let (text, attachments) = extract_outgoing_attachments(output);

        assert_eq!(text, "Here is the file.");
        assert_eq!(attachments.len(), 1);
        assert!(matches!(
            attachments[0].kind,
            OutgoingAttachmentKind::Document
        ));
        assert_eq!(
            attachments[0].path,
            PathBuf::from("/home/konst/.zeroclaw/workspace/volcano_modelling.ics")
        );
        assert_eq!(attachments[0].caption.as_deref(), Some("Calendar file"));
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
        assert!(is_recent_file_followup_request("send it to me"));
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

        let state = test_app_state(Some(workspace_dir));

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

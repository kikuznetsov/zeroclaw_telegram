use crate::attachments::DownloadedAttachment;
use crate::chat_state::{ChatHistoryEntry, RememberedFact};

const BRIDGE_PROTOCOL_INSTRUCTIONS: &str = r#"Telegram bridge capability note:
- If you want to send a real file into the Telegram chat, output exactly one of these marker lines on its own line:
  [[telegram_document:relative/or/absolute/path|optional caption]]
  [[telegram_photo:relative/or/absolute/path|optional caption]]
- Do not wrap marker lines in Markdown, bold markers, code fences, or any other formatting.
- Only use these markers when the file already exists on the local machine.
- Do not say a file was sent unless you emitted one of those marker lines.
"#;

pub(crate) fn build_attachment_prompt(
    caption: &str,
    attachments: &[DownloadedAttachment],
) -> String {
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

pub(crate) fn build_bridge_prompt(
    prompt: &str,
    history: &[ChatHistoryEntry],
    facts: &[RememberedFact],
) -> String {
    let mut prepared_prompt = String::new();
    prepared_prompt.push_str(BRIDGE_PROTOCOL_INSTRUCTIONS);
    prepared_prompt
        .push_str("\nPrefer absolute file paths when you reference or send local files.");

    if !facts.is_empty() {
        prepared_prompt.push_str("\n\nRemembered user facts:\n");
        prepared_prompt.push_str(
            "These are durable user-provided facts captured from earlier messages. Prefer newer facts over older transcript guesses.\n",
        );
        for fact in facts {
            prepared_prompt.push_str("- ");
            prepared_prompt.push_str(&fact.key);
            prepared_prompt.push_str(": ");
            prepared_prompt.push_str(&fact.value);
            prepared_prompt.push('\n');
        }
    }

    if !history.is_empty() {
        prepared_prompt.push_str(
            "\n\nRecent chat history for context (oldest to newest; newer messages appear later and are more relevant):\n",
        );
        prepared_prompt.push_str(
            "If the history contains conflicting facts, prefer the most recent user-provided fact.\n",
        );
        for entry in history {
            prepared_prompt.push('[');
            prepared_prompt.push_str(entry.role.label());
            prepared_prompt.push_str("]\n");
            prepared_prompt.push_str(&entry.text);
            prepared_prompt.push('\n');
        }
    }

    prepared_prompt.push_str("\nUser request:\n");
    prepared_prompt.push_str(prompt);
    prepared_prompt
}

pub(crate) fn build_attachment_history_message(caption: &str, attachment_count: usize) -> String {
    let mut message = format!(
        "[user sent {} attachment{}]",
        attachment_count,
        if attachment_count == 1 { "" } else { "s" }
    );

    let caption = caption.trim();
    if !caption.is_empty() {
        message.push_str("\nCaption: ");
        message.push_str(caption);
    }

    message
}

pub(crate) fn build_assistant_history_message(
    text_output: &str,
    attachment_count: usize,
) -> Option<String> {
    let mut parts = Vec::new();
    let text_output = text_output.trim();

    if !text_output.is_empty() {
        parts.push(text_output.to_string());
    }

    if attachment_count > 0 {
        parts.push(format!(
            "[assistant sent {} attachment{}]",
            attachment_count,
            if attachment_count == 1 { "" } else { "s" }
        ));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
}

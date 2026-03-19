use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use teloxide::types::ChatId;
use tokio::fs;
use tokio::sync::Mutex;

const MAX_DIRECT_SEND_MATCHES: usize = 5;
const MAX_CHAT_HISTORY_MESSAGES: usize = 10;
const MAX_USER_HISTORY_CHARS: usize = 500;
const MAX_ASSISTANT_HISTORY_CHARS: usize = 300;
const MAX_FACTS_PER_CHAT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ChatHistoryRole {
    User,
    Assistant,
}

impl ChatHistoryRole {
    pub(crate) fn label(self) -> &'static str {
        match self {
            ChatHistoryRole::User => "user",
            ChatHistoryRole::Assistant => "assistant",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatHistoryEntry {
    pub(crate) role: ChatHistoryRole,
    pub(crate) text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RememberedFact {
    pub(crate) key: String,
    pub(crate) value: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct ChatContext {
    last_referenced_paths: Vec<PathBuf>,
    recent_messages: Vec<ChatHistoryEntry>,
    remembered_facts: Vec<RememberedFact>,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedChatStore {
    chats: HashMap<i64, ChatContext>,
}

#[derive(Clone)]
pub(crate) struct ChatStore {
    path: Arc<PathBuf>,
    contexts: Arc<Mutex<HashMap<i64, ChatContext>>>,
}

impl ChatStore {
    pub(crate) async fn open(path: PathBuf) -> Result<Self> {
        let contexts = load_contexts(&path).await?;
        Ok(Self {
            path: Arc::new(path),
            contexts: Arc::new(Mutex::new(contexts)),
        })
    }

    #[cfg(test)]
    pub(crate) fn empty_for_tests(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
            contexts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) async fn set_recent_paths(
        &self,
        chat_id: ChatId,
        mut paths: Vec<PathBuf>,
    ) -> Result<()> {
        dedupe_paths(&mut paths);
        paths.truncate(MAX_DIRECT_SEND_MATCHES);

        let mut contexts = self.contexts.lock().await;
        let context = contexts.entry(chat_id.0).or_default();
        context.last_referenced_paths = paths;
        persist_locked(&self.path, &contexts).await
    }

    pub(crate) async fn recent_paths(&self, chat_id: ChatId) -> Vec<PathBuf> {
        let contexts = self.contexts.lock().await;
        contexts
            .get(&chat_id.0)
            .map(|context| context.last_referenced_paths.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn push_message(
        &self,
        chat_id: ChatId,
        role: ChatHistoryRole,
        text: &str,
    ) -> Result<()> {
        let Some(text) = normalize_history_text(text, role) else {
            return Ok(());
        };

        let mut contexts = self.contexts.lock().await;
        let context = contexts.entry(chat_id.0).or_default();
        context.recent_messages.push(ChatHistoryEntry {
            role,
            text: text.clone(),
        });

        if context.recent_messages.len() > MAX_CHAT_HISTORY_MESSAGES {
            let overflow = context.recent_messages.len() - MAX_CHAT_HISTORY_MESSAGES;
            context.recent_messages.drain(0..overflow);
        }

        if matches!(role, ChatHistoryRole::User) {
            if let Some(fact) = extract_fact_from_user_message(&text) {
                upsert_fact(&mut context.remembered_facts, fact);
            }
        }

        persist_locked(&self.path, &contexts).await
    }

    pub(crate) async fn recent_history(&self, chat_id: ChatId) -> Vec<ChatHistoryEntry> {
        let contexts = self.contexts.lock().await;
        contexts
            .get(&chat_id.0)
            .map(|context| context.recent_messages.clone())
            .unwrap_or_default()
    }

    pub(crate) async fn remembered_facts(&self, chat_id: ChatId) -> Vec<RememberedFact> {
        let contexts = self.contexts.lock().await;
        contexts
            .get(&chat_id.0)
            .map(|context| context.remembered_facts.clone())
            .unwrap_or_default()
    }
}

pub(crate) fn normalize_history_text(text: &str, role: ChatHistoryRole) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let single_line = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    let limit = match role {
        ChatHistoryRole::User => MAX_USER_HISTORY_CHARS,
        ChatHistoryRole::Assistant => MAX_ASSISTANT_HISTORY_CHARS,
    };

    let mut normalized: String = single_line.chars().take(limit).collect();
    if single_line.chars().count() > limit {
        normalized.push_str("... [truncated]");
    }

    Some(normalized)
}

fn extract_fact_from_user_message(text: &str) -> Option<RememberedFact> {
    let re =
        Regex::new(r"(?i)^\s*my\s+([a-z][a-z0-9 _-]{0,40}?)\s+is\s+(.+?)\s*[\.\!\?]?\s*$").unwrap();
    let captures = re.captures(text)?;
    let key = captures.get(1)?.as_str();
    let value = captures.get(2)?.as_str();

    let key = normalize_fact_key(key)?;
    let value = normalize_fact_value(value)?;

    Some(RememberedFact { key, value })
}

fn normalize_fact_key(key: &str) -> Option<String> {
    let key = key
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase();

    if key.is_empty() {
        None
    } else {
        Some(key)
    }
}

fn normalize_fact_value(value: &str) -> Option<String> {
    let value = value
        .trim()
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn upsert_fact(facts: &mut Vec<RememberedFact>, fact: RememberedFact) {
    if let Some(existing) = facts.iter_mut().find(|existing| existing.key == fact.key) {
        existing.value = fact.value;
        return;
    }

    facts.push(fact);
    if facts.len() > MAX_FACTS_PER_CHAT {
        let overflow = facts.len() - MAX_FACTS_PER_CHAT;
        facts.drain(0..overflow);
    }
}

async fn load_contexts(path: &Path) -> Result<HashMap<i64, ChatContext>> {
    match fs::read_to_string(path).await {
        Ok(contents) => {
            let persisted: PersistedChatStore = serde_json::from_str(&contents)
                .with_context(|| format!("Failed to parse chat store {}", path.display()))?;
            Ok(persisted.chats)
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(err) => {
            Err(err).with_context(|| format!("Failed to read chat store {}", path.display()))
        }
    }
}

async fn persist_locked(path: &Path, contexts: &HashMap<i64, ChatContext>) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create chat store directory {}", parent.display()))?;

    let payload = PersistedChatStore {
        chats: contexts.clone(),
    };
    let serialized =
        serde_json::to_string_pretty(&payload).context("Failed to serialize chat store")?;
    let temp_path = path.with_extension("tmp");

    fs::write(&temp_path, serialized).await.with_context(|| {
        format!(
            "Failed to write temporary chat store {}",
            temp_path.display()
        )
    })?;
    fs::rename(&temp_path, path)
        .await
        .with_context(|| format!("Failed to persist chat store {}", path.display()))?;

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_store_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tg-zeroclaw-bridge-{name}-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn extracts_simple_user_fact() {
        let fact = extract_fact_from_user_message("my favorite color is red").unwrap();

        assert_eq!(fact.key, "favorite color");
        assert_eq!(fact.value, "red");
    }

    #[tokio::test]
    async fn persists_history_and_facts_to_disk() {
        let path = temp_store_path("chat-store");
        let store = ChatStore::empty_for_tests(path.clone());
        let chat_id = ChatId(7);

        store
            .push_message(chat_id, ChatHistoryRole::User, "my favorite color is blue")
            .await
            .unwrap();
        store
            .set_recent_paths(chat_id, vec![PathBuf::from("/tmp/file.txt")])
            .await
            .unwrap();

        let reopened = ChatStore::open(path.clone()).await.unwrap();
        let history = reopened.recent_history(chat_id).await;
        let facts = reopened.remembered_facts(chat_id).await;
        let paths = reopened.recent_paths(chat_id).await;

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].text, "my favorite color is blue");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "favorite color");
        assert_eq!(facts[0].value, "blue");
        assert_eq!(paths, vec![PathBuf::from("/tmp/file.txt")]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn normalizes_assistant_history_more_aggressively() {
        let normalized = normalize_history_text(
            "Line one with details.\nLine two with a lot more text.",
            ChatHistoryRole::Assistant,
        )
        .unwrap();

        assert_eq!(
            normalized,
            "Line one with details. Line two with a lot more text."
        );
    }

    #[tokio::test]
    async fn chat_history_keeps_last_ten_messages() {
        let path = temp_store_path("chat-history");
        let store = ChatStore::empty_for_tests(path.clone());
        let chat_id = ChatId(42);

        for idx in 0..12 {
            store
                .push_message(chat_id, ChatHistoryRole::User, &format!("message {idx}"))
                .await
                .unwrap();
        }

        let history = store.recent_history(chat_id).await;
        assert_eq!(history.len(), 10);
        assert_eq!(history[0].text, "message 2");
        assert_eq!(history[9].text, "message 11");

        let _ = std::fs::remove_file(path);
    }
}

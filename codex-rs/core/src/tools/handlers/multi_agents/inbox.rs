use super::locks::lock_file_exclusive;
use super::now_unix_seconds;
use super::team_dir;
use crate::function_tool::FunctionCallError;
use codex_protocol::ThreadId;
use codex_protocol::user_input::UserInput;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

const TEAM_INBOX_DIR: &str = "inbox";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TeamInboxEntry {
    pub(super) id: String,
    pub(super) created_at: i64,
    pub(super) team_id: String,
    pub(super) from_thread_id: String,
    pub(super) from_name: Option<String>,
    pub(super) to_thread_id: String,
    pub(super) input_items: Vec<UserInput>,
    pub(super) prompt: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TeamInboxEntryState {
    Queued,
    LiveDelivered,
    Acked,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct TeamInboxState {
    entries: HashMap<String, TeamInboxEntryState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TeamInboxAckToken {
    pub(super) team_id: String,
    pub(super) thread_id: String,
    pub(super) entry_ids: Vec<String>,
}

pub(super) fn inbox_dir(codex_home: &Path, team_id: &str) -> PathBuf {
    team_dir(codex_home, team_id).join(TEAM_INBOX_DIR)
}

fn inbox_path(codex_home: &Path, team_id: &str, thread_id: ThreadId) -> PathBuf {
    inbox_dir(codex_home, team_id).join(format!("{thread_id}.jsonl"))
}

fn inbox_lock_path(codex_home: &Path, team_id: &str, thread_id: ThreadId) -> PathBuf {
    inbox_dir(codex_home, team_id).join(format!("{thread_id}.lock"))
}

fn inbox_state_path(codex_home: &Path, team_id: &str, thread_id: ThreadId) -> PathBuf {
    inbox_dir(codex_home, team_id).join(format!("{thread_id}.state.json"))
}

fn inbox_error(
    action: &str,
    team_id: &str,
    thread_id: ThreadId,
    err: impl std::fmt::Display,
) -> FunctionCallError {
    FunctionCallError::RespondToModel(format!(
        "failed to {action} inbox for team `{team_id}` thread `{thread_id}`: {err}"
    ))
}

async fn read_state(
    codex_home: &Path,
    team_id: &str,
    thread_id: ThreadId,
) -> Result<TeamInboxState, FunctionCallError> {
    let state_path = inbox_state_path(codex_home, team_id, thread_id);
    let raw = match tokio::fs::read_to_string(&state_path).await {
        Ok(raw) => raw,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(TeamInboxState::default()),
        Err(err) => return Err(inbox_error("read", team_id, thread_id, err)),
    };

    serde_json::from_str(&raw).map_err(|err| inbox_error("parse", team_id, thread_id, err))
}

async fn write_state(
    codex_home: &Path,
    team_id: &str,
    thread_id: ThreadId,
    state: &TeamInboxState,
) -> Result<(), FunctionCallError> {
    let state_path = inbox_state_path(codex_home, team_id, thread_id);
    super::write_json_atomic(&state_path, state)
        .await
        .map_err(|err| inbox_error("write", team_id, thread_id, err))
}

async fn existing_entry_ids(
    codex_home: &Path,
    team_id: &str,
    thread_id: ThreadId,
) -> Result<HashSet<String>, FunctionCallError> {
    let inbox_file = match tokio::fs::File::open(inbox_path(codex_home, team_id, thread_id)).await {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(err) => return Err(inbox_error("open", team_id, thread_id, err)),
    };

    let mut reader = BufReader::new(inbox_file).lines();
    let mut ids = HashSet::new();
    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|err| inbox_error("read", team_id, thread_id, err))?
    {
        let entry: TeamInboxEntry = serde_json::from_str(&line)
            .map_err(|err| inbox_error("parse", team_id, thread_id, err))?;
        ids.insert(entry.id);
    }
    Ok(ids)
}

pub(super) async fn append_inbox_entry(
    codex_home: &Path,
    team_id: &str,
    receiver_thread_id: ThreadId,
    sender_thread_id: ThreadId,
    sender_name: Option<&str>,
    input_items: &[UserInput],
    prompt: &str,
) -> Result<String, FunctionCallError> {
    let inbox_dir = inbox_dir(codex_home, team_id);
    tokio::fs::create_dir_all(&inbox_dir)
        .await
        .map_err(|err| inbox_error("create", team_id, receiver_thread_id, err))?;

    let lock_path = inbox_lock_path(codex_home, team_id, receiver_thread_id);
    let _lock = lock_file_exclusive(&lock_path)
        .await
        .map_err(|err| inbox_error("lock", team_id, receiver_thread_id, err))?;

    let entry = TeamInboxEntry {
        id: ThreadId::new().to_string(),
        created_at: now_unix_seconds(),
        team_id: team_id.to_string(),
        from_thread_id: sender_thread_id.to_string(),
        from_name: sender_name.map(std::string::ToString::to_string),
        to_thread_id: receiver_thread_id.to_string(),
        input_items: input_items.to_vec(),
        prompt: prompt.to_string(),
    };

    let mut serialized = serde_json::to_string(&entry)
        .map_err(|err| inbox_error("serialize", team_id, receiver_thread_id, err))?;
    serialized.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_path(codex_home, team_id, receiver_thread_id))
        .await
        .map_err(|err| inbox_error("open", team_id, receiver_thread_id, err))?;
    file.write_all(serialized.as_bytes())
        .await
        .map_err(|err| inbox_error("append", team_id, receiver_thread_id, err))?;

    let mut state = read_state(codex_home, team_id, receiver_thread_id).await?;
    state
        .entries
        .insert(entry.id.clone(), TeamInboxEntryState::Queued);
    write_state(codex_home, team_id, receiver_thread_id, &state).await?;

    Ok(entry.id)
}

pub(super) async fn mark_inbox_entry_live_delivered(
    codex_home: &Path,
    team_id: &str,
    receiver_thread_id: ThreadId,
    entry_id: &str,
) -> Result<(), FunctionCallError> {
    let inbox_dir = inbox_dir(codex_home, team_id);
    tokio::fs::create_dir_all(&inbox_dir)
        .await
        .map_err(|err| inbox_error("create", team_id, receiver_thread_id, err))?;

    let lock_path = inbox_lock_path(codex_home, team_id, receiver_thread_id);
    let _lock = lock_file_exclusive(&lock_path)
        .await
        .map_err(|err| inbox_error("lock", team_id, receiver_thread_id, err))?;

    let mut state = read_state(codex_home, team_id, receiver_thread_id).await?;
    if !matches!(
        state.entries.get(entry_id),
        Some(TeamInboxEntryState::Acked)
    ) {
        state
            .entries
            .insert(entry_id.to_string(), TeamInboxEntryState::LiveDelivered);
    }
    write_state(codex_home, team_id, receiver_thread_id, &state).await
}

pub(super) async fn pop_inbox_entries(
    codex_home: &Path,
    team_id: &str,
    receiver_thread_id: ThreadId,
    limit: usize,
) -> Result<(Vec<TeamInboxEntry>, Option<TeamInboxAckToken>), FunctionCallError> {
    let inbox_dir = inbox_dir(codex_home, team_id);
    tokio::fs::create_dir_all(&inbox_dir)
        .await
        .map_err(|err| inbox_error("create", team_id, receiver_thread_id, err))?;

    let lock_path = inbox_lock_path(codex_home, team_id, receiver_thread_id);
    let _lock = lock_file_exclusive(&lock_path)
        .await
        .map_err(|err| inbox_error("lock", team_id, receiver_thread_id, err))?;

    let state = read_state(codex_home, team_id, receiver_thread_id).await?;
    let inbox_file =
        match tokio::fs::File::open(inbox_path(codex_home, team_id, receiver_thread_id)).await {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok((Vec::new(), None)),
            Err(err) => return Err(inbox_error("open", team_id, receiver_thread_id, err)),
        };

    let mut reader = BufReader::new(inbox_file).lines();
    let mut entries = Vec::new();
    let mut entry_ids = Vec::new();
    while let Some(line) = reader
        .next_line()
        .await
        .map_err(|err| inbox_error("read", team_id, receiver_thread_id, err))?
    {
        let entry: TeamInboxEntry = serde_json::from_str(&line)
            .map_err(|err| inbox_error("parse", team_id, receiver_thread_id, err))?;
        let entry_state = state
            .entries
            .get(&entry.id)
            .copied()
            .unwrap_or(TeamInboxEntryState::Queued);
        if entry_state != TeamInboxEntryState::Queued {
            continue;
        }

        entry_ids.push(entry.id.clone());
        entries.push(entry);
        if entries.len() >= limit {
            break;
        }
    }

    if entries.is_empty() {
        return Ok((entries, None));
    }

    Ok((
        entries,
        Some(TeamInboxAckToken {
            team_id: team_id.to_string(),
            thread_id: receiver_thread_id.to_string(),
            entry_ids,
        }),
    ))
}

pub(super) async fn ack_inbox(
    codex_home: &Path,
    token: &TeamInboxAckToken,
) -> Result<(), FunctionCallError> {
    let receiver_thread_id = super::agent_id(&token.thread_id)?;
    let team_id = token.team_id.as_str();

    let inbox_dir = inbox_dir(codex_home, team_id);
    tokio::fs::create_dir_all(&inbox_dir)
        .await
        .map_err(|err| inbox_error("create", team_id, receiver_thread_id, err))?;

    let lock_path = inbox_lock_path(codex_home, team_id, receiver_thread_id);
    let _lock = lock_file_exclusive(&lock_path)
        .await
        .map_err(|err| inbox_error("lock", team_id, receiver_thread_id, err))?;

    if token.entry_ids.is_empty() {
        return Ok(());
    }

    let existing_ids = existing_entry_ids(codex_home, team_id, receiver_thread_id).await?;
    let mut state = read_state(codex_home, team_id, receiver_thread_id).await?;
    for entry_id in &token.entry_ids {
        if !existing_ids.contains(entry_id) {
            return Err(FunctionCallError::RespondToModel(
                "ack_token references missing inbox entry".to_string(),
            ));
        }
        state
            .entries
            .insert(entry_id.clone(), TeamInboxEntryState::Acked);
    }
    write_state(codex_home, team_id, receiver_thread_id, &state).await
}

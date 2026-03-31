use std::path::PathBuf;

use crate::history_cell::HistoryCell;
use crate::markdown::append_markdown;
use crate::render::line_utils::prefix_lines;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadUnsubscribeParams;
use codex_app_server_protocol::ThreadUnsubscribeResponse;
use codex_app_server_protocol::Turn;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_core::config::types::ApprovalsReviewer;
use codex_protocol::ThreadId;
use codex_protocol::models::MessagePhase;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::user_input::UserInput as CoreUserInput;
use color_eyre::eyre::Result;
use color_eyre::eyre::WrapErr;
use ratatui::style::Stylize;
use ratatui::text::Line;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep;
use uuid::Uuid;

const BTW_INSTRUCTIONS: &str = "Answer the user's next message as a quick side question in the context of the current thread. Do not use tools, run commands, or modify files. Give a concise direct answer in a single assistant message.";
const BTW_POLL_INTERVAL: Duration = Duration::from_millis(250);
const BTW_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BtwRequest {
    pub(crate) source_thread_id: ThreadId,
    pub(crate) question: String,
    pub(crate) items: Vec<CoreUserInput>,
    pub(crate) cwd: PathBuf,
    pub(crate) approvals_reviewer: ApprovalsReviewer,
}

#[derive(Debug)]
pub(crate) struct BtwHistoryCell {
    question: String,
    outcome: BtwHistoryOutcome,
    cwd: PathBuf,
}

#[derive(Debug)]
enum BtwHistoryOutcome {
    Pending,
    Completed(String),
    Failed(String),
}

impl BtwHistoryCell {
    pub(crate) fn pending(question: String, cwd: PathBuf) -> Self {
        Self {
            question,
            outcome: BtwHistoryOutcome::Pending,
            cwd,
        }
    }

    pub(crate) fn completed(question: String, answer: String, cwd: PathBuf) -> Self {
        Self {
            question,
            outcome: BtwHistoryOutcome::Completed(answer),
            cwd,
        }
    }

    pub(crate) fn failed(question: String, error: String, cwd: PathBuf) -> Self {
        Self {
            question,
            outcome: BtwHistoryOutcome::Failed(error),
            cwd,
        }
    }
}

impl HistoryCell for BtwHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines =
            vec![vec!["/btw".magenta(), " ".into(), self.question.clone().into()].into()];

        match &self.outcome {
            BtwHistoryOutcome::Pending => {
                lines.push(vec!["↳ ".dim(), "Answering in background…".dim()].into());
            }
            BtwHistoryOutcome::Failed(error) => {
                lines.push(vec!["↳ ".dim(), format!("Failed: {error}").red()].into());
            }
            BtwHistoryOutcome::Completed(answer) => {
                let mut answer_lines = Vec::new();
                let wrap_width = usize::from(width.max(1)).saturating_sub(2).max(1);
                append_markdown(
                    answer,
                    Some(wrap_width),
                    Some(self.cwd.as_path()),
                    &mut answer_lines,
                );
                if answer_lines.is_empty() {
                    lines.push(vec!["↳ ".dim(), "(No answer returned)".dim()].into());
                } else {
                    lines.extend(prefix_lines(answer_lines, "↳ ".dim(), "  ".into()));
                }
            }
        }

        lines
    }
}

pub(crate) async fn run_btw(
    request_handle: AppServerRequestHandle,
    request: BtwRequest,
) -> Result<String> {
    let forked_thread_id = fork_btw_thread(&request_handle, request.source_thread_id).await?;
    let result = async {
        let turn_id = start_btw_turn(&request_handle, &forked_thread_id, &request).await?;
        wait_for_btw_answer(&request_handle, &forked_thread_id, &turn_id).await
    }
    .await;

    if let Err(err) = unsubscribe_thread(&request_handle, &forked_thread_id).await {
        tracing::warn!(
            thread_id = forked_thread_id,
            error = %err,
            "failed to unsubscribe /btw side thread",
        );
    }

    result
}

async fn fork_btw_thread(
    request_handle: &AppServerRequestHandle,
    source_thread_id: ThreadId,
) -> Result<String> {
    let request_id = RequestId::String(format!("btw-fork-{}", Uuid::new_v4()));
    let response: ThreadForkResponse = request_handle
        .request_typed(ClientRequest::ThreadFork {
            request_id,
            params: ThreadForkParams {
                thread_id: source_thread_id.to_string(),
                ephemeral: true,
                persist_extended_history: true,
                ..ThreadForkParams::default()
            },
        })
        .await
        .wrap_err("thread/fork failed for /btw")?;
    Ok(response.thread.id)
}

async fn start_btw_turn(
    request_handle: &AppServerRequestHandle,
    thread_id: &str,
    request: &BtwRequest,
) -> Result<String> {
    let request_id = RequestId::String(format!("btw-turn-start-{}", Uuid::new_v4()));
    let response: TurnStartResponse = request_handle
        .request_typed(ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input: btw_turn_input(&request.items),
                cwd: Some(request.cwd.clone()),
                approval_policy: Some(AskForApproval::Never.into()),
                approvals_reviewer: Some(request.approvals_reviewer.into()),
                sandbox_policy: Some(SandboxPolicy::new_read_only_policy().into()),
                model: None,
                service_tier: None,
                effort: None,
                summary: None,
                personality: None,
                output_schema: None,
                collaboration_mode: None,
            },
        })
        .await
        .wrap_err("turn/start failed for /btw")?;
    Ok(response.turn.id)
}

fn btw_turn_input(items: &[CoreUserInput]) -> Vec<codex_app_server_protocol::UserInput> {
    let mut turn_input = Vec::with_capacity(items.len() + 1);
    turn_input.push(codex_app_server_protocol::UserInput::Text {
        text: BTW_INSTRUCTIONS.to_string(),
        text_elements: Vec::new(),
    });
    turn_input.extend(items.iter().cloned().map(Into::into));
    turn_input
}

async fn wait_for_btw_answer(
    request_handle: &AppServerRequestHandle,
    thread_id: &str,
    turn_id: &str,
) -> Result<String> {
    let deadline = Instant::now() + BTW_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            color_eyre::eyre::bail!("Timed out waiting for /btw to finish.");
        }

        let request_id = RequestId::String(format!("btw-thread-read-{}", Uuid::new_v4()));
        let response: ThreadReadResponse = request_handle
            .request_typed(ClientRequest::ThreadRead {
                request_id,
                params: ThreadReadParams {
                    thread_id: thread_id.to_string(),
                    include_turns: true,
                },
            })
            .await
            .wrap_err("thread/read failed while waiting for /btw")?;

        if let Some(turn) = response
            .thread
            .turns
            .into_iter()
            .find(|turn| turn.id == turn_id)
        {
            match turn.status {
                TurnStatus::Completed => return extract_btw_answer(&turn),
                TurnStatus::Failed => {
                    let message = turn
                        .error
                        .map(|error| error.message)
                        .unwrap_or_else(|| "The /btw side question failed.".to_string());
                    color_eyre::eyre::bail!("{message}");
                }
                TurnStatus::Interrupted => {
                    color_eyre::eyre::bail!("The /btw side question was interrupted.");
                }
                TurnStatus::InProgress => {}
            }
        }

        sleep(BTW_POLL_INTERVAL).await;
    }
}

async fn unsubscribe_thread(
    request_handle: &AppServerRequestHandle,
    thread_id: &str,
) -> Result<()> {
    let request_id = RequestId::String(format!("btw-unsubscribe-{}", Uuid::new_v4()));
    let _: ThreadUnsubscribeResponse = request_handle
        .request_typed(ClientRequest::ThreadUnsubscribe {
            request_id,
            params: ThreadUnsubscribeParams {
                thread_id: thread_id.to_string(),
            },
        })
        .await
        .wrap_err("thread/unsubscribe failed for /btw")?;
    Ok(())
}

fn extract_btw_answer(turn: &Turn) -> Result<String> {
    let mut final_answers = Vec::new();
    let mut fallback_answers = Vec::new();

    for item in &turn.items {
        let ThreadItem::AgentMessage { text, phase, .. } = item else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let owned = trimmed.to_string();
        if matches!(phase, Some(MessagePhase::FinalAnswer)) {
            final_answers.push(owned);
        } else {
            fallback_answers.push(owned);
        }
    }

    let answers = if final_answers.is_empty() {
        fallback_answers
    } else {
        final_answers
    };

    if answers.is_empty() {
        color_eyre::eyre::bail!("No assistant answer was returned for /btw.");
    }

    Ok(answers.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::BtwHistoryCell;
    use super::extract_btw_answer;
    use crate::history_cell::HistoryCell;
    use codex_app_server_protocol::ThreadItem;
    use codex_app_server_protocol::Turn;
    use codex_app_server_protocol::TurnStatus;
    use codex_protocol::models::MessagePhase;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    fn lines_to_string(cell: &BtwHistoryCell) -> String {
        cell.display_lines(/*width*/ 60)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn pending_btw_history_cell_snapshot() {
        let cell = BtwHistoryCell::pending(
            "why did the last change work?".to_string(),
            PathBuf::from("/tmp/project"),
        );
        assert_snapshot!("btw_history_pending", lines_to_string(&cell));
    }

    #[test]
    fn completed_btw_history_cell_snapshot() {
        let cell = BtwHistoryCell::completed(
            "what changed?".to_string(),
            "- Updated `foo`\n- Kept `bar` untouched".to_string(),
            PathBuf::from("/tmp/project"),
        );
        assert_snapshot!("btw_history_completed", lines_to_string(&cell));
    }

    #[test]
    fn extract_btw_answer_prefers_final_answer_phase() {
        let turn = Turn {
            id: "turn-1".to_string(),
            items: vec![
                ThreadItem::AgentMessage {
                    id: "msg-1".to_string(),
                    text: "working".to_string(),
                    phase: Some(MessagePhase::Commentary),
                    memory_citation: None,
                },
                ThreadItem::AgentMessage {
                    id: "msg-2".to_string(),
                    text: "done".to_string(),
                    phase: Some(MessagePhase::FinalAnswer),
                    memory_citation: None,
                },
            ],
            status: TurnStatus::Completed,
            error: None,
        };

        assert_eq!(extract_btw_answer(&turn).unwrap(), "done");
    }

    #[test]
    fn extract_btw_answer_falls_back_to_non_empty_messages() {
        let turn = Turn {
            id: "turn-1".to_string(),
            items: vec![ThreadItem::AgentMessage {
                id: "msg-1".to_string(),
                text: "fallback answer".to_string(),
                phase: None,
                memory_citation: None,
            }],
            status: TurnStatus::Completed,
            error: None,
        };

        assert_eq!(extract_btw_answer(&turn).unwrap(), "fallback answer");
    }
}

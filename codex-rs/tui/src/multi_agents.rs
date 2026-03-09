use crate::history_cell::PlainHistoryCell;
use crate::render::line_utils::prefix_lines;
use crate::text_formatting::truncate_text;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CollabAgentInteractionEndEvent;
use codex_protocol::protocol::CollabAgentRef;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::CollabAgentStatusEntry;
use codex_protocol::protocol::CollabCloseEndEvent;
use codex_protocol::protocol::CollabResumeBeginEvent;
use codex_protocol::protocol::CollabResumeEndEvent;
use codex_protocol::protocol::CollabWaitingBeginEvent;
use codex_protocol::protocol::CollabWaitingEndEvent;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use std::collections::HashMap;
use std::collections::HashSet;

const COLLAB_PROMPT_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES: usize = 160;
const COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES: usize = 240;
const TEAM_SPAWN_CALL_PREFIX: &str = "team/spawn:";
const TEAM_WAIT_CALL_PREFIX: &str = "team/wait:";
const TEAM_CLOSE_CALL_PREFIX: &str = "team/close:";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AgentPickerBadge {
    #[default]
    None,
    Working,
    Finish,
    Error,
}

impl AgentPickerBadge {
    fn suffix(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Working => Some("[working]"),
            Self::Finish => Some("[finish]"),
            Self::Error => Some("[error]"),
        }
    }
}

pub(crate) fn agent_picker_badge_from_status(status: &AgentStatus) -> AgentPickerBadge {
    match status {
        AgentStatus::Running => AgentPickerBadge::Working,
        AgentStatus::Completed(_) => AgentPickerBadge::Finish,
        AgentStatus::Errored(_) => AgentPickerBadge::Error,
        AgentStatus::PendingInit | AgentStatus::Shutdown | AgentStatus::NotFound => {
            AgentPickerBadge::None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AgentPickerThreadEntry {
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) is_closed: bool,
    pub(crate) badge: AgentPickerBadge,
}

#[derive(Clone, Copy)]
struct AgentLabel<'a> {
    thread_id: Option<ThreadId>,
    nickname: Option<&'a str>,
    role: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeamCallKind {
    Spawn,
    Wait,
    Close,
}

pub(crate) fn agent_picker_status_dot_spans(is_closed: bool) -> Vec<Span<'static>> {
    let dot = if is_closed {
        "•".into()
    } else {
        "•".green()
    };
    vec![dot, " ".into()]
}

pub(crate) fn format_agent_picker_item_name(
    agent_nickname: Option<&str>,
    agent_role: Option<&str>,
    is_primary: bool,
) -> String {
    if is_primary {
        return "Main [default]".to_string();
    }

    let agent_nickname = agent_nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let agent_role = agent_role.map(str::trim).filter(|role| !role.is_empty());
    match (agent_nickname, agent_role) {
        (Some(agent_nickname), Some(agent_role)) => format!("{agent_nickname} [{agent_role}]"),
        (Some(agent_nickname), None) => agent_nickname.to_string(),
        (None, Some(agent_role)) => format!("[{agent_role}]"),
        (None, None) => "Agent".to_string(),
    }
}

pub(crate) fn format_agent_picker_list_item_name(
    agent_nickname: Option<&str>,
    agent_role: Option<&str>,
    badge: AgentPickerBadge,
    is_primary: bool,
) -> String {
    let mut label = format_agent_picker_item_name(agent_nickname, agent_role, is_primary);
    if !is_primary && let Some(suffix) = badge.suffix() {
        label.push(' ');
        label.push_str(suffix);
    }
    label
}

pub(crate) fn sort_agent_picker_threads(agent_threads: &mut [(ThreadId, AgentPickerThreadEntry)]) {
    agent_threads.sort_by(|(left_id, left), (right_id, right)| {
        left.is_closed
            .cmp(&right.is_closed)
            .then_with(|| left_id.to_string().cmp(&right_id.to_string()))
    });
}

pub(crate) fn spawn_end(ev: CollabAgentSpawnEndEvent) -> PlainHistoryCell {
    let CollabAgentSpawnEndEvent {
        call_id: _,
        sender_thread_id: _,
        new_thread_id,
        new_agent_nickname,
        new_agent_role,
        prompt,
        status: _,
    } = ev;

    let title = match new_thread_id {
        Some(thread_id) => title_with_agent(
            "Spawned",
            AgentLabel {
                thread_id: Some(thread_id),
                nickname: new_agent_nickname.as_deref(),
                role: new_agent_role.as_deref(),
            },
        ),
        None => title_text("Agent spawn failed"),
    };

    let mut details = Vec::new();
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

pub(crate) fn interaction_end(ev: CollabAgentInteractionEndEvent) -> PlainHistoryCell {
    let CollabAgentInteractionEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        prompt,
        status: _,
    } = ev;

    let title = title_with_agent(
        "Sent input to",
        AgentLabel {
            thread_id: Some(receiver_thread_id),
            nickname: receiver_agent_nickname.as_deref(),
            role: receiver_agent_role.as_deref(),
        },
    );

    let mut details = Vec::new();
    if let Some(line) = prompt_line(&prompt) {
        details.push(line);
    }
    collab_event(title, details)
}

pub(crate) fn waiting_begin(ev: CollabWaitingBeginEvent) -> PlainHistoryCell {
    let CollabWaitingBeginEvent {
        sender_thread_id: _,
        receiver_thread_ids,
        receiver_agents,
        call_id,
    } = ev;
    let receiver_agents = merge_wait_receivers(&receiver_thread_ids, receiver_agents);

    let (title, details) = match team_call_kind(&call_id) {
        Some(TeamCallKind::Spawn) => (
            title_text("Creating agent team"),
            team_agent_details(&receiver_agents),
        ),
        Some(TeamCallKind::Wait) => (
            title_text("Waiting for agent team"),
            team_agent_details(&receiver_agents),
        ),
        Some(TeamCallKind::Close) => (
            title_text("Closing agent team"),
            team_agent_details(&receiver_agents),
        ),
        None => {
            let title = match receiver_agents.as_slice() {
                [receiver] => title_with_agent("Waiting for", agent_label_from_ref(receiver)),
                [] => title_text("Waiting for agents"),
                _ => title_text(format!("Waiting for {} agents", receiver_agents.len())),
            };
            let details = if receiver_agents.len() > 1 {
                receiver_agents
                    .iter()
                    .map(|receiver| agent_label_line(agent_label_from_ref(receiver)))
                    .collect()
            } else {
                Vec::new()
            };
            (title, details)
        }
    };

    collab_event(title, details)
}

pub(crate) fn waiting_end(ev: CollabWaitingEndEvent) -> PlainHistoryCell {
    let CollabWaitingEndEvent {
        call_id,
        sender_thread_id: _,
        agent_statuses,
        statuses,
        failure_reason,
    } = ev;
    let mut details = wait_complete_lines(&statuses, &agent_statuses);
    if let Some(reason) = failure_reason.as_deref() {
        details.push(failure_reason_line(reason));
    }
    let title = match (team_call_kind(&call_id), failure_reason.is_some()) {
        (Some(TeamCallKind::Spawn), false) => title_text("Created agent team"),
        (Some(TeamCallKind::Spawn), true) => title_text("Agent team creation failed"),
        (Some(TeamCallKind::Wait), false) => title_text("Finished waiting for agent team"),
        (Some(TeamCallKind::Wait), true) => title_text("Agent team wait failed"),
        (Some(TeamCallKind::Close), false) => title_text("Closed agent team"),
        (Some(TeamCallKind::Close), true) => title_text("Agent team close failed"),
        (None, false) => title_text("Finished waiting"),
        (None, true) => title_text("Wait failed"),
    };
    collab_event(title, details)
}

pub(crate) fn close_end(ev: CollabCloseEndEvent) -> PlainHistoryCell {
    let CollabCloseEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status: _,
    } = ev;

    collab_event(
        title_with_agent(
            "Closed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
        ),
        Vec::new(),
    )
}

pub(crate) fn resume_begin(ev: CollabResumeBeginEvent) -> PlainHistoryCell {
    let CollabResumeBeginEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
    } = ev;

    collab_event(
        title_with_agent(
            "Resuming",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
        ),
        Vec::new(),
    )
}

pub(crate) fn resume_end(ev: CollabResumeEndEvent) -> PlainHistoryCell {
    let CollabResumeEndEvent {
        call_id: _,
        sender_thread_id: _,
        receiver_thread_id,
        receiver_agent_nickname,
        receiver_agent_role,
        status,
    } = ev;

    collab_event(
        title_with_agent(
            "Resumed",
            AgentLabel {
                thread_id: Some(receiver_thread_id),
                nickname: receiver_agent_nickname.as_deref(),
                role: receiver_agent_role.as_deref(),
            },
        ),
        vec![status_summary_line(&status)],
    )
}

fn collab_event(title: Line<'static>, details: Vec<Line<'static>>) -> PlainHistoryCell {
    let mut lines: Vec<Line<'static>> = vec![title];
    if !details.is_empty() {
        lines.extend(prefix_lines(details, "  └ ".dim(), "    ".into()));
    }
    PlainHistoryCell::new(lines)
}

fn title_text(title: impl Into<String>) -> Line<'static> {
    title_spans_line(vec![Span::from(title.into()).bold()])
}

fn title_with_agent(prefix: &str, agent: AgentLabel<'_>) -> Line<'static> {
    let mut spans = vec![Span::from(format!("{prefix} ")).bold()];
    spans.extend(agent_label_spans(agent));
    title_spans_line(spans)
}

fn title_spans_line(mut spans: Vec<Span<'static>>) -> Line<'static> {
    let mut title = Vec::with_capacity(spans.len() + 1);
    title.push(Span::from("• ").dim());
    title.append(&mut spans);
    title.into()
}

fn agent_label_from_ref(agent: &CollabAgentRef) -> AgentLabel<'_> {
    AgentLabel {
        thread_id: Some(agent.thread_id),
        nickname: agent.agent_nickname.as_deref(),
        role: agent.agent_role.as_deref(),
    }
}

fn team_call_kind(call_id: &str) -> Option<TeamCallKind> {
    if call_id.starts_with(TEAM_SPAWN_CALL_PREFIX) {
        Some(TeamCallKind::Spawn)
    } else if call_id.starts_with(TEAM_WAIT_CALL_PREFIX) {
        Some(TeamCallKind::Wait)
    } else if call_id.starts_with(TEAM_CLOSE_CALL_PREFIX) {
        Some(TeamCallKind::Close)
    } else {
        None
    }
}

fn team_agent_details(receiver_agents: &[CollabAgentRef]) -> Vec<Line<'static>> {
    receiver_agents
        .iter()
        .map(|receiver| agent_label_line(agent_label_from_ref(receiver)))
        .collect()
}

fn agent_label_line(agent: AgentLabel<'_>) -> Line<'static> {
    agent_label_spans(agent).into()
}

fn agent_label_spans(agent: AgentLabel<'_>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let nickname = agent
        .nickname
        .map(str::trim)
        .filter(|nickname| !nickname.is_empty());
    let role = agent.role.map(str::trim).filter(|role| !role.is_empty());

    if let Some(nickname) = nickname {
        spans.push(Span::from(nickname.to_string()).cyan().bold());
    } else if let Some(thread_id) = agent.thread_id {
        spans.push(Span::from(thread_id.to_string()).cyan());
    } else {
        spans.push(Span::from("agent").cyan());
    }

    if let Some(role) = role {
        spans.push(Span::from(" ").dim());
        spans.push(Span::from(format!("[{role}]")));
    }

    spans
}

fn prompt_line(prompt: &str) -> Option<Line<'static>> {
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(Line::from(Span::from(truncate_text(
            trimmed,
            COLLAB_PROMPT_PREVIEW_GRAPHEMES,
        ))))
    }
}

fn merge_wait_receivers(
    receiver_thread_ids: &[ThreadId],
    mut receiver_agents: Vec<CollabAgentRef>,
) -> Vec<CollabAgentRef> {
    if receiver_agents.is_empty() {
        return receiver_thread_ids
            .iter()
            .map(|thread_id| CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            })
            .collect();
    }

    let mut seen = receiver_agents
        .iter()
        .map(|agent| agent.thread_id)
        .collect::<HashSet<_>>();
    for thread_id in receiver_thread_ids {
        if seen.insert(*thread_id) {
            receiver_agents.push(CollabAgentRef {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
            });
        }
    }
    receiver_agents
}

fn wait_complete_lines(
    statuses: &HashMap<ThreadId, AgentStatus>,
    agent_statuses: &[CollabAgentStatusEntry],
) -> Vec<Line<'static>> {
    if statuses.is_empty() && agent_statuses.is_empty() {
        return vec![Line::from(Span::from("No agents completed yet"))];
    }

    let entries = if agent_statuses.is_empty() {
        let mut entries = statuses
            .iter()
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries
    } else {
        let mut entries = agent_statuses.to_vec();
        let seen = entries
            .iter()
            .map(|entry| entry.thread_id)
            .collect::<HashSet<_>>();
        let mut extras = statuses
            .iter()
            .filter(|(thread_id, _)| !seen.contains(thread_id))
            .map(|(thread_id, status)| CollabAgentStatusEntry {
                thread_id: *thread_id,
                agent_nickname: None,
                agent_role: None,
                status: status.clone(),
            })
            .collect::<Vec<_>>();
        extras.sort_by(|left, right| left.thread_id.to_string().cmp(&right.thread_id.to_string()));
        entries.extend(extras);
        entries
    };

    entries
        .into_iter()
        .map(|entry| {
            let CollabAgentStatusEntry {
                thread_id,
                agent_nickname,
                agent_role,
                status,
            } = entry;
            let mut spans = agent_label_spans(AgentLabel {
                thread_id: Some(thread_id),
                nickname: agent_nickname.as_deref(),
                role: agent_role.as_deref(),
            });
            spans.push(Span::from(": ").dim());
            spans.extend(status_summary_spans(&status));
            spans.into()
        })
        .collect()
}

fn status_summary_line(status: &AgentStatus) -> Line<'static> {
    status_summary_spans(status).into()
}

fn failure_reason_line(reason: &str) -> Line<'static> {
    vec![
        "Failure: ".red(),
        truncate_text(reason, COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES).into(),
    ]
    .into()
}

fn status_summary_spans(status: &AgentStatus) -> Vec<Span<'static>> {
    match status {
        AgentStatus::PendingInit => vec![Span::from("Pending init").cyan()],
        AgentStatus::Running => vec![Span::from("Running").cyan().bold()],
        AgentStatus::Completed(message) => {
            let mut spans = vec![Span::from("Completed").green()];
            if let Some(message) = message.as_ref() {
                let message_preview = truncate_text(
                    &message.split_whitespace().collect::<Vec<_>>().join(" "),
                    COLLAB_AGENT_RESPONSE_PREVIEW_GRAPHEMES,
                );
                if !message_preview.is_empty() {
                    spans.push(Span::from(" - ").dim());
                    spans.push(Span::from(message_preview));
                }
            }
            spans
        }
        AgentStatus::Errored(error) => {
            let mut spans = vec![Span::from("Error").red()];
            let error_preview = truncate_text(
                &error.split_whitespace().collect::<Vec<_>>().join(" "),
                COLLAB_AGENT_ERROR_PREVIEW_GRAPHEMES,
            );
            if !error_preview.is_empty() {
                spans.push(Span::from(" - ").dim());
                spans.push(Span::from(error_preview));
            }
            spans
        }
        AgentStatus::Shutdown => vec![Span::from("Shutdown")],
        AgentStatus::NotFound => vec![Span::from("Not found").red()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::HistoryCell;
    use insta::assert_snapshot;
    use pretty_assertions::assert_eq;
    use ratatui::style::Color;
    use ratatui::style::Modifier;

    #[test]
    fn collab_events_snapshot() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let bob_id = ThreadId::from_string("00000000-0000-0000-0000-000000000003")
            .expect("valid bob thread id");

        let spawn = spawn_end(CollabAgentSpawnEndEvent {
            call_id: "call-spawn".to_string(),
            sender_thread_id,
            new_thread_id: Some(robie_id),
            new_agent_nickname: Some("Robie".to_string()),
            new_agent_role: Some("explorer".to_string()),
            prompt: "Compute 11! and reply with just the integer result.".to_string(),
            status: AgentStatus::PendingInit,
        });

        let send = interaction_end(CollabAgentInteractionEndEvent {
            call_id: "call-send".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            prompt: "Please continue and return the answer only.".to_string(),
            status: AgentStatus::Running,
        });

        let waiting = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![robie_id],
            receiver_agents: vec![CollabAgentRef {
                thread_id: robie_id,
                agent_nickname: Some("Robie".to_string()),
                agent_role: Some("explorer".to_string()),
            }],
            call_id: "call-wait".to_string(),
        });

        let mut statuses = HashMap::new();
        statuses.insert(
            robie_id,
            AgentStatus::Completed(Some("39916800".to_string())),
        );
        statuses.insert(bob_id, AgentStatus::Errored("tool timeout".to_string()));
        let finished = waiting_end(CollabWaitingEndEvent {
            sender_thread_id,
            call_id: "call-wait".to_string(),
            agent_statuses: vec![
                CollabAgentStatusEntry {
                    thread_id: robie_id,
                    agent_nickname: Some("Robie".to_string()),
                    agent_role: Some("explorer".to_string()),
                    status: AgentStatus::Completed(Some("39916800".to_string())),
                },
                CollabAgentStatusEntry {
                    thread_id: bob_id,
                    agent_nickname: Some("Bob".to_string()),
                    agent_role: Some("worker".to_string()),
                    status: AgentStatus::Errored("tool timeout".to_string()),
                },
            ],
            statuses,
            failure_reason: None,
        });

        let close = close_end(CollabCloseEndEvent {
            call_id: "call-close".to_string(),
            sender_thread_id,
            receiver_thread_id: robie_id,
            receiver_agent_nickname: Some("Robie".to_string()),
            receiver_agent_role: Some("explorer".to_string()),
            status: AgentStatus::Completed(Some("39916800".to_string())),
        });

        let snapshot = [spawn, send, waiting, finished, close]
            .iter()
            .map(cell_to_text)
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_snapshot!("collab_agent_transcript", snapshot);
    }

    #[test]
    fn title_styles_nickname_and_role() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let robie_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid robie thread id");
        let cell = spawn_end(CollabAgentSpawnEndEvent {
            call_id: "call-spawn".to_string(),
            sender_thread_id,
            new_thread_id: Some(robie_id),
            new_agent_nickname: Some("Robie".to_string()),
            new_agent_role: Some("explorer".to_string()),
            prompt: String::new(),
            status: AgentStatus::PendingInit,
        });

        let lines = cell.display_lines(200);
        let title = &lines[0];
        assert_eq!(title.spans[2].content.as_ref(), "Robie");
        assert_eq!(title.spans[2].style.fg, Some(Color::Cyan));
        assert!(title.spans[2].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(title.spans[4].content.as_ref(), "[explorer]");
        assert_eq!(title.spans[4].style.fg, None);
        assert!(!title.spans[4].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn team_spawn_waiting_end_uses_team_title() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let planner_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid planner thread id");
        let mut statuses = HashMap::new();
        statuses.insert(planner_id, AgentStatus::Completed(None));

        let cell = waiting_end(CollabWaitingEndEvent {
            call_id: "team/spawn:call-team".to_string(),
            sender_thread_id,
            agent_statuses: vec![CollabAgentStatusEntry {
                thread_id: planner_id,
                agent_nickname: Some("planner".to_string()),
                agent_role: Some("develop".to_string()),
                status: AgentStatus::Completed(None),
            }],
            statuses,
            failure_reason: None,
        });

        assert_snapshot!("collab_team_spawn_end", cell_to_text(&cell));
    }

    #[test]
    fn team_wait_failure_uses_failed_title() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let planner_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid planner thread id");
        let mut statuses = HashMap::new();
        statuses.insert(planner_id, AgentStatus::Completed(None));

        let cell = waiting_end(CollabWaitingEndEvent {
            call_id: "team/wait:call-team".to_string(),
            sender_thread_id,
            agent_statuses: vec![CollabAgentStatusEntry {
                thread_id: planner_id,
                agent_nickname: Some("planner".to_string()),
                agent_role: Some("develop".to_string()),
                status: AgentStatus::Completed(None),
            }],
            statuses,
            failure_reason: Some(
                "teammate_idle hook 'idle-guard' blocked: blocked teammate".into(),
            ),
        });

        assert_snapshot!("collab_team_wait_failed", cell_to_text(&cell));
    }

    #[test]
    fn team_close_waiting_begin_uses_team_title() {
        let sender_thread_id = ThreadId::from_string("00000000-0000-0000-0000-000000000001")
            .expect("valid sender thread id");
        let planner_id = ThreadId::from_string("00000000-0000-0000-0000-000000000002")
            .expect("valid planner thread id");
        let cell = waiting_begin(CollabWaitingBeginEvent {
            sender_thread_id,
            receiver_thread_ids: vec![planner_id],
            receiver_agents: vec![CollabAgentRef {
                thread_id: planner_id,
                agent_nickname: Some("planner".to_string()),
                agent_role: Some("develop".to_string()),
            }],
            call_id: "team/close:call-team".to_string(),
        });

        let text = cell_to_text(&cell);
        assert!(text.contains("Closing agent team"));
        assert!(text.contains("planner [develop]"));
    }

    fn cell_to_text(cell: &PlainHistoryCell) -> String {
        cell.display_lines(200)
            .iter()
            .map(line_to_text)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn line_to_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }
}

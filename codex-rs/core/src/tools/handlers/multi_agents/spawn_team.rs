use super::*;
use crate::agent::next_thread_spawn_depth;
use crate::agent::role::apply_role_to_config;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct SpawnTeamArgs {
    team_id: Option<String>,
    members: Vec<SpawnTeamMemberArgs>,
}

#[derive(Debug, Deserialize)]
pub(super) struct SpawnTeamMemberArgs {
    pub(super) name: String,
    pub(super) task: String,
    pub(super) agent_type: Option<String>,
    pub(super) model_provider: Option<String>,
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) worktree: bool,
    #[serde(default, alias = "backendground")]
    pub(super) background: bool,
}

#[derive(Debug, Serialize)]
struct SpawnTeamMemberResult {
    name: String,
    agent_id: String,
    status: AgentStatus,
}

#[derive(Debug, Serialize)]
struct SpawnTeamResult {
    team_id: String,
    members: Vec<SpawnTeamMemberResult>,
}

struct PendingSpawnDispatch {
    agent_id: ThreadId,
    notification_source: Option<SessionSource>,
    input_items: Vec<UserInput>,
    injected_context: String,
    background: bool,
}

pub async fn handle(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    let SpawnTeamArgs {
        team_id: provided_team_id,
        members: requested_members,
    } = parse_arguments(&arguments)?;
    if let Some(team_id) =
        find_team_for_member(turn.config.codex_home.as_path(), session.conversation_id).await?
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "spawn_team is disabled for agent team teammates (team `{team_id}`). Ask the team lead to spawn teams."
        )));
    }
    if requested_members.is_empty() {
        return Err(FunctionCallError::RespondToModel(
            "members must be non-empty".to_string(),
        ));
    }

    let mut seen_names = HashSet::new();
    for member in &requested_members {
        let name = member.name.trim();
        if name.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "member name must be non-empty".to_string(),
            ));
        }
        if !seen_names.insert(name.to_string()) {
            return Err(FunctionCallError::RespondToModel(format!(
                "duplicate member name `{name}`"
            )));
        }
        if member.task.trim().is_empty() {
            return Err(FunctionCallError::RespondToModel(format!(
                "task for member `{name}` must be non-empty"
            )));
        }
    }

    let team_id = match provided_team_id {
        Some(team_id) => normalized_team_id(&team_id)?,
        None => ThreadId::new().to_string(),
    };
    let team_dir_path = team_dir(turn.config.codex_home.as_path(), &team_id);
    let team_lock = lock_team(turn.config.codex_home.as_path(), &team_id).await?;
    if tokio::fs::try_exists(team_config_path(turn.config.codex_home.as_path(), &team_id))
        .await
        .map_err(|err| team_persistence_error("check team config", &team_id, err))?
    {
        return Err(FunctionCallError::RespondToModel(format!(
            "team `{team_id}` already exists"
        )));
    }

    let child_depth = next_thread_spawn_depth(&turn.session_source);
    if exceeds_thread_spawn_depth_limit(child_depth, turn.config.agent_max_depth) {
        return Err(FunctionCallError::RespondToModel(
            "Agent depth limit reached. Solve the task yourself.".to_string(),
        ));
    }
    let created_at = now_unix_seconds();

    let event_call_id = prefixed_team_call_id(TEAM_SPAWN_CALL_PREFIX, &call_id);
    session
        .send_event(
            &turn,
            CollabWaitingBeginEvent {
                sender_thread_id: session.conversation_id,
                receiver_thread_ids: Vec::new(),
                receiver_agents: Vec::new(),
                call_id: event_call_id.clone(),
            }
            .into(),
        )
        .await;

    let mut statuses = HashMap::new();
    let mut spawned_members = Vec::new();
    let mut pending_dispatches = Vec::new();

    for member in &requested_members {
        let member_name = member.name.trim().to_string();
        let role_name = optional_non_empty(&member.agent_type, "agent_type")?;
        let model_provider = optional_non_empty(&member.model_provider, "model_provider")?;
        let model = optional_non_empty(&member.model, "model")?;
        let use_worktree = member.worktree;
        let background = member.background;

        let config_result = async {
            let mut config = build_agent_spawn_config(
                &session.get_base_instructions().await,
                turn.as_ref(),
                child_depth,
            )?;
            apply_role_to_config(&mut config, role_name)
                .await
                .map_err(FunctionCallError::RespondToModel)?;
            apply_member_model_overrides(&mut config, model_provider, model)?;
            apply_spawn_agent_runtime_overrides(&mut config, turn.as_ref())?;
            apply_spawn_agent_overrides(&mut config, child_depth);
            Ok::<_, FunctionCallError>(config)
        }
        .await;
        let mut config = match config_result {
            Ok(config) => config,
            Err(err) => {
                cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
                let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
                session
                    .send_event(
                        &turn,
                        CollabWaitingEndEvent {
                            sender_thread_id: session.conversation_id,
                            call_id: event_call_id,
                            agent_statuses,
                            statuses,
                            failure_reason: None,
                        }
                        .into(),
                    )
                    .await;
                drop(team_lock);
                let _ = remove_dir_if_exists(&team_dir_path).await;
                return Err(err);
            }
        };
        let worktree_lease = if use_worktree {
            match create_agent_worktree(&session, &turn).await {
                Ok(lease) => {
                    config.cwd = lease.worktree_path.clone();
                    Some(lease)
                }
                Err(err) => {
                    cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
                    let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
                    session
                        .send_event(
                            &turn,
                            CollabWaitingEndEvent {
                                sender_thread_id: session.conversation_id,
                                call_id: event_call_id,
                                agent_statuses,
                                statuses,
                                failure_reason: None,
                            }
                            .into(),
                        )
                        .await;
                    drop(team_lock);
                    let _ = remove_dir_if_exists(&team_dir_path).await;
                    return Err(err);
                }
            }
        } else {
            None
        };
        let input_items = vec![UserInput::Text {
            text: member.task.trim().to_string(),
            text_elements: Vec::new(),
        }];
        let spawn_result = session
            .services
            .agent_control
            .spawn_agent_thread(
                config.clone(),
                Some(thread_spawn_source_with_role(
                    session.conversation_id,
                    child_depth,
                    role_name.map(str::to_owned),
                )),
            )
            .await;
        let spawn_result = match spawn_result {
            Ok(result) => Ok(result),
            Err(err @ CodexErr::AgentLimitReached { .. }) => {
                if reap_finished_agents_for_slots(session.as_ref(), turn.as_ref(), 1).await == 0 {
                    Err(err)
                } else {
                    session
                        .services
                        .agent_control
                        .spawn_agent_thread(
                            config,
                            Some(thread_spawn_source_with_role(
                                session.conversation_id,
                                child_depth,
                                role_name.map(str::to_owned),
                            )),
                        )
                        .await
                }
            }
            Err(err) => Err(err),
        }
        .map_err(collab_spawn_error);

        let (agent_id, notification_source) = match spawn_result {
            Ok((agent_id, notification_source)) => (agent_id, notification_source),
            Err(err) => {
                if let Some(lease) = worktree_lease {
                    let _ = remove_worktree_lease(&session, &turn, lease).await;
                }
                cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
                let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
                session
                    .send_event(
                        &turn,
                        CollabWaitingEndEvent {
                            sender_thread_id: session.conversation_id,
                            call_id: event_call_id,
                            agent_statuses,
                            statuses,
                            failure_reason: None,
                        }
                        .into(),
                    )
                    .await;
                drop(team_lock);
                let _ = remove_dir_if_exists(&team_dir_path).await;
                return Err(err);
            }
        };

        let hook_context = dispatch_subagent_start_hook(
            session.as_ref(),
            turn.as_ref(),
            agent_id,
            role_name.unwrap_or("default"),
        )
        .await;
        let team_bootstrap = build_spawn_team_bootstrap_message(
            &team_id,
            session.conversation_id,
            &member_name,
            member.agent_type.as_deref(),
            background,
            use_worktree,
            member.task.trim(),
            &requested_members,
        );
        let mut injected_context = vec![team_bootstrap];
        injected_context.extend(hook_context);
        let injected = injected_context.join("\n\n");
        if let Some(lease) = worktree_lease {
            if let Err(err) =
                register_worktree_lease(turn.config.codex_home.as_path(), agent_id, lease).await
            {
                let _ = session
                    .services
                    .agent_control
                    .shutdown_agent(agent_id)
                    .await;
                cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
                let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
                session
                    .send_event(
                        &turn,
                        CollabWaitingEndEvent {
                            sender_thread_id: session.conversation_id,
                            call_id: event_call_id,
                            agent_statuses,
                            statuses,
                            failure_reason: None,
                        }
                        .into(),
                    )
                    .await;
                drop(team_lock);
                let _ = remove_dir_if_exists(&team_dir_path).await;
                return Err(err);
            }
        }

        let status = session.services.agent_control.get_status(agent_id).await;
        statuses.insert(agent_id, status);
        spawned_members.push(TeamMember {
            name: member_name,
            member_id: ThreadId::new().to_string(),
            agent_id,
            agent_type: member.agent_type.clone(),
            model_provider: member.model_provider.clone(),
            model: member.model.clone(),
            initial_task: member.task.trim().to_string(),
            background,
            worktree: use_worktree,
        });
        pending_dispatches.push(PendingSpawnDispatch {
            agent_id,
            notification_source,
            input_items,
            injected_context: injected,
            background,
        });
    }
    let team_record = TeamRecord {
        members: spawned_members.clone(),
        created_at,
    };

    if let Err(err) = insert_team_record(
        session.conversation_id,
        team_id.clone(),
        team_record.clone(),
    ) {
        cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
        let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
        session
            .send_event(
                &turn,
                CollabWaitingEndEvent {
                    sender_thread_id: session.conversation_id,
                    call_id: event_call_id,
                    agent_statuses,
                    statuses,
                    failure_reason: None,
                }
                .into(),
            )
            .await;
        drop(team_lock);
        let _ = remove_dir_if_exists(&team_dir_path).await;
        return Err(err);
    }
    let initial_tasks = build_initial_team_tasks(&requested_members, &spawned_members, created_at);
    if let Err(err) = persist_team_state(
        turn.config.codex_home.as_path(),
        session.conversation_id,
        &team_id,
        PersistedTeamState::Active,
        &team_record,
        Some(&initial_tasks),
    )
    .await
    {
        let _ = remove_team_record(session.conversation_id, &team_id);
        let _ = remove_team_persistence(turn.config.codex_home.as_path(), &team_id).await;
        cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
        let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
        session
            .send_event(
                &turn,
                CollabWaitingEndEvent {
                    sender_thread_id: session.conversation_id,
                    call_id: event_call_id,
                    agent_statuses,
                    statuses,
                    failure_reason: None,
                }
                .into(),
            )
            .await;
        drop(team_lock);
        let _ = remove_dir_if_exists(&team_dir_path).await;
        return Err(err);
    }

    for dispatch in pending_dispatches {
        if !dispatch.injected_context.is_empty()
            && let Err(err) = session
                .services
                .agent_control
                .inject_developer_message_without_turn(dispatch.agent_id, dispatch.injected_context)
                .await
        {
            warn!("failed to inject spawn_team teammate context: {err}");
        }

        if let Err(err) = session
            .services
            .agent_control
            .send_spawn_input(
                dispatch.agent_id,
                dispatch.input_items,
                dispatch.notification_source,
            )
            .await
        {
            let _ = remove_team_record(session.conversation_id, &team_id);
            let _ = remove_team_persistence(turn.config.codex_home.as_path(), &team_id).await;
            cleanup_spawned_team_members(&session, &turn, &spawned_members).await;
            let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
            session
                .send_event(
                    &turn,
                    CollabWaitingEndEvent {
                        sender_thread_id: session.conversation_id,
                        call_id: event_call_id,
                        agent_statuses,
                        statuses,
                        failure_reason: None,
                    }
                    .into(),
                )
                .await;
            drop(team_lock);
            let _ = remove_dir_if_exists(&team_dir_path).await;
            return Err(collab_spawn_error(err));
        }

        if dispatch.background {
            maybe_start_background_agent_cleanup(session.clone(), turn.clone(), dispatch.agent_id);
        }
    }
    drop(team_lock);

    let agent_statuses = team_member_status_entries(&spawned_members, &statuses);
    session
        .send_event(
            &turn,
            CollabWaitingEndEvent {
                sender_thread_id: session.conversation_id,
                call_id: event_call_id,
                agent_statuses,
                statuses: statuses.clone(),
                failure_reason: None,
            }
            .into(),
        )
        .await;

    let members = spawned_members
        .into_iter()
        .map(|member| SpawnTeamMemberResult {
            status: statuses
                .get(&member.agent_id)
                .cloned()
                .unwrap_or(AgentStatus::NotFound),
            name: member.name,
            agent_id: member.agent_id.to_string(),
        })
        .collect::<Vec<_>>();
    let content = serde_json::to_string(&SpawnTeamResult { team_id, members }).map_err(|err| {
        FunctionCallError::Fatal(format!("failed to serialize spawn_team result: {err}"))
    })?;

    Ok(ToolOutput::Function {
        body: FunctionCallOutputBody::Text(content),
        success: Some(true),
    })
}

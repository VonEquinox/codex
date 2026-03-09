use super::*;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct TeamMessageArgs {
    team_id: String,
    member_name: String,
    message: Option<String>,
    items: Option<Vec<UserInput>>,
    #[serde(default)]
    interrupt: bool,
}

#[derive(Debug, Serialize)]
struct TeamMessageResult {
    team_id: String,
    member_name: String,
    agent_id: String,
    submission_id: String,
    delivered: bool,
    inbox_entry_id: String,
    error: Option<String>,
}

pub async fn handle(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    call_id: String,
    arguments: String,
) -> Result<ToolOutput, FunctionCallError> {
    let args: TeamMessageArgs = parse_arguments(&arguments)?;
    let team_id = normalized_team_id(&args.team_id)?;
    let config = read_persisted_team_config(turn.config.codex_home.as_path(), &team_id).await?;
    assert_team_member_or_lead(&team_id, &config, session.conversation_id)?;
    assert_team_state_allows_collaboration(&team_id, config.state, "team_message")?;
    let team = get_team_record(
        turn.config.codex_home.as_path(),
        session.conversation_id,
        &team_id,
    )
    .await?;
    let member = find_team_member(&team, &team_id, &args.member_name)?;
    let sender_thread_id = session.conversation_id.to_string();
    let sender_name = if sender_thread_id == config.lead_thread_id {
        Some("lead")
    } else {
        let sender = config
            .members
            .iter()
            .find(|candidate| candidate.agent_id == sender_thread_id)
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "thread `{}` is not a member of team `{team_id}`",
                    session.conversation_id
                ))
            })?;
        if member.agent_id.to_string() == sender.agent_id {
            return Err(FunctionCallError::RespondToModel(
                "team_message must target another teammate".to_string(),
            ));
        }
        Some(sender.name.as_str())
    };
    let input_items = parse_collab_input(args.message, args.items)?;
    let prompt = input_preview(&input_items);
    let inbox_entry_id = inbox::append_inbox_entry(
        turn.config.codex_home.as_path(),
        &team_id,
        member.agent_id,
        session.conversation_id,
        sender_name,
        &input_items,
        &prompt,
    )
    .await?;

    let delivery = send_input_to_member(
        &session,
        &turn,
        call_id,
        member.agent_id,
        input_items,
        prompt,
        args.interrupt,
    )
    .await;

    let (delivered, submission_id, error) = match delivery {
        Ok(submission_id) => {
            if let Err(err) = inbox::mark_inbox_entry_live_delivered(
                turn.config.codex_home.as_path(),
                &team_id,
                member.agent_id,
                &inbox_entry_id,
            )
            .await
            {
                warn!(
                    "failed to mark inbox entry {inbox_entry_id} as live-delivered for team \
                     {team_id}: {err}"
                );
            }
            (true, submission_id, None)
        }
        Err(err) => (false, String::new(), Some(err.to_string())),
    };

    let content = serde_json::to_string(&TeamMessageResult {
        team_id,
        member_name: member.name,
        agent_id: member.agent_id.to_string(),
        submission_id,
        delivered,
        inbox_entry_id,
        error,
    })
    .map_err(|err| {
        FunctionCallError::Fatal(format!("failed to serialize team_message result: {err}"))
    })?;

    Ok(ToolOutput::Function {
        body: FunctionCallOutputBody::Text(content),
        success: Some(true),
    })
}

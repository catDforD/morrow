use super::*;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ServerMessage {
    Snapshot {
        session: Session,
        running_turn: Option<RunningTurnSnapshot>,
        permissions: PermissionProfile,
        subagents: Vec<SubagentInstanceSnapshot>,
        approvals: Vec<ApprovalRequest>,
    },
    AgentEvent(Box<AgentEventEnvelope>),
    TurnSaved {
        session: String,
        turn_index: usize,
    },
    TurnRejected {
        request_id: String,
        reason: String,
    },
    ApprovalQueueUpdated {
        approvals: Vec<ApprovalRequest>,
    },
    SubagentTranscript {
        transcript: Box<SubagentTranscriptSnapshot>,
    },
    SubagentDeleted {
        instance_id: String,
    },
    SubagentRejected {
        request_id: String,
        reason: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ClientMessage {
    StartTurn {
        request_id: String,
        prompt: String,
        #[serde(default)]
        prompt_resolved: bool,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        model_selection: Option<ModelSelection>,
    },
    ApprovalDecision {
        request_id: String,
        approved: bool,
    },
    CancelTurn {
        turn_id: String,
    },
    SpawnSubagent {
        request_id: String,
        role: SubagentRole,
        task: String,
    },
    SendSubagent {
        request_id: String,
        instance_id: String,
        message: String,
        #[serde(default)]
        model_selection: Option<ModelSelection>,
    },
    InspectSubagent {
        request_id: String,
        instance_id: String,
    },
    CancelSubagent {
        instance_id: String,
    },
    DeleteSubagent {
        instance_id: String,
    },
}

pub(crate) async fn session_ws(
    State(state): State<AppState>,
    Path(name): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(error) = require_active_session(&state, &name) {
        return error.into_response();
    }
    ws.on_upgrade(move |socket| handle_socket(socket, state, name))
}

async fn handle_socket(socket: WebSocket, state: AppState, session_name: String) {
    let (mut sender, mut receiver) = socket.split();
    let resources = match register_session_subscription(&state, &session_name).await {
        Ok(resources) => resources,
        Err(error) => {
            let _ = send_session_frame(
                &mut sender,
                &SessionStreamFrame::ResyncRequired { reason: error },
            )
            .await;
            return;
        }
    };
    resources
        .handle
        .replace_subagents(resources.supervisor.snapshots().await)
        .await;
    resources
        .handle
        .set_approvals(approval_snapshots(&state, &session_name).await)
        .await;
    let tx = resources.tx;
    let mut subscription = match resources.handle.subscribe().await {
        Ok(subscription) => subscription,
        Err(error) => {
            let _ = send_session_frame(
                &mut sender,
                &SessionStreamFrame::ResyncRequired {
                    reason: error.to_string(),
                },
            )
            .await;
            release_session_subscription(&state, &session_name).await;
            return;
        }
    };
    let snapshot = SessionStreamFrame::Snapshot(Box::new(subscription.snapshot.clone()));
    if send_session_frame(&mut sender, &snapshot).await.is_err() {
        release_session_subscription(&state, &session_name).await;
        return;
    }

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                match handle_client_ws_message(message, &state, &session_name, &tx).await {
                    Ok(Some(frame)) => {
                        if send_session_frame(&mut sender, &frame).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(()) => break,
                }
            }
            event = subscription.recv() => {
                match event {
                    Ok(event) => {
                        let frame = SessionStreamFrame::Event(Box::new(event));
                        if send_session_frame(&mut sender, &frame).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let frame = SessionStreamFrame::ResyncRequired {
                            reason: format!("session stream lagged by {skipped} events"),
                        };
                        let _ = send_session_frame(&mut sender, &frame).await;
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    release_session_subscription(&state, &session_name).await;
}

async fn send_session_frame(
    sender: &mut SplitSink<WebSocket, Message>,
    message: &SessionStreamFrame,
) -> Result<(), ()> {
    let json = serde_json::to_string(message).map_err(|_| ())?;
    sender
        .send(Message::Text(json.into()))
        .await
        .map_err(|_| ())
}

async fn handle_client_ws_message(
    message: Message,
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
) -> Result<Option<SessionStreamFrame>, ()> {
    let text = match message {
        Message::Text(text) => text,
        Message::Close(_) => return Err(()),
        _ => return Ok(None),
    };

    let parsed = serde_json::from_str::<ClientMessage>(&text);
    let Ok(message) = parsed else {
        return Ok(Some(SessionStreamFrame::CommandResult {
            request_id: "invalid-message".to_string(),
            accepted: false,
            operation_id: None,
            turn_id: None,
            error: Some("invalid websocket message".to_string()),
        }));
    };

    Ok(dispatch_client_message(message, state, session_name, tx).await)
}

pub(crate) async fn dispatch_client_message(
    message: ClientMessage,
    state: &AppState,
    session_name: &str,
    tx: &broadcast::Sender<ServerMessage>,
) -> Option<SessionStreamFrame> {
    match message {
        ClientMessage::StartTurn {
            request_id,
            prompt,
            prompt_resolved,
            permission_mode,
            model_selection,
        } => {
            let result = start_turn(
                state.clone(),
                session_name.to_string(),
                StartTurnRequest {
                    prompt,
                    prompt_resolved,
                    permission_mode,
                    model_selection,
                    resolved_model: None,
                    mcp_servers: None,
                    subagent_identities: None,
                    subagent_role_overrides: None,
                    subagent_role_models: None,
                },
                tx.clone(),
            )
            .await;
            Some(match result {
                Ok((operation_id, turn_id)) => SessionStreamFrame::CommandResult {
                    request_id,
                    accepted: true,
                    operation_id: Some(operation_id),
                    turn_id: Some(turn_id),
                    error: None,
                },
                Err(error) => SessionStreamFrame::CommandResult {
                    request_id,
                    accepted: false,
                    operation_id: None,
                    turn_id: None,
                    error: Some(error),
                },
            })
        }
        ClientMessage::ApprovalDecision {
            request_id,
            approved,
        } => {
            resolve_approval(state, session_name, request_id, approved, tx).await;
            None
        }
        ClientMessage::CancelTurn { turn_id } => {
            cancel_turn(state, session_name, turn_id, tx).await;
            None
        }
        ClientMessage::SpawnSubagent {
            request_id,
            role,
            task,
        } => {
            let result = with_session_command(state, session_name, async {
                let supervisor = prepare_direct_subagent_supervisor(state, session_name).await?;
                supervisor.spawn(role, task).await
            })
            .await;
            Some(command_result(request_id, result.map(|_| ())))
        }
        ClientMessage::SendSubagent {
            request_id,
            instance_id,
            message,
            ..
        } => {
            let result = with_session_command(state, session_name, async {
                let supervisor = prepare_direct_subagent_supervisor(state, session_name).await?;
                supervisor.send(instance_id, message).await
            })
            .await;
            Some(command_result(request_id, result.map(|_| ())))
        }
        ClientMessage::InspectSubagent {
            request_id,
            instance_id,
        } => {
            let result = async {
                let resources = ensure_session_resources(state, session_name).await?;
                let document = resources.supervisor.document(&instance_id).await?;
                let projection = resources
                    .supervisor
                    .projection(&instance_id)
                    .await
                    .map_err(|error| error.to_string())?;
                let events = resources
                    .supervisor
                    .events(&instance_id)
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>(SubagentTranscriptSnapshot::from_document(
                    document, projection, events,
                ))
            }
            .await;
            match result {
                Ok(transcript) => Some(SessionStreamFrame::CommandData {
                    request_id,
                    data: serde_json::to_value(transcript)
                        .expect("subagent transcript must serialize"),
                }),
                Err(error) => Some(SessionStreamFrame::CommandResult {
                    request_id,
                    accepted: false,
                    operation_id: None,
                    turn_id: None,
                    error: Some(error),
                }),
            }
        }
        ClientMessage::CancelSubagent { instance_id } => {
            let result = async {
                let resources = ensure_session_resources(state, session_name).await?;
                resources.supervisor.cancel(instance_id).await
            }
            .await;
            if let Err(error) = result {
                broadcast_error(tx, error);
            }
            None
        }
        ClientMessage::DeleteSubagent { instance_id } => {
            let result = async {
                let resources = ensure_session_resources(state, session_name).await?;
                resources.supervisor.delete(&instance_id).await?;
                resources.handle.remove_subagent(instance_id.clone()).await;
                Ok::<_, String>(())
            }
            .await;
            match result {
                Ok(()) => broadcast_message(tx, ServerMessage::SubagentDeleted { instance_id }),
                Err(error) => broadcast_error(tx, error),
            }
            None
        }
    }
}

fn command_result(request_id: String, result: Result<(), String>) -> SessionStreamFrame {
    match result {
        Ok(()) => SessionStreamFrame::CommandResult {
            request_id,
            accepted: true,
            operation_id: None,
            turn_id: None,
            error: None,
        },
        Err(error) => SessionStreamFrame::CommandResult {
            request_id,
            accepted: false,
            operation_id: None,
            turn_id: None,
            error: Some(error),
        },
    }
}

pub(crate) fn broadcast_message(tx: &broadcast::Sender<ServerMessage>, message: ServerMessage) {
    let _ = tx.send(message);
}

pub(crate) fn broadcast_error(tx: &broadcast::Sender<ServerMessage>, message: impl ToString) {
    broadcast_message(
        tx,
        ServerMessage::Error {
            message: message.to_string(),
        },
    );
}

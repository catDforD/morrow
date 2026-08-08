use agent_protocol::{
    ApprovalOrigin, Message, ModelContextProjection, Session, SessionFact, SessionFactEnvelope,
    SessionLogHeader, SessionProjection, SessionStepKind, SessionStepProjection, SessionStepStatus,
    SessionTurnStatus, Thread, Turn, TurnProjection, TurnRecord, TurnStatus, TurnStep,
    TurnStepKind,
};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionProjectionError {
    #[error("session fact revision {actual} does not follow {expected}")]
    RevisionGap { expected: u64, actual: u64 },
    #[error("session fact revision {revision} is missing turn_id")]
    MissingTurnId { revision: u64 },
    #[error("session fact revision {revision} references unknown turn {turn_id:?}")]
    UnknownTurn { revision: u64, turn_id: String },
    #[error("session fact revision {revision} is missing operation_id")]
    MissingOperationId { revision: u64 },
}

#[derive(Debug, Clone)]
struct ProjectedTurn {
    projection: TurnProjection,
    completed_revision: Option<u64>,
}

pub fn project_session(
    header: &SessionLogHeader,
    facts: &[SessionFactEnvelope],
) -> Result<SessionProjection, SessionProjectionError> {
    let mut turns = Vec::<ProjectedTurn>::new();
    let mut turn_indices = HashMap::<String, usize>::new();
    let mut summary = None;
    let mut covered_through_turn_id = None;
    let mut compaction_revision = None;
    let mut legacy_checkpoint = None::<(u64, Vec<Message>)>;
    let mut middleware_audit = Vec::new();
    let mut diagnostics = Vec::new();

    for (index, envelope) in facts.iter().enumerate() {
        let expected_revision = index as u64 + 1;
        if envelope.revision != expected_revision {
            return Err(SessionProjectionError::RevisionGap {
                expected: expected_revision,
                actual: envelope.revision,
            });
        }
        match &envelope.fact {
            SessionFact::TurnStarted {
                user_message,
                model,
                permissions,
            } => {
                let turn_id =
                    envelope
                        .turn_id
                        .clone()
                        .ok_or(SessionProjectionError::MissingTurnId {
                            revision: envelope.revision,
                        })?;
                let operation_id = envelope.operation_id.clone().ok_or(
                    SessionProjectionError::MissingOperationId {
                        revision: envelope.revision,
                    },
                )?;
                let index = turns.len();
                turn_indices.insert(turn_id.clone(), index);
                turns.push(ProjectedTurn {
                    projection: TurnProjection {
                        id: turn_id,
                        operation_id,
                        index,
                        status: SessionTurnStatus::Running,
                        user_message: user_message.clone(),
                        model: model.clone(),
                        permissions: *permissions,
                        messages: vec![user_message.clone()],
                        steps: Vec::new(),
                        notices: Vec::new(),
                        error: None,
                        started_at_ms: envelope.timestamp_ms,
                        completed_at_ms: None,
                    },
                    completed_revision: None,
                });
            }
            SessionFact::ContextCompacted {
                summary: compacted,
                covered_through_turn_id: covered,
            } => {
                summary = Some(compacted.clone());
                covered_through_turn_id = Some(covered.clone());
                compaction_revision = Some(envelope.revision);
                legacy_checkpoint = None;
            }
            SessionFact::LegacyContextCheckpoint {
                messages,
                diagnostic,
                ..
            } => {
                legacy_checkpoint = Some((envelope.revision, messages.clone()));
                if let Some(diagnostic) = diagnostic {
                    diagnostics.push(diagnostic.clone());
                }
            }
            SessionFact::MiddlewareFinished { invocation } => {
                middleware_audit.push(invocation.clone());
            }
            fact => {
                let turn_id =
                    envelope
                        .turn_id
                        .as_deref()
                        .ok_or(SessionProjectionError::MissingTurnId {
                            revision: envelope.revision,
                        })?;
                let index = *turn_indices.get(turn_id).ok_or_else(|| {
                    SessionProjectionError::UnknownTurn {
                        revision: envelope.revision,
                        turn_id: turn_id.to_string(),
                    }
                })?;
                apply_turn_fact(&mut turns[index], envelope, fact);
            }
        }
    }

    let context = build_context(
        &turns,
        summary,
        covered_through_turn_id,
        compaction_revision,
        legacy_checkpoint,
    );

    Ok(SessionProjection {
        session_id: header.session_id.clone(),
        revision: facts.last().map_or(0, |fact| fact.revision),
        turns: turns.into_iter().map(|turn| turn.projection).collect(),
        context,
        middleware_audit,
        diagnostics,
    })
}

fn apply_turn_fact(turn: &mut ProjectedTurn, envelope: &SessionFactEnvelope, fact: &SessionFact) {
    match fact {
        SessionFact::NoticeRecorded { message } => turn.projection.notices.push(message.clone()),
        SessionFact::ModelCallStarted { model_call_id } => {
            turn.projection.steps.push(SessionStepProjection {
                id: model_call_id.clone(),
                kind: SessionStepKind::ModelCall,
                status: SessionStepStatus::Running,
                model_message: None,
                tool_call: None,
                tool_result: None,
                tool_summary: None,
                approval: None,
                approval_decision: None,
                error: None,
            });
        }
        SessionFact::ModelMessageCommitted {
            model_call_id,
            message,
        } => {
            if let Some(step) = turn
                .projection
                .steps
                .iter_mut()
                .rev()
                .find(|step| step.id == *model_call_id)
            {
                step.status = SessionStepStatus::Completed;
                step.model_message = Some(message.clone());
            }
            turn.projection.messages.push(message.clone());
        }
        SessionFact::ToolCallStarted { tool_call } => {
            turn.projection.steps.push(SessionStepProjection {
                id: tool_call.id.clone(),
                kind: SessionStepKind::ToolCall,
                status: SessionStepStatus::Running,
                model_message: None,
                tool_call: Some(tool_call.clone()),
                tool_result: None,
                tool_summary: None,
                approval: None,
                approval_decision: None,
                error: None,
            });
        }
        SessionFact::ApprovalRequested { request } => {
            let tool_call_id = match &request.origin {
                ApprovalOrigin::ParentTurn { tool_call_id, .. }
                | ApprovalOrigin::SubagentRun { tool_call_id, .. } => tool_call_id.as_deref(),
                ApprovalOrigin::Unknown => None,
            };
            let step_index = tool_call_id
                .and_then(|id| turn.projection.steps.iter().rposition(|step| step.id == id))
                .or_else(|| {
                    turn.projection.steps.iter().rposition(|step| {
                        step.kind == SessionStepKind::ToolCall
                            && step.status == SessionStepStatus::Running
                    })
                });
            if let Some(step) = step_index.map(|index| &mut turn.projection.steps[index]) {
                step.approval = Some(request.clone());
            }
        }
        SessionFact::ApprovalResolved { decision } => {
            if let Some(step) = turn.projection.steps.iter_mut().rev().find(|step| {
                step.approval
                    .as_ref()
                    .is_some_and(|request| request.id == decision.request_id)
            }) {
                step.approval_decision = Some(decision.clone());
            }
        }
        SessionFact::ToolCallFinished {
            tool_call_id,
            result,
            ok,
            summary,
        } => {
            if let Some(step) = turn
                .projection
                .steps
                .iter_mut()
                .rev()
                .find(|step| step.id == *tool_call_id)
            {
                step.status = if *ok {
                    SessionStepStatus::Completed
                } else {
                    SessionStepStatus::Failed
                };
                step.tool_result = Some(result.clone());
                step.tool_summary = summary.clone();
                step.error = summary.as_ref().and_then(|summary| summary.error.clone());
            }
            turn.projection.messages.push(result.clone());
        }
        SessionFact::TurnCompleted => {
            turn.projection.status = SessionTurnStatus::Completed;
            turn.projection.completed_at_ms = Some(envelope.timestamp_ms);
            turn.completed_revision = Some(envelope.revision);
        }
        SessionFact::TurnFailed { error } => {
            terminalize_turn(
                turn,
                SessionTurnStatus::Failed,
                SessionStepStatus::Failed,
                error,
                envelope,
            );
        }
        SessionFact::TurnCancelled { reason } => {
            terminalize_turn(
                turn,
                SessionTurnStatus::Cancelled,
                SessionStepStatus::Interrupted,
                reason,
                envelope,
            );
        }
        SessionFact::TurnInterrupted { reason } => {
            turn.projection.status = SessionTurnStatus::Interrupted;
            turn.projection.error = Some(reason.clone());
            turn.projection.completed_at_ms = Some(envelope.timestamp_ms);
            turn.completed_revision = Some(envelope.revision);
            for step in turn
                .projection
                .steps
                .iter_mut()
                .filter(|step| step.status == SessionStepStatus::Running)
            {
                step.status = if step.kind == SessionStepKind::ToolCall {
                    SessionStepStatus::OutcomeUnknown
                } else {
                    SessionStepStatus::Interrupted
                };
                step.error = Some(reason.clone());
            }
        }
        SessionFact::TurnStarted { .. }
        | SessionFact::ContextCompacted { .. }
        | SessionFact::MiddlewareFinished { .. }
        | SessionFact::LegacyContextCheckpoint { .. } => {}
    }
}

fn terminalize_turn(
    turn: &mut ProjectedTurn,
    status: SessionTurnStatus,
    step_status: SessionStepStatus,
    error: &str,
    envelope: &SessionFactEnvelope,
) {
    turn.projection.status = status;
    turn.projection.error = Some(error.to_string());
    turn.projection.completed_at_ms = Some(envelope.timestamp_ms);
    turn.completed_revision = Some(envelope.revision);
    for step in turn
        .projection
        .steps
        .iter_mut()
        .filter(|step| step.status == SessionStepStatus::Running)
    {
        step.status = step_status;
        step.error = Some(error.to_string());
    }
}

fn build_context(
    turns: &[ProjectedTurn],
    summary: Option<String>,
    covered_through_turn_id: Option<String>,
    compaction_revision: Option<u64>,
    legacy_checkpoint: Option<(u64, Vec<Message>)>,
) -> ModelContextProjection {
    let mut messages = Vec::new();
    let mut legacy = false;
    let mut minimum_revision = 0u64;

    if let Some((revision, checkpoint)) = legacy_checkpoint {
        messages = checkpoint;
        minimum_revision = revision;
        legacy = true;
    } else {
        if let Some(summary) = summary.as_ref() {
            messages.push(Message::system(format!("Session summary:\n{summary}")));
        }
        if let Some(revision) = compaction_revision {
            minimum_revision = revision;
        }
    }

    let covered_index = covered_through_turn_id
        .as_ref()
        .and_then(|id| turns.iter().position(|turn| turn.projection.id == *id));

    for (index, turn) in turns.iter().enumerate() {
        if turn.projection.status != SessionTurnStatus::Completed {
            continue;
        }
        if legacy {
            if turn
                .completed_revision
                .is_some_and(|revision| revision > minimum_revision)
            {
                messages.extend(turn.projection.messages.clone());
            }
            continue;
        }
        if covered_index.is_some_and(|covered| index <= covered) {
            continue;
        }
        messages.extend(turn.projection.messages.clone());
    }

    ModelContextProjection {
        summary,
        covered_through_turn_id,
        messages,
        legacy_checkpoint: legacy,
    }
}

pub fn projection_to_legacy_session(projection: &SessionProjection) -> Session {
    let turns = projection
        .turns
        .iter()
        .map(projection_turn_to_record)
        .collect::<Vec<_>>();
    let summarized_turns = projection
        .context
        .covered_through_turn_id
        .as_ref()
        .and_then(|id| projection.turns.iter().position(|turn| turn.id == *id))
        .map_or(0, |index| index + 1);
    Session {
        active_thread: Thread {
            messages: projection.context.messages.clone(),
        },
        turns,
        context: agent_protocol::SessionContext {
            summary: projection.context.summary.clone(),
            summarized_turns,
        },
    }
}

fn projection_turn_to_record(projection: &TurnProjection) -> TurnRecord {
    let status = match projection.status {
        SessionTurnStatus::Running => TurnStatus::Running,
        SessionTurnStatus::Completed => TurnStatus::Completed,
        SessionTurnStatus::Failed
        | SessionTurnStatus::Cancelled
        | SessionTurnStatus::Interrupted => TurnStatus::Failed,
    };
    let assistant_message = projection
        .messages
        .iter()
        .rev()
        .find(|message| message.role == agent_protocol::Role::Assistant)
        .cloned();
    let steps = projection
        .steps
        .iter()
        .map(|step| TurnStep {
            kind: match step.kind {
                SessionStepKind::ModelCall => TurnStepKind::ModelCall,
                SessionStepKind::ToolCall => TurnStepKind::ToolCall,
            },
            status: match step.status {
                SessionStepStatus::Running => TurnStatus::Running,
                SessionStepStatus::Completed => TurnStatus::Completed,
                SessionStepStatus::Failed
                | SessionStepStatus::Interrupted
                | SessionStepStatus::OutcomeUnknown => TurnStatus::Failed,
            },
            tool_name: step
                .tool_call
                .as_ref()
                .map(|call| call.function.name.clone()),
            tool_call_id: step.tool_call.as_ref().map(|call| call.id.clone()),
            error: step.error.clone(),
        })
        .collect();
    TurnRecord {
        turn: Turn {
            status,
            user_message: projection.user_message.clone(),
            assistant_message,
            model: Some(projection.model.clone()),
            steps,
            error: projection.error.clone(),
        },
        messages: projection.messages.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{PermissionProfile, ReasoningLevel, SessionFact};

    fn model() -> agent_protocol::ModelInvocation {
        agent_protocol::ModelInvocation {
            provider_id: "test".to_string(),
            provider_name: "Test".to_string(),
            model_id: "model".to_string(),
            model_name: "Model".to_string(),
            reasoning: ReasoningLevel::Off,
        }
    }

    #[test]
    fn completed_turn_enters_context_and_interrupted_turn_does_not() {
        let header = SessionLogHeader {
            schema_version: agent_protocol::SESSION_DOCUMENT_SCHEMA_VERSION,
            session_id: "session-1".to_string(),
            created_at_ms: 1,
        };
        let facts = vec![
            SessionFactEnvelope {
                revision: 1,
                timestamp_ms: 1,
                operation_id: Some("op-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                fact: SessionFact::TurnStarted {
                    user_message: Message::user("hello"),
                    model: model(),
                    permissions: PermissionProfile::default(),
                },
            },
            SessionFactEnvelope {
                revision: 2,
                timestamp_ms: 2,
                operation_id: Some("op-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                fact: SessionFact::ModelCallStarted {
                    model_call_id: "model-1".to_string(),
                },
            },
            SessionFactEnvelope {
                revision: 3,
                timestamp_ms: 3,
                operation_id: Some("op-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                fact: SessionFact::ModelMessageCommitted {
                    model_call_id: "model-1".to_string(),
                    message: Message::assistant("hi"),
                },
            },
            SessionFactEnvelope {
                revision: 4,
                timestamp_ms: 4,
                operation_id: Some("op-1".to_string()),
                turn_id: Some("turn-1".to_string()),
                fact: SessionFact::TurnCompleted,
            },
            SessionFactEnvelope {
                revision: 5,
                timestamp_ms: 5,
                operation_id: Some("op-2".to_string()),
                turn_id: Some("turn-2".to_string()),
                fact: SessionFact::TurnStarted {
                    user_message: Message::user("unfinished"),
                    model: model(),
                    permissions: PermissionProfile::default(),
                },
            },
            SessionFactEnvelope {
                revision: 6,
                timestamp_ms: 6,
                operation_id: Some("op-2".to_string()),
                turn_id: Some("turn-2".to_string()),
                fact: SessionFact::TurnInterrupted {
                    reason: "restart".to_string(),
                },
            },
            SessionFactEnvelope {
                revision: 7,
                timestamp_ms: 7,
                operation_id: Some("op-3".to_string()),
                turn_id: Some("turn-3".to_string()),
                fact: SessionFact::TurnStarted {
                    user_message: Message::user("failed"),
                    model: model(),
                    permissions: PermissionProfile::default(),
                },
            },
            SessionFactEnvelope {
                revision: 8,
                timestamp_ms: 8,
                operation_id: Some("op-3".to_string()),
                turn_id: Some("turn-3".to_string()),
                fact: SessionFact::TurnFailed {
                    error: "failed".to_string(),
                },
            },
            SessionFactEnvelope {
                revision: 9,
                timestamp_ms: 9,
                operation_id: Some("op-4".to_string()),
                turn_id: Some("turn-4".to_string()),
                fact: SessionFact::TurnStarted {
                    user_message: Message::user("cancelled"),
                    model: model(),
                    permissions: PermissionProfile::default(),
                },
            },
            SessionFactEnvelope {
                revision: 10,
                timestamp_ms: 10,
                operation_id: Some("op-4".to_string()),
                turn_id: Some("turn-4".to_string()),
                fact: SessionFact::TurnCancelled {
                    reason: "cancelled".to_string(),
                },
            },
        ];

        let projection = project_session(&header, &facts).expect("project");

        assert_eq!(
            projection.context.messages,
            vec![Message::user("hello"), Message::assistant("hi")]
        );
        assert_eq!(projection.turns[1].status, SessionTurnStatus::Interrupted);
        assert_eq!(projection.turns[2].status, SessionTurnStatus::Failed);
        assert_eq!(projection.turns[3].status, SessionTurnStatus::Cancelled);
    }

    #[test]
    fn latest_compaction_selects_completed_turns_after_its_boundary() {
        let header = SessionLogHeader {
            schema_version: agent_protocol::SESSION_DOCUMENT_SCHEMA_VERSION,
            session_id: "session-compact".to_string(),
            created_at_ms: 1,
        };
        let mut facts = Vec::new();
        for (index, prompt) in ["old", "new"].into_iter().enumerate() {
            let revision = facts.len() as u64 + 1;
            let operation_id = format!("op-{index}");
            let turn_id = format!("turn-{index}");
            facts.push(SessionFactEnvelope {
                revision,
                timestamp_ms: revision,
                operation_id: Some(operation_id.clone()),
                turn_id: Some(turn_id.clone()),
                fact: SessionFact::TurnStarted {
                    user_message: Message::user(prompt),
                    model: model(),
                    permissions: PermissionProfile::default(),
                },
            });
            let revision = facts.len() as u64 + 1;
            facts.push(SessionFactEnvelope {
                revision,
                timestamp_ms: revision,
                operation_id: Some(operation_id.clone()),
                turn_id: Some(turn_id.clone()),
                fact: SessionFact::ModelCallStarted {
                    model_call_id: format!("model-{index}"),
                },
            });
            let revision = facts.len() as u64 + 1;
            facts.push(SessionFactEnvelope {
                revision,
                timestamp_ms: revision,
                operation_id: Some(operation_id.clone()),
                turn_id: Some(turn_id.clone()),
                fact: SessionFact::ModelMessageCommitted {
                    model_call_id: format!("model-{index}"),
                    message: Message::assistant(format!("answer-{index}")),
                },
            });
            let revision = facts.len() as u64 + 1;
            facts.push(SessionFactEnvelope {
                revision,
                timestamp_ms: revision,
                operation_id: Some(operation_id),
                turn_id: Some(turn_id),
                fact: SessionFact::TurnCompleted,
            });
        }
        let revision = facts.len() as u64 + 1;
        facts.push(SessionFactEnvelope {
            revision,
            timestamp_ms: revision,
            operation_id: None,
            turn_id: None,
            fact: SessionFact::ContextCompacted {
                summary: "old summary".to_string(),
                covered_through_turn_id: "turn-0".to_string(),
            },
        });

        let projection = project_session(&header, &facts).expect("project compacted session");

        assert_eq!(projection.revision, 9);
        assert_eq!(projection.turns.len(), 2);
        assert_eq!(
            projection.context.messages,
            vec![
                Message::system("Session summary:\nold summary"),
                Message::user("new"),
                Message::assistant("answer-1"),
            ]
        );
        assert_eq!(
            project_session(&header, &facts).expect("project deterministically"),
            projection
        );
    }

    #[test]
    fn session_level_middleware_audit_does_not_require_a_turn() {
        let header = SessionLogHeader {
            schema_version: agent_protocol::SESSION_DOCUMENT_SCHEMA_VERSION,
            session_id: "session-middleware".to_string(),
            created_at_ms: 1,
        };
        let invocation = agent_protocol::MiddlewareInvocationFinished {
            invocation_id: "middleware-1".to_string(),
            middleware_id: "policy".to_string(),
            source: agent_protocol::MiddlewareSource::Internal,
            stage: agent_protocol::MiddlewareStage::BeforePrompt,
            outcome: agent_protocol::MiddlewareOutcome::Deny,
            started_at_ms: 1,
            duration_ms: 2,
            reason: Some("blocked".to_string()),
        };
        let facts = vec![SessionFactEnvelope {
            revision: 1,
            timestamp_ms: 3,
            operation_id: None,
            turn_id: None,
            fact: SessionFact::MiddlewareFinished {
                invocation: invocation.clone(),
            },
        }];

        let projection = project_session(&header, &facts).expect("project audit");

        assert!(projection.turns.is_empty());
        assert_eq!(projection.middleware_audit, vec![invocation]);
    }
}

use crate::{SessionStore, SessionStoreError, SessionWriterLease, project_session, timestamp_ms};
use agent_protocol::{
    ApprovalRequest, OperationProjection, PermissionProfile, SESSION_STREAM_SCHEMA_VERSION,
    SessionFact, SessionFactEnvelope, SessionLogHeader, SessionProjection, SessionSnapshot,
    SessionStreamFrame, SessionUpdate, SessionUpdateEnvelope, StreamCursor,
    StreamingMessageProjection, SubagentInstanceSnapshot,
};
use std::sync::{
    Mutex as StdMutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, broadcast};

static HANDLE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct SessionHandle {
    session_name: String,
    store: SessionStore,
    lease: StdMutex<Option<SessionWriterLease>>,
    permissions: PermissionProfile,
    commit_lock: Mutex<()>,
    state: Mutex<SessionHandleState>,
    tx: broadcast::Sender<SessionUpdateEnvelope>,
}

struct SessionHandleState {
    header: SessionLogHeader,
    facts: Vec<SessionFactEnvelope>,
    projection: SessionProjection,
    stream_id: String,
    sequence: u64,
    active_operation: Option<OperationProjection>,
    approvals: Vec<ApprovalRequest>,
    subagents: Vec<SubagentInstanceSnapshot>,
    invalidated: Option<String>,
}

pub struct SessionSubscription {
    pub snapshot: SessionSnapshot,
    receiver: broadcast::Receiver<SessionUpdateEnvelope>,
}

impl SessionSubscription {
    pub async fn recv(&mut self) -> Result<SessionUpdateEnvelope, broadcast::error::RecvError> {
        self.receiver.recv().await
    }
}

impl SessionHandle {
    pub fn open(
        store: SessionStore,
        session_name: impl Into<String>,
        permissions: PermissionProfile,
    ) -> Result<Self, SessionStoreError> {
        let lease = store.acquire_writer()?;
        store.ensure_v5(&lease)?;
        Self::from_open_store(store, session_name.into(), permissions, lease)
    }

    pub fn open_existing(
        store: SessionStore,
        session_name: impl Into<String>,
        permissions: PermissionProfile,
    ) -> Result<Self, SessionStoreError> {
        if !store.has_active_document() {
            return Err(SessionStoreError::SessionNotFound {
                name: session_name.into(),
            });
        }
        let session_name = session_name.into();
        let lease = store.acquire_writer()?;
        store.ensure_v5_existing(&lease)?;
        Self::from_open_store(store, session_name, permissions, lease)
    }

    fn from_open_store(
        store: SessionStore,
        session_name: String,
        permissions: PermissionProfile,
        lease: SessionWriterLease,
    ) -> Result<Self, SessionStoreError> {
        let _ = store.recover_interrupted(&lease)?;
        let (header, facts) = store.load_log()?;
        let projection = project_session(&header, &facts)?;
        let (tx, _) = broadcast::channel(256);
        Ok(Self {
            session_name,
            store,
            lease: StdMutex::new(Some(lease)),
            permissions,
            commit_lock: Mutex::new(()),
            state: Mutex::new(SessionHandleState {
                header,
                facts,
                projection,
                stream_id: next_handle_id("stream"),
                sequence: 0,
                active_operation: None,
                approvals: Vec::new(),
                subagents: Vec::new(),
                invalidated: None,
            }),
            tx,
        })
    }

    pub fn store(&self) -> &SessionStore {
        &self.store
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub async fn projection(&self) -> SessionProjection {
        self.state.lock().await.projection.clone()
    }

    pub async fn hard_reset(&self) -> Result<SessionProjection, SessionStoreError> {
        let _commit = self.commit_lock.lock().await;
        self.ensure_writable().await?;
        let projection = {
            let lease = self.lease.lock().expect("session lease lock poisoned");
            self.store.reset_with_lease(
                lease
                    .as_ref()
                    .expect("writable session handle must retain its lease"),
            )?
        };
        let (header, facts) = self.store.load_log()?;
        let envelope = {
            let mut state = self.state.lock().await;
            state.header = header;
            state.facts = facts;
            state.projection = projection.clone();
            state.stream_id = next_handle_id("stream");
            state.sequence = 0;
            state.active_operation = None;
            state.approvals.clear();
            state.subagents.clear();
            next_envelope(
                &mut state,
                SessionUpdate::ContextReplaced(projection.context.clone()),
            )
        };
        let _ = self.tx.send(envelope);
        Ok(projection)
    }

    pub async fn archive(&self) -> Result<(), SessionStoreError> {
        let _commit = self.commit_lock.lock().await;
        self.ensure_writable().await?;
        let released_lease = {
            let mut lease = self.lease.lock().expect("session lease lock poisoned");
            self.store.archive_with_lease(
                lease
                    .as_ref()
                    .expect("writable session handle must retain its lease"),
            )?;
            lease
                .take()
                .expect("writable session handle must retain its lease")
        };
        let envelope = {
            let mut state = self.state.lock().await;
            let reason = "session was archived; acquire a new snapshot after restore".to_string();
            state.invalidated = Some(reason.clone());
            state.stream_id = next_handle_id("stream");
            state.sequence = 0;
            state.active_operation = None;
            state.approvals.clear();
            next_envelope(&mut state, SessionUpdate::Notice { message: reason })
        };
        let _ = self.tx.send(envelope);
        drop(released_lease);
        Ok(())
    }

    pub async fn revision(&self) -> u64 {
        self.state.lock().await.projection.revision
    }

    pub async fn export_document_bytes(&self) -> Result<Vec<u8>, SessionStoreError> {
        let _commit = self.commit_lock.lock().await;
        self.ensure_writable().await?;
        let lease = self.lease.lock().expect("session lease lock poisoned");
        self.store.export_document_bytes_with_lease(
            lease
                .as_ref()
                .expect("writable session handle must retain its lease"),
        )
    }

    pub async fn subscribe(&self) -> Result<SessionSubscription, SessionStoreError> {
        let state = self.state.lock().await;
        if let Some(reason) = state.invalidated.as_ref() {
            return Err(SessionStoreError::HandleInvalidated {
                name: self.session_name.clone(),
                reason: reason.clone(),
            });
        }
        let receiver = self.tx.subscribe();
        let snapshot = snapshot_from_state(&self.session_name, self.permissions, &state);
        Ok(SessionSubscription { snapshot, receiver })
    }

    pub async fn snapshot(&self) -> SessionSnapshot {
        let state = self.state.lock().await;
        snapshot_from_state(&self.session_name, self.permissions, &state)
    }

    pub async fn begin_operation(
        &self,
        user_message: agent_protocol::Message,
        model: agent_protocol::ModelInvocation,
        permissions: PermissionProfile,
        system_prompt: String,
    ) -> Result<(String, String), SessionStoreError> {
        let _commit = self.commit_lock.lock().await;
        self.ensure_writable().await?;
        {
            let state = self.state.lock().await;
            if state.active_operation.is_some()
                || state
                    .projection
                    .turns
                    .iter()
                    .any(|turn| turn.status == agent_protocol::SessionTurnStatus::Running)
            {
                return Err(SessionStoreError::OperationActive {
                    name: self.session_name.clone(),
                });
            }
        }
        let operation_id = next_handle_id("operation");
        let turn_id = next_handle_id("turn");
        let fact = SessionFact::TurnStarted {
            user_message,
            model,
            permissions,
            system_prompt,
        };
        let expected_revision = self.state.lock().await.projection.revision;
        let envelope = {
            let lease = self.lease.lock().expect("session lease lock poisoned");
            self.store.append_fact(
                lease
                    .as_ref()
                    .expect("writable session handle must retain its lease"),
                expected_revision,
                Some(operation_id.clone()),
                Some(turn_id.clone()),
                fact.clone(),
            )?
        };
        let operation = OperationProjection {
            operation_id: operation_id.clone(),
            turn_id: turn_id.clone(),
            phase: "starting".to_string(),
            streaming: None,
            cancellable: true,
        };
        let envelopes = {
            let mut state = self.state.lock().await;
            state.facts.push(envelope);
            state.projection = project_session(&state.header, &state.facts)?;
            let mut envelopes = updates_for_fact(&state.projection, Some(&turn_id), &fact)
                .into_iter()
                .map(|update| next_envelope(&mut state, update))
                .collect::<Vec<_>>();
            state.active_operation = Some(operation.clone());
            envelopes.push(next_envelope(
                &mut state,
                SessionUpdate::OperationReplaced(Some(operation)),
            ));
            envelopes
        };
        for envelope in envelopes {
            let _ = self.tx.send(envelope);
        }
        Ok((operation_id, turn_id))
    }

    pub async fn commit_fact(
        &self,
        operation_id: Option<String>,
        turn_id: Option<String>,
        fact: SessionFact,
    ) -> Result<SessionFactEnvelope, SessionStoreError> {
        let _commit = self.commit_lock.lock().await;
        self.ensure_writable().await?;
        let expected_revision = self.state.lock().await.projection.revision;
        let envelope = {
            let lease = self.lease.lock().expect("session lease lock poisoned");
            self.store.append_fact(
                lease
                    .as_ref()
                    .expect("writable session handle must retain its lease"),
                expected_revision,
                operation_id,
                turn_id.clone(),
                fact.clone(),
            )?
        };

        let envelopes = {
            let mut state = self.state.lock().await;
            state.facts.push(envelope.clone());
            state.projection = project_session(&state.header, &state.facts)?;
            updates_for_fact(&state.projection, turn_id.as_deref(), &fact)
                .into_iter()
                .map(|update| next_envelope(&mut state, update))
                .collect::<Vec<_>>()
        };
        for event in envelopes {
            let _ = self.tx.send(event);
        }
        Ok(envelope)
    }

    async fn ensure_writable(&self) -> Result<(), SessionStoreError> {
        let state = self.state.lock().await;
        match state.invalidated.as_ref() {
            Some(reason) => Err(SessionStoreError::HandleInvalidated {
                name: self.session_name.clone(),
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    pub async fn replace_operation(&self, operation: Option<OperationProjection>) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            state.active_operation = operation.clone();
            next_envelope(&mut state, SessionUpdate::OperationReplaced(operation))
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn set_operation_phase(&self, phase: impl Into<String>) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            if let Some(operation) = state.active_operation.as_mut() {
                operation.phase = phase.into();
            }
            let operation = state.active_operation.clone();
            next_envelope(&mut state, SessionUpdate::OperationReplaced(operation))
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn append_stream_delta(
        &self,
        operation_id: &str,
        model_call_id: &str,
        text: Option<String>,
        reasoning: Option<String>,
    ) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            let Some(operation) = state.active_operation.as_mut() else {
                return;
            };
            if operation.operation_id != operation_id {
                return;
            }
            let streaming = operation
                .streaming
                .get_or_insert_with(|| StreamingMessageProjection {
                    model_call_id: model_call_id.to_string(),
                    content: String::new(),
                    reasoning: String::new(),
                });
            if streaming.model_call_id != model_call_id {
                *streaming = StreamingMessageProjection {
                    model_call_id: model_call_id.to_string(),
                    content: String::new(),
                    reasoning: String::new(),
                };
            }
            if let Some(text) = text.as_ref() {
                streaming.content.push_str(text);
            }
            if let Some(reasoning) = reasoning.as_ref() {
                streaming.reasoning.push_str(reasoning);
            }
            next_envelope(
                &mut state,
                SessionUpdate::ModelStreamDelta {
                    operation_id: operation_id.to_string(),
                    model_call_id: model_call_id.to_string(),
                    text,
                    reasoning,
                },
            )
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn clear_streaming(&self, phase: impl Into<String>) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            if let Some(operation) = state.active_operation.as_mut() {
                operation.streaming = None;
                operation.phase = phase.into();
            }
            let operation = state.active_operation.clone();
            next_envelope(&mut state, SessionUpdate::OperationReplaced(operation))
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn set_approvals(&self, approvals: Vec<ApprovalRequest>) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            state.approvals = approvals.clone();
            next_envelope(&mut state, SessionUpdate::ApprovalsReplaced(approvals))
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn replace_subagents(&self, subagents: Vec<SubagentInstanceSnapshot>) {
        let envelopes = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            let removed = state
                .subagents
                .iter()
                .filter(|current| !subagents.iter().any(|next| next.id == current.id))
                .map(|current| current.id.clone())
                .collect::<Vec<_>>();
            state.subagents = subagents.clone();
            let mut updates = removed
                .into_iter()
                .map(|instance_id| SessionUpdate::SubagentRemoved { instance_id })
                .collect::<Vec<_>>();
            updates.extend(
                subagents
                    .into_iter()
                    .map(|snapshot| SessionUpdate::SubagentUpserted(Box::new(snapshot))),
            );
            updates
                .into_iter()
                .map(|update| next_envelope(&mut state, update))
                .collect::<Vec<_>>()
        };
        for envelope in envelopes {
            let _ = self.tx.send(envelope);
        }
    }

    pub async fn upsert_subagent(&self, snapshot: SubagentInstanceSnapshot) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            state.subagents.retain(|current| current.id != snapshot.id);
            state.subagents.push(snapshot.clone());
            state
                .subagents
                .sort_by_key(|instance| instance.created_at_ms);
            next_envelope(
                &mut state,
                SessionUpdate::SubagentUpserted(Box::new(snapshot)),
            )
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn remove_subagent(&self, instance_id: impl Into<String>) {
        let instance_id = instance_id.into();
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            state
                .subagents
                .retain(|snapshot| snapshot.id != instance_id);
            next_envelope(&mut state, SessionUpdate::SubagentRemoved { instance_id })
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn notice(&self, message: impl Into<String>) {
        let envelope = {
            let mut state = self.state.lock().await;
            if state.invalidated.is_some() {
                return;
            }
            next_envelope(
                &mut state,
                SessionUpdate::Notice {
                    message: message.into(),
                },
            )
        };
        let _ = self.tx.send(envelope);
    }

    pub async fn snapshot_frame(&self) -> SessionStreamFrame {
        SessionStreamFrame::Snapshot(Box::new(self.snapshot().await))
    }
}

fn next_envelope(state: &mut SessionHandleState, update: SessionUpdate) -> SessionUpdateEnvelope {
    state.sequence += 1;
    SessionUpdateEnvelope {
        schema_version: SESSION_STREAM_SCHEMA_VERSION,
        stream_id: state.stream_id.clone(),
        sequence: state.sequence,
        session_revision: state.projection.revision,
        timestamp_ms: timestamp_ms(),
        update,
    }
}

fn snapshot_from_state(
    session_name: &str,
    permissions: PermissionProfile,
    state: &SessionHandleState,
) -> SessionSnapshot {
    SessionSnapshot {
        schema_version: SESSION_STREAM_SCHEMA_VERSION,
        session_name: session_name.to_string(),
        session_id: state.header.session_id.clone(),
        revision: state.projection.revision,
        cursor: StreamCursor {
            stream_id: state.stream_id.clone(),
            sequence: state.sequence,
        },
        session: state.projection.clone(),
        active_operation: state.active_operation.clone(),
        permissions,
        approvals: state.approvals.clone(),
        subagents: state.subagents.clone(),
    }
}

fn updates_for_fact(
    projection: &SessionProjection,
    turn_id: Option<&str>,
    fact: &SessionFact,
) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if let Some(turn_id) = turn_id
        && let Some(turn) = projection.turns.iter().find(|turn| turn.id == turn_id)
    {
        updates.push(SessionUpdate::TurnUpserted(Box::new(turn.clone())));
    }
    if matches!(
        fact,
        SessionFact::TurnCompleted
            | SessionFact::ContextCompacted { .. }
            | SessionFact::LegacyContextCheckpoint { .. }
    ) {
        updates.push(SessionUpdate::ContextReplaced(projection.context.clone()));
    }
    if let SessionFact::MiddlewareFinished { invocation } = fact {
        updates.push(SessionUpdate::MiddlewareRecorded(invocation.clone()));
    }
    updates
}

fn next_handle_id(prefix: &str) -> String {
    let counter = HANDLE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{:016x}-{counter:04x}", timestamp_ms())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::{Message, ModelInvocation, ReasoningLevel};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn unique_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("morrow-session-handle-{name}-{stamp}"));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    fn model() -> ModelInvocation {
        ModelInvocation {
            provider_id: "test".to_string(),
            provider_name: "Test".to_string(),
            model_id: "model".to_string(),
            model_name: "Model".to_string(),
            reasoning: ReasoningLevel::Off,
        }
    }

    #[tokio::test]
    async fn subscription_snapshot_and_events_share_one_cursor() {
        let root = unique_dir("root");
        let legacy = unique_dir("legacy");
        let workspace = unique_dir("workspace");
        let store = SessionStore::new(&root, &legacy, &workspace, "default").expect("store");
        let handle =
            SessionHandle::open(store, "default", PermissionProfile::default()).expect("handle");
        let mut subscription = handle.subscribe().await.expect("subscribe");
        assert_eq!(subscription.snapshot.cursor.sequence, 0);

        handle
            .begin_operation(
                Message::user("hello"),
                model(),
                PermissionProfile::default(),
                "system".to_string(),
            )
            .await
            .expect("begin");

        let first = subscription.recv().await.expect("first event");
        assert_eq!(first.sequence, 1);
        let second = subscription.recv().await.expect("second event");
        assert_eq!(second.sequence, 2);
        assert_eq!(first.stream_id, subscription.snapshot.cursor.stream_id);
        assert!(matches!(
            handle
                .begin_operation(
                    Message::user("second"),
                    model(),
                    PermissionProfile::default(),
                    "system".to_string(),
                )
                .await,
            Err(SessionStoreError::OperationActive { .. })
        ));
    }

    #[tokio::test]
    async fn archive_invalidates_subscribers_and_releases_writer_lease() {
        let root = unique_dir("archive-root");
        let legacy = unique_dir("archive-legacy");
        let workspace = unique_dir("archive-workspace");
        let store = SessionStore::new(&root, &legacy, &workspace, "default").expect("store");
        let handle = SessionHandle::open(store.clone(), "default", PermissionProfile::default())
            .expect("handle");
        let mut subscription = handle.subscribe().await.expect("subscribe");

        handle.archive().await.expect("archive");

        let event = subscription.recv().await.expect("invalidation event");
        assert_ne!(event.stream_id, subscription.snapshot.cursor.stream_id);
        assert!(matches!(event.update, SessionUpdate::Notice { .. }));
        assert!(matches!(
            handle
                .begin_operation(
                    Message::user("too late"),
                    model(),
                    PermissionProfile::default(),
                    "system".to_string(),
                )
                .await,
            Err(SessionStoreError::HandleInvalidated { .. })
        ));
        store
            .restore()
            .expect("restore while old subscriber still exists");
    }
}

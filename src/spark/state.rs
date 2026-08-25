//! Crash-safe Spark control state owned by one bounded database actor.

use std::{
    collections::BTreeMap,
    fs,
    num::NonZeroUsize,
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{mpsc as std_mpsc, Arc},
    thread,
    time::Duration,
};

use hmac::{Hmac, Mac};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use rusqlite_migration::{Migrations, M};
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};

use super::reconcile::{RESTART_FAILURE_LIMIT, RESTART_WINDOW_SECONDS};
use super::resources::{DeclaredEnvelope, EmergencyRecord};
use super::wire::{
    CompatibilityEvaluationDocument, InstanceDesiredState, InstanceDocument, InstanceObservedState,
    ModelDocument, OperationDocument, OperationEvent, OperationProgress, OperationState,
    ProblemDocument, TokenCreateRequest, TokenDocument, INSTANCE_SCHEMA, OPERATION_EVENT_SCHEMA,
    OPERATION_SCHEMA, PROBLEM_SCHEMA, TOKEN_SCHEMA,
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_ACTIVE_TOKENS: i64 = 1024;
const MIGRATION_1: &str = r#"
CREATE TABLE models (id TEXT PRIMARY KEY, repository TEXT NOT NULL, commit_sha TEXT NOT NULL, metadata_json TEXT NOT NULL);
CREATE TABLE aliases (name TEXT PRIMARY KEY, model_id TEXT NOT NULL REFERENCES models(id));
CREATE TABLE instances (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, model_id TEXT NOT NULL REFERENCES models(id), generation INTEGER NOT NULL, desired_state TEXT NOT NULL, observed_state TEXT NOT NULL, metadata_json TEXT NOT NULL);
CREATE TABLE operations (id TEXT PRIMARY KEY, kind TEXT NOT NULL, actor_token_id TEXT NOT NULL, target TEXT, state TEXT NOT NULL, progress_json TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, result_json TEXT, problem_json TEXT);
CREATE TABLE operation_events (operation_id TEXT NOT NULL REFERENCES operations(id), sequence INTEGER NOT NULL, state TEXT NOT NULL, progress_json TEXT NOT NULL, occurred_at TEXT NOT NULL, PRIMARY KEY(operation_id, sequence));
CREATE TABLE idempotency (token_id TEXT NOT NULL, operation_kind TEXT NOT NULL, key TEXT NOT NULL, request_sha256 TEXT NOT NULL, operation_id TEXT NOT NULL REFERENCES operations(id), expires_at TEXT NOT NULL, PRIMARY KEY(token_id, operation_kind, key));
CREATE TABLE benchmarks (id TEXT PRIMARY KEY, model_id TEXT NOT NULL, metadata_json TEXT NOT NULL);
CREATE TABLE token_metadata (id TEXT PRIMARY KEY, name TEXT NOT NULL, verifier BLOB NOT NULL, scopes_json TEXT NOT NULL, allowed_cidrs_json TEXT NOT NULL, expires_at TEXT, max_concurrent_inference INTEGER NOT NULL, created_at TEXT NOT NULL, last_used_at TEXT, revoked_at TEXT);
CREATE TABLE audit (sequence INTEGER PRIMARY KEY AUTOINCREMENT, occurred_at TEXT NOT NULL, actor_token_id TEXT NOT NULL, action TEXT NOT NULL, target TEXT, outcome TEXT NOT NULL, metadata_json TEXT NOT NULL);
"#;
const MIGRATION_2: &str = r#"
CREATE INDEX operations_updated_at_idx ON operations(updated_at DESC);
CREATE INDEX operation_events_lookup_idx ON operation_events(operation_id, sequence);
CREATE INDEX token_metadata_active_idx ON token_metadata(revoked_at, expires_at);
CREATE INDEX audit_occurred_at_idx ON audit(occurred_at DESC);
"#;
const MIGRATION_3: &str = r#"
CREATE TABLE instance_resources (
    instance_id TEXT PRIMARY KEY REFERENCES instances(id) ON DELETE CASCADE,
    cold_start_peak_bytes INTEGER NOT NULL CHECK(cold_start_peak_bytes >= 0),
    steady_peak_bytes INTEGER NOT NULL CHECK(steady_peak_bytes >= 0),
    incremental_start_peak_bytes INTEGER NOT NULL CHECK(incremental_start_peak_bytes >= 0),
    phase TEXT NOT NULL,
    started_sequence INTEGER NOT NULL CHECK(started_sequence >= 0),
    current_memory_bytes INTEGER NOT NULL CHECK(current_memory_bytes >= 0),
    previous_memory_bytes INTEGER NOT NULL CHECK(previous_memory_bytes >= 0),
    restart_suppressed INTEGER NOT NULL DEFAULT 0 CHECK(restart_suppressed IN (0,1)),
    suppression_cause TEXT
);
CREATE TABLE transition_leases (
    operation_id TEXT PRIMARY KEY,
    acquired_at TEXT NOT NULL
);
CREATE TABLE emergency_records (
    event_id TEXT PRIMARY KEY,
    instance_id TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation >= 0),
    cause TEXT NOT NULL,
    evidence_json TEXT NOT NULL,
    imported_at TEXT NOT NULL
);
"#;
const MIGRATION_4: &str = r#"
CREATE INDEX instances_desired_observed_idx ON instances(desired_state, observed_state);
"#;
const MIGRATION_5: &str = r#"
CREATE TABLE restart_failures (
    instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK(generation > 0),
    failed_at_unix_seconds INTEGER NOT NULL CHECK(failed_at_unix_seconds >= 0),
    PRIMARY KEY(instance_id, generation, failed_at_unix_seconds)
);
CREATE INDEX restart_failures_window_idx ON restart_failures(instance_id, generation, failed_at_unix_seconds);
CREATE TABLE quarantine_evidence (
    container_id TEXT PRIMARY KEY,
    instance_id TEXT,
    generation INTEGER,
    cause TEXT NOT NULL,
    observed_at TEXT NOT NULL
);
"#;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    Overloaded,
    Conflict(String),
    NotFound,
    Invalid(String),
    Unavailable(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Overloaded => formatter.write_str("Spark state queue is saturated"),
            Self::Conflict(message) | Self::Invalid(message) | Self::Unavailable(message) => {
                formatter.write_str(message)
            }
            Self::NotFound => formatter.write_str("Spark resource was not found"),
        }
    }
}

impl std::error::Error for StateError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseHealth {
    pub journal_mode: String,
    pub synchronous: String,
    pub foreign_keys: bool,
    pub backup_valid: bool,
    pub queue_capacity: usize,
}

#[derive(Clone)]
pub struct TokenVerifier {
    pub token: TokenDocument,
    verifier: [u8; 32],
}

impl TokenVerifier {
    pub fn verify(&self, pepper: &SecretString, secret: &str) -> bool {
        let Ok(mut mac) = HmacSha256::new_from_slice(pepper.expose_secret().as_bytes()) else {
            return false;
        };
        mac.update(self.token.id.as_bytes());
        mac.update(secret.as_bytes());
        mac.verify_slice(&self.verifier).is_ok()
    }
}

#[derive(Clone, Default)]
pub struct AuthSnapshot {
    pub tokens: BTreeMap<String, TokenVerifier>,
}

#[derive(Debug, Clone)]
pub struct AcceptedOperation {
    pub operation: OperationDocument,
    pub reused: bool,
}

#[derive(Debug, Clone)]
pub struct CreatedToken {
    pub operation: OperationDocument,
    pub token: TokenDocument,
    pub bearer_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AcceptedInstance {
    pub instance: InstanceDocument,
    pub reused: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEvidence {
    pub container_id: String,
    pub instance_id: Option<String>,
    pub generation: Option<u64>,
    pub cause: String,
}

enum Command {
    Health(oneshot::Sender<Result<DatabaseHealth, StateError>>),
    Accept {
        token_id: String,
        kind: String,
        key: String,
        request_sha256: String,
        target: Option<String>,
        reply: oneshot::Sender<Result<AcceptedOperation, StateError>>,
    },
    ListOperations(oneshot::Sender<Result<Vec<OperationDocument>, StateError>>),
    GetOperation {
        id: String,
        reply: oneshot::Sender<Result<OperationDocument, StateError>>,
    },
    Events {
        id: String,
        after: u64,
        reply: oneshot::Sender<Result<Vec<OperationEvent>, StateError>>,
    },
    Transition {
        id: String,
        state: OperationState,
        progress: OperationProgress,
        result: Option<serde_json::Value>,
        problem: Option<ProblemDocument>,
        reply: oneshot::Sender<Result<OperationDocument, StateError>>,
    },
    Cancel {
        id: String,
        actor: String,
        reply: oneshot::Sender<Result<OperationDocument, StateError>>,
    },
    CreateToken {
        actor: String,
        key: String,
        request: TokenCreateRequest,
        reply: oneshot::Sender<Result<CreatedToken, StateError>>,
    },
    ListTokens(oneshot::Sender<Result<Vec<TokenDocument>, StateError>>),
    RevokeToken {
        actor: String,
        key: String,
        token_id: String,
        reply: oneshot::Sender<Result<OperationDocument, StateError>>,
    },
    ListModels(oneshot::Sender<Result<Vec<ModelDocument>, StateError>>),
    GetModel {
        reference: String,
        reply: oneshot::Sender<Result<ModelDocument, StateError>>,
    },
    PromoteModel {
        model: ModelDocument,
        update_alias: bool,
        reply: oneshot::Sender<Result<ModelDocument, StateError>>,
    },
    RemoveModel {
        id: String,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    BeginServe {
        instance: InstanceDocument,
        reply: oneshot::Sender<Result<AcceptedInstance, StateError>>,
    },
    ListInstances(oneshot::Sender<Result<Vec<InstanceDocument>, StateError>>),
    GetInstance {
        reference: String,
        reply: oneshot::Sender<Result<InstanceDocument, StateError>>,
    },
    SetInstanceObserved {
        id: String,
        generation: u64,
        observed: InstanceObservedState,
        endpoint: Option<String>,
        failure: Option<String>,
        startup_milliseconds: Option<u64>,
        reply: oneshot::Sender<Result<InstanceDocument, StateError>>,
    },
    BeginStop {
        reference: String,
        reply: oneshot::Sender<Result<InstanceDocument, StateError>>,
    },
    RecordRestartFailure {
        id: String,
        generation: u64,
        failed_at_unix_seconds: u64,
        reply: oneshot::Sender<Result<InstanceDocument, StateError>>,
    },
    MarkQuarantine {
        id: String,
        generation: u64,
        cause: String,
        reply: oneshot::Sender<Result<InstanceDocument, StateError>>,
    },
    RecordQuarantine {
        evidence: QuarantineEvidence,
        reply: oneshot::Sender<Result<(), StateError>>,
    },
    ListQuarantine(oneshot::Sender<Result<Vec<QuarantineEvidence>, StateError>>),
    DesiredResourceEnvelopes(oneshot::Sender<Result<Vec<DeclaredEnvelope>, StateError>>),
    ImportEmergency {
        record: EmergencyRecord,
        reply: oneshot::Sender<Result<bool, StateError>>,
    },
    StoreEvaluation {
        evaluation: CompatibilityEvaluationDocument,
        reply: oneshot::Sender<Result<CompatibilityEvaluationDocument, StateError>>,
    },
    SelectedEvaluation {
        model_id: String,
        objective: String,
        reply: oneshot::Sender<Result<Option<CompatibilityEvaluationDocument>, StateError>>,
    },
    Snapshot(oneshot::Sender<Result<AuthSnapshot, StateError>>),
    Shutdown(std_mpsc::SyncSender<()>),
}

#[derive(Clone)]
pub struct DbActor {
    sender: mpsc::Sender<Command>,
    queue_capacity: usize,
    pepper: Arc<SecretString>,
}

impl DbActor {
    pub fn open(
        path: impl AsRef<Path>,
        backup_dir: impl AsRef<Path>,
        queue_capacity: usize,
        max_backups: usize,
        pepper: SecretString,
    ) -> Result<Self, StateError> {
        let capacity = NonZeroUsize::new(queue_capacity).ok_or_else(|| {
            StateError::Invalid("database queue capacity must be positive".into())
        })?;
        let max_backups = NonZeroUsize::new(max_backups).ok_or_else(|| {
            StateError::Invalid("database backup retention must be positive".into())
        })?;
        let (sender, receiver) = mpsc::channel(capacity.get());
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let path = path.as_ref().to_owned();
        let backup_dir = backup_dir.as_ref().to_owned();
        let actor_pepper = pepper.clone();
        thread::Builder::new()
            .name("sy-spark-db".into())
            .spawn(
                move || match open_connection(&path, &backup_dir, max_backups.get()) {
                    Ok(connection) => {
                        let _ = ready_tx.send(Ok(()));
                        actor_loop(connection, receiver, actor_pepper);
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                },
            )
            .map_err(|error| StateError::Unavailable(format!("start database actor: {error}")))?;
        ready_rx.recv().map_err(|_| {
            StateError::Unavailable("database actor exited during startup".into())
        })??;
        Ok(Self {
            sender,
            queue_capacity: capacity.get(),
            pepper: Arc::new(pepper),
        })
    }

    pub fn pepper(&self) -> Arc<SecretString> {
        Arc::clone(&self.pepper)
    }

    async fn submit<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, StateError>>) -> Command,
    ) -> Result<T, StateError> {
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(build(reply))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => StateError::Overloaded,
                mpsc::error::TrySendError::Closed(_) => {
                    StateError::Unavailable("database actor is unavailable".into())
                }
            })?;
        receive
            .await
            .map_err(|_| StateError::Unavailable("database actor dropped its reply".into()))?
    }

    pub async fn health(&self) -> Result<DatabaseHealth, StateError> {
        let mut health = self.submit(Command::Health).await?;
        health.queue_capacity = self.queue_capacity;
        Ok(health)
    }

    pub async fn accept_operation(
        &self,
        token_id: &str,
        kind: &str,
        key: &str,
        request_sha256: &str,
        target: Option<String>,
    ) -> Result<AcceptedOperation, StateError> {
        validate_idempotency(key, request_sha256)?;
        self.submit(|reply| Command::Accept {
            token_id: token_id.into(),
            kind: kind.into(),
            key: key.into(),
            request_sha256: request_sha256.into(),
            target,
            reply,
        })
        .await
    }

    pub async fn list_operations(&self) -> Result<Vec<OperationDocument>, StateError> {
        self.submit(Command::ListOperations).await
    }

    pub async fn operation(&self, id: &str) -> Result<OperationDocument, StateError> {
        self.submit(|reply| Command::GetOperation {
            id: id.into(),
            reply,
        })
        .await
    }

    pub async fn events(&self, id: &str, after: u64) -> Result<Vec<OperationEvent>, StateError> {
        self.submit(|reply| Command::Events {
            id: id.into(),
            after,
            reply,
        })
        .await
    }

    pub async fn transition(
        &self,
        id: &str,
        state: OperationState,
        progress: OperationProgress,
        result: Option<serde_json::Value>,
        problem: Option<ProblemDocument>,
    ) -> Result<OperationDocument, StateError> {
        self.submit(|reply| Command::Transition {
            id: id.into(),
            state,
            progress,
            result,
            problem,
            reply,
        })
        .await
    }

    pub async fn cancel(&self, id: &str, actor: &str) -> Result<OperationDocument, StateError> {
        self.submit(|reply| Command::Cancel {
            id: id.into(),
            actor: actor.into(),
            reply,
        })
        .await
    }

    pub async fn create_token(
        &self,
        actor: &str,
        key: &str,
        request: TokenCreateRequest,
    ) -> Result<CreatedToken, StateError> {
        validate_idempotency(
            key,
            &super::wire::canonical_request_sha256(&request)
                .map_err(|error| StateError::Invalid(format!("encode token request: {error}")))?,
        )?;
        validate_token_request(&request)?;
        self.submit(|reply| Command::CreateToken {
            actor: actor.into(),
            key: key.into(),
            request,
            reply,
        })
        .await
    }

    pub async fn list_tokens(&self) -> Result<Vec<TokenDocument>, StateError> {
        self.submit(Command::ListTokens).await
    }

    pub async fn list_models(&self) -> Result<Vec<ModelDocument>, StateError> {
        self.submit(Command::ListModels).await
    }

    pub async fn model(&self, reference: &str) -> Result<ModelDocument, StateError> {
        self.submit(|reply| Command::GetModel {
            reference: reference.into(),
            reply,
        })
        .await
    }

    pub async fn promote_model(
        &self,
        model: ModelDocument,
        update_alias: bool,
    ) -> Result<ModelDocument, StateError> {
        self.submit(|reply| Command::PromoteModel {
            model,
            update_alias,
            reply,
        })
        .await
    }

    pub async fn remove_model(&self, id: &str) -> Result<(), StateError> {
        self.submit(|reply| Command::RemoveModel {
            id: id.into(),
            reply,
        })
        .await
    }

    pub async fn begin_serve(
        &self,
        instance: InstanceDocument,
    ) -> Result<AcceptedInstance, StateError> {
        validate_instance(&instance)?;
        self.submit(|reply| Command::BeginServe { instance, reply })
            .await
    }

    pub async fn list_instances(&self) -> Result<Vec<InstanceDocument>, StateError> {
        self.submit(Command::ListInstances).await
    }

    pub async fn instance(&self, reference: &str) -> Result<InstanceDocument, StateError> {
        self.submit(|reply| Command::GetInstance {
            reference: reference.into(),
            reply,
        })
        .await
    }

    pub async fn set_instance_observed(
        &self,
        id: &str,
        generation: u64,
        observed: InstanceObservedState,
        endpoint: Option<String>,
        failure: Option<String>,
        startup_milliseconds: Option<u64>,
    ) -> Result<InstanceDocument, StateError> {
        self.submit(|reply| Command::SetInstanceObserved {
            id: id.into(),
            generation,
            observed,
            endpoint,
            failure,
            startup_milliseconds,
            reply,
        })
        .await
    }

    pub async fn begin_stop(&self, reference: &str) -> Result<InstanceDocument, StateError> {
        self.submit(|reply| Command::BeginStop {
            reference: reference.into(),
            reply,
        })
        .await
    }

    pub async fn record_restart_failure(
        &self,
        id: &str,
        generation: u64,
        failed_at_unix_seconds: u64,
    ) -> Result<InstanceDocument, StateError> {
        self.submit(|reply| Command::RecordRestartFailure {
            id: id.into(),
            generation,
            failed_at_unix_seconds,
            reply,
        })
        .await
    }

    pub async fn mark_quarantine(
        &self,
        id: &str,
        generation: u64,
        cause: &str,
    ) -> Result<InstanceDocument, StateError> {
        self.submit(|reply| Command::MarkQuarantine {
            id: id.into(),
            generation,
            cause: cause.into(),
            reply,
        })
        .await
    }

    pub async fn record_quarantine(&self, evidence: QuarantineEvidence) -> Result<(), StateError> {
        self.submit(|reply| Command::RecordQuarantine { evidence, reply })
            .await
    }

    pub async fn list_quarantine(&self) -> Result<Vec<QuarantineEvidence>, StateError> {
        self.submit(Command::ListQuarantine).await
    }

    pub async fn desired_resource_envelopes(&self) -> Result<Vec<DeclaredEnvelope>, StateError> {
        self.submit(Command::DesiredResourceEnvelopes).await
    }

    pub async fn import_emergency(&self, record: EmergencyRecord) -> Result<bool, StateError> {
        self.submit(|reply| Command::ImportEmergency { record, reply })
            .await
    }

    pub async fn revoke_token(
        &self,
        actor: &str,
        key: &str,
        token_id: &str,
    ) -> Result<OperationDocument, StateError> {
        validate_idempotency(key, &format!("{:x}", Sha256::digest(token_id.as_bytes())))?;
        self.submit(|reply| Command::RevokeToken {
            actor: actor.into(),
            key: key.into(),
            token_id: token_id.into(),
            reply,
        })
        .await
    }

    pub async fn auth_snapshot(&self) -> Result<AuthSnapshot, StateError> {
        self.submit(Command::Snapshot).await
    }

    pub async fn store_evaluation(
        &self,
        evaluation: CompatibilityEvaluationDocument,
    ) -> Result<CompatibilityEvaluationDocument, StateError> {
        self.submit(|reply| Command::StoreEvaluation { evaluation, reply })
            .await
    }

    pub async fn selected_evaluation(
        &self,
        model_id: &str,
        objective: &str,
    ) -> Result<Option<CompatibilityEvaluationDocument>, StateError> {
        self.submit(|reply| Command::SelectedEvaluation {
            model_id: model_id.into(),
            objective: objective.into(),
            reply,
        })
        .await
    }

    pub fn shutdown(&self) -> Result<(), StateError> {
        let (done, receive) = std_mpsc::sync_channel(1);
        self.sender
            .try_send(Command::Shutdown(done))
            .map_err(|_| StateError::Unavailable("database actor cannot accept shutdown".into()))?;
        receive
            .recv_timeout(BUSY_TIMEOUT)
            .map_err(|_| StateError::Unavailable("database actor did not shut down".into()))
    }
}

fn open_connection(
    path: &Path,
    backup_dir: &Path,
    max_backups: usize,
) -> Result<Connection, StateError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(state_io)?;
    }
    if !path.exists() {
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(state_io)?;
    }
    let mut connection = Connection::open(path).map_err(state_sql)?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(state_sql)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(state_sql)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(state_sql)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(state_sql)?;
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(state_sql)?;
    if version > 0 && version < 4 {
        verified_backup(&connection, backup_dir, max_backups)?;
    }
    let migrations = Migrations::new(vec![
        M::up(MIGRATION_1),
        M::up(MIGRATION_2),
        M::up(MIGRATION_3),
        M::up(MIGRATION_4),
        M::up(MIGRATION_5),
    ]);
    migrations.validate().map_err(state_migration)?;
    migrations
        .to_latest(&mut connection)
        .map_err(state_migration)?;
    verified_backup(&connection, backup_dir, max_backups)?;
    Ok(connection)
}

fn verified_backup(
    connection: &Connection,
    directory: &Path,
    max_backups: usize,
) -> Result<(), StateError> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(directory).map_err(state_io)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).map_err(state_io)?;
    let path = directory.join(format!(
        "state-{}-{}.sqlite3",
        chrono::Utc::now().format("%Y%m%d"),
        ulid::Ulid::new()
    ));
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(state_io)?;
    let mut destination = Connection::open(&path).map_err(state_sql)?;
    let backup = rusqlite::backup::Backup::new(connection, &mut destination).map_err(state_sql)?;
    backup
        .run_to_completion(32, Duration::from_millis(10), None)
        .map_err(state_sql)?;
    drop(backup);
    let integrity: String = destination
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(state_sql)?;
    if integrity != "ok" {
        return Err(StateError::Unavailable(
            "database backup verification failed".into(),
        ));
    }
    destination.close().map_err(|(_, error)| state_sql(error))?;
    fs::OpenOptions::new()
        .read(true)
        .open(&path)
        .and_then(|file| file.sync_all())
        .map_err(state_io)?;
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(state_io)?;
    let mut backups = fs::read_dir(directory)
        .map_err(state_io)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "sqlite3")
        })
        .collect::<Vec<_>>();
    backups.sort();
    let remove_count = backups.len().saturating_sub(max_backups);
    for expired in backups.into_iter().take(remove_count) {
        fs::remove_file(expired).map_err(state_io)?;
    }
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(state_io)
}

fn actor_loop(
    mut connection: Connection,
    mut receiver: mpsc::Receiver<Command>,
    pepper: SecretString,
) {
    while let Some(command) = receiver.blocking_recv() {
        match command {
            Command::Health(reply) => {
                let _ = reply.send(database_health(&connection));
            }
            Command::Accept {
                token_id,
                kind,
                key,
                request_sha256,
                target,
                reply,
            } => {
                let _ = reply.send(accept_operation(
                    &mut connection,
                    &token_id,
                    &kind,
                    &key,
                    &request_sha256,
                    target,
                ));
            }
            Command::ListOperations(reply) => {
                let _ = reply.send(list_operations(&connection));
            }
            Command::GetOperation { id, reply } => {
                let _ = reply.send(read_operation(&connection, &id));
            }
            Command::Events { id, after, reply } => {
                let _ = reply.send(read_events(&connection, &id, after));
            }
            Command::Transition {
                id,
                state,
                progress,
                result,
                problem,
                reply,
            } => {
                let _ = reply.send(transition(
                    &mut connection,
                    &id,
                    state,
                    progress,
                    result,
                    problem,
                ));
            }
            Command::Cancel { id, actor, reply } => {
                let _ = reply.send(cancel_operation(&mut connection, &id, &actor));
            }
            Command::CreateToken {
                actor,
                key,
                request,
                reply,
            } => {
                let _ = reply.send(create_token(
                    &mut connection,
                    &pepper,
                    &actor,
                    &key,
                    request,
                ));
            }
            Command::ListTokens(reply) => {
                let _ = reply.send(list_tokens(&connection));
            }
            Command::RevokeToken {
                actor,
                key,
                token_id,
                reply,
            } => {
                let _ = reply.send(revoke_token(&mut connection, &actor, &key, &token_id));
            }
            Command::Snapshot(reply) => {
                let _ = reply.send(load_snapshot(&connection));
            }
            Command::ListModels(reply) => {
                let _ = reply.send(list_models(&connection));
            }
            Command::GetModel { reference, reply } => {
                let _ = reply.send(read_model(&connection, &reference));
            }
            Command::PromoteModel {
                model,
                update_alias,
                reply,
            } => {
                let _ = reply.send(promote_model(&mut connection, model, update_alias));
            }
            Command::RemoveModel { id, reply } => {
                let _ = reply.send(remove_model(&mut connection, &id));
            }
            Command::BeginServe { instance, reply } => {
                let _ = reply.send(begin_serve(&mut connection, instance));
            }
            Command::ListInstances(reply) => {
                let _ = reply.send(list_instances(&connection));
            }
            Command::GetInstance { reference, reply } => {
                let _ = reply.send(read_instance(&connection, &reference));
            }
            Command::SetInstanceObserved {
                id,
                generation,
                observed,
                endpoint,
                failure,
                startup_milliseconds,
                reply,
            } => {
                let _ = reply.send(set_instance_observed(
                    &mut connection,
                    &id,
                    generation,
                    observed,
                    endpoint,
                    failure,
                    startup_milliseconds,
                ));
            }
            Command::BeginStop { reference, reply } => {
                let _ = reply.send(begin_stop(&mut connection, &reference));
            }
            Command::RecordRestartFailure {
                id,
                generation,
                failed_at_unix_seconds,
                reply,
            } => {
                let _ = reply.send(record_restart_failure(
                    &mut connection,
                    &id,
                    generation,
                    failed_at_unix_seconds,
                ));
            }
            Command::MarkQuarantine {
                id,
                generation,
                cause,
                reply,
            } => {
                let _ = reply.send(mark_quarantine(&mut connection, &id, generation, &cause));
            }
            Command::RecordQuarantine { evidence, reply } => {
                let _ = reply.send(record_quarantine(&mut connection, &evidence));
            }
            Command::ListQuarantine(reply) => {
                let _ = reply.send(list_quarantine(&connection));
            }
            Command::DesiredResourceEnvelopes(reply) => {
                let _ = reply.send(desired_resource_envelopes(&connection));
            }
            Command::ImportEmergency { record, reply } => {
                let _ = reply.send(import_emergency(&mut connection, &record));
            }
            Command::StoreEvaluation { evaluation, reply } => {
                let _ = reply.send(store_evaluation(&mut connection, evaluation));
            }
            Command::SelectedEvaluation {
                model_id,
                objective,
                reply,
            } => {
                let _ = reply.send(selected_evaluation(&connection, &model_id, &objective));
            }
            Command::Shutdown(done) => {
                let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
                let _ = done.send(());
                break;
            }
        }
    }
}

fn store_evaluation(
    connection: &mut Connection,
    evaluation: CompatibilityEvaluationDocument,
) -> Result<CompatibilityEvaluationDocument, StateError> {
    connection.execute(
        "INSERT INTO benchmarks(id,model_id,metadata_json) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET metadata_json=excluded.metadata_json",
        params![evaluation.id, evaluation.model_id, json(&evaluation)?],
    ).map_err(state_sql)?;
    Ok(evaluation)
}

fn selected_evaluation(
    connection: &Connection,
    model_id: &str,
    objective: &str,
) -> Result<Option<CompatibilityEvaluationDocument>, StateError> {
    let mut statement = connection
        .prepare("SELECT metadata_json FROM benchmarks WHERE model_id=?1 ORDER BY rowid DESC")
        .map_err(state_sql)?;
    let mut rows = statement.query(params![model_id]).map_err(state_sql)?;
    while let Some(row) = rows.next().map_err(state_sql)? {
        let raw: String = row.get(0).map_err(state_sql)?;
        let evaluation: CompatibilityEvaluationDocument =
            serde_json::from_str(&raw).map_err(|error| {
                StateError::Unavailable(format!("decode compatibility evidence: {error}"))
            })?;
        if evaluation.objective == objective
            && evaluation.selected_recipe_id.is_some()
            && evaluation.invalidated_reason.is_none()
        {
            return Ok(Some(evaluation));
        }
    }
    Ok(None)
}

fn database_health(connection: &Connection) -> Result<DatabaseHealth, StateError> {
    let journal_mode = connection
        .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
        .map_err(state_sql)?;
    let synchronous = connection
        .pragma_query_value(None, "synchronous", |row| row.get::<_, u8>(0))
        .map_err(state_sql)?;
    let foreign_keys = connection
        .pragma_query_value(None, "foreign_keys", |row| row.get::<_, bool>(0))
        .map_err(state_sql)?;
    Ok(DatabaseHealth {
        journal_mode,
        synchronous: match synchronous {
            2 => "FULL",
            3 => "EXTRA",
            _ => "UNSAFE",
        }
        .into(),
        foreign_keys,
        backup_valid: true,
        queue_capacity: 0,
    })
}

fn accept_operation(
    connection: &mut Connection,
    token_id: &str,
    kind: &str,
    key: &str,
    request_sha256: &str,
    target: Option<String>,
) -> Result<AcceptedOperation, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let prior: Option<(String, String)> = transaction
        .query_row(
            "SELECT request_sha256, operation_id FROM idempotency WHERE token_id=?1 AND operation_kind=?2 AND key=?3",
            params![token_id, kind, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(state_sql)?;
    if let Some((prior_hash, operation_id)) = prior {
        if prior_hash != request_sha256 {
            return Err(StateError::Conflict(
                "idempotency key was reused with a changed request".into(),
            ));
        }
        let operation = read_operation_tx(&transaction, &operation_id)?;
        transaction.commit().map_err(state_sql)?;
        return Ok(AcceptedOperation {
            operation,
            reused: true,
        });
    }
    let id = ulid::Ulid::new().to_string();
    let now = now();
    let progress = OperationProgress {
        stage: "accepted".into(),
        current: None,
        total: None,
        unit: None,
        message: "operation durably accepted".into(),
    };
    transaction.execute(
        "INSERT INTO operations(id,kind,actor_token_id,target,state,progress_json,created_at,updated_at) VALUES(?1,?2,?3,?4,'accepted',?5,?6,?6)",
        params![id, kind, token_id, target, json(&progress)?, now],
    ).map_err(state_sql)?;
    transaction.execute(
        "INSERT INTO idempotency(token_id,operation_kind,key,request_sha256,operation_id,expires_at) VALUES(?1,?2,?3,?4,?5,datetime('now','+90 days'))",
        params![token_id, kind, key, request_sha256, id],
    ).map_err(state_sql)?;
    append_event(&transaction, &id, OperationState::Accepted, &progress, &now)?;
    append_audit(
        &transaction,
        token_id,
        "operation.accept",
        Some(&id),
        "accepted",
        serde_json::json!({"kind":kind}),
    )?;
    let operation = read_operation_tx(&transaction, &id)?;
    transaction.commit().map_err(state_sql)?;
    Ok(AcceptedOperation {
        operation,
        reused: false,
    })
}

fn transition(
    connection: &mut Connection,
    id: &str,
    next: OperationState,
    progress: OperationProgress,
    result: Option<serde_json::Value>,
    problem: Option<ProblemDocument>,
) -> Result<OperationDocument, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let current = read_operation_tx(&transaction, id)?;
    if current.state == next && current.state.is_terminal() {
        transaction.commit().map_err(state_sql)?;
        return Ok(current);
    }
    let legal = legal_operation_transition(current.state, next);
    if !legal {
        return Err(StateError::Conflict(format!(
            "illegal operation transition from {:?} to {:?}",
            current.state, next
        )));
    }
    let updated = now();
    transaction.execute(
        "UPDATE operations SET state=?2, progress_json=?3, updated_at=?4, result_json=?5, problem_json=?6 WHERE id=?1",
        params![id, state_text(next), json(&progress)?, updated, optional_json(result.as_ref())?, optional_json(problem.as_ref())?],
    ).map_err(state_sql)?;
    append_event(&transaction, id, next, &progress, &updated)?;
    let operation = read_operation_tx(&transaction, id)?;
    transaction.commit().map_err(state_sql)?;
    Ok(operation)
}

fn legal_operation_transition(current: OperationState, next: OperationState) -> bool {
    matches!(
        (current, next),
        (OperationState::Accepted, OperationState::Running)
            | (OperationState::Running, OperationState::Running)
            | (OperationState::Accepted, OperationState::Cancelled)
            | (OperationState::Running, OperationState::Succeeded)
            | (OperationState::Running, OperationState::Failed)
            | (OperationState::Running, OperationState::Cancelled)
    )
}

fn cancel_operation(
    connection: &mut Connection,
    id: &str,
    actor: &str,
) -> Result<OperationDocument, StateError> {
    let current = read_operation(connection, id)?;
    if current.state == OperationState::Cancelled {
        return Ok(current);
    }
    if current.state.is_terminal() {
        return Err(StateError::Conflict(
            "terminal operations are immutable; use the domain stop command when applicable".into(),
        ));
    }
    let operation = transition(
        connection,
        id,
        OperationState::Cancelled,
        OperationProgress {
            stage: "cancelled".into(),
            current: None,
            total: None,
            unit: None,
            message: "cancellation committed".into(),
        },
        None,
        None,
    )?;
    connection.execute(
        "INSERT INTO audit(occurred_at,actor_token_id,action,target,outcome,metadata_json) VALUES(?1,?2,'operation.cancel',?3,'cancelled','{}')",
        params![now(), actor, id],
    ).map_err(state_sql)?;
    Ok(operation)
}

fn create_token(
    connection: &mut Connection,
    pepper: &SecretString,
    actor: &str,
    key: &str,
    request: TokenCreateRequest,
) -> Result<CreatedToken, StateError> {
    let request_hash = super::wire::canonical_request_sha256(&request)
        .map_err(|error| StateError::Invalid(format!("encode token request: {error}")))?;
    let accepted = accept_operation(connection, actor, "token.create", key, &request_hash, None)?;
    if accepted.reused {
        let token_id = accepted
            .operation
            .result
            .as_ref()
            .and_then(|value| value.get("token_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                StateError::Conflict(
                    "created token secret is no longer retrievable; use a new idempotency key"
                        .into(),
                )
            })?
            .to_owned();
        return Ok(CreatedToken {
            operation: accepted.operation,
            token: read_token(connection, &token_id)?,
            bearer_token: None,
        });
    }
    let token_id = ulid::Ulid::new().to_string();
    let secret = random_secret()?;
    let verifier = token_hmac(pepper, &token_id, &secret)?;
    let created_at = now();
    let active: i64 = connection.query_row(
        "SELECT COUNT(*) FROM token_metadata WHERE revoked_at IS NULL AND (expires_at IS NULL OR julianday(expires_at)>julianday('now'))",
        [],
        |row| row.get(0),
    ).map_err(state_sql)?;
    if active >= MAX_ACTIVE_TOKENS {
        return Err(StateError::Overloaded);
    }
    connection.execute(
        "INSERT INTO token_metadata(id,name,verifier,scopes_json,allowed_cidrs_json,expires_at,max_concurrent_inference,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        params![token_id, request.name, verifier.as_slice(), json(&request.scopes)?, json(&request.allowed_cidrs)?, request.expires_at, request.max_concurrent_inference, created_at],
    ).map_err(state_sql)?;
    let result = serde_json::json!({"token_id":token_id});
    let running = transition(
        connection,
        &accepted.operation.id,
        OperationState::Running,
        OperationProgress {
            stage: "publishing".into(),
            current: Some(0),
            total: Some(1),
            unit: Some("token".into()),
            message: "publishing token policy".into(),
        },
        None,
        None,
    )?;
    let operation = transition(
        connection,
        &running.id,
        OperationState::Succeeded,
        OperationProgress {
            stage: "complete".into(),
            current: Some(1),
            total: Some(1),
            unit: Some("token".into()),
            message: "token policy published".into(),
        },
        Some(result),
        None,
    )?;
    connection.execute(
        "INSERT INTO audit(occurred_at,actor_token_id,action,target,outcome,metadata_json) VALUES(?1,?2,'token.create',?3,'succeeded',?4)",
        params![now(), actor, token_id, json(&serde_json::json!({"scopes":request.scopes}))?],
    ).map_err(state_sql)?;
    let token = read_token(connection, &token_id)?;
    Ok(CreatedToken {
        operation,
        token,
        bearer_token: Some(format!("sy_{token_id}_{secret}")),
    })
}

fn revoke_token(
    connection: &mut Connection,
    actor: &str,
    key: &str,
    token_id: &str,
) -> Result<OperationDocument, StateError> {
    let request_hash = format!("{:x}", Sha256::digest(token_id.as_bytes()));
    let accepted = accept_operation(
        connection,
        actor,
        "token.revoke",
        key,
        &request_hash,
        Some(token_id.into()),
    )?;
    if accepted.reused {
        return Ok(accepted.operation);
    }
    if read_token(connection, token_id)?.revoked_at.is_none() {
        connection
            .execute(
                "UPDATE token_metadata SET revoked_at=?2 WHERE id=?1",
                params![token_id, now()],
            )
            .map_err(state_sql)?;
    }
    let running = transition(
        connection,
        &accepted.operation.id,
        OperationState::Running,
        OperationProgress {
            stage: "revoking".into(),
            current: Some(0),
            total: Some(1),
            unit: Some("token".into()),
            message: "committing revocation".into(),
        },
        None,
        None,
    )?;
    let operation = transition(
        connection,
        &running.id,
        OperationState::Succeeded,
        OperationProgress {
            stage: "complete".into(),
            current: Some(1),
            total: Some(1),
            unit: Some("token".into()),
            message: "revocation published".into(),
        },
        Some(serde_json::json!({"token_id":token_id})),
        None,
    )?;
    connection.execute(
        "INSERT INTO audit(occurred_at,actor_token_id,action,target,outcome,metadata_json) VALUES(?1,?2,'token.revoke',?3,'succeeded','{}')",
        params![now(), actor, token_id],
    ).map_err(state_sql)?;
    Ok(operation)
}

fn promote_model(
    connection: &mut Connection,
    mut model: ModelDocument,
    update_alias: bool,
) -> Result<ModelDocument, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    for alias in &model.aliases {
        let prior: Option<String> = transaction
            .query_row(
                "SELECT model_id FROM aliases WHERE name=?1",
                [alias],
                |row| row.get(0),
            )
            .optional()
            .map_err(state_sql)?;
        if prior.as_deref().is_some_and(|id| id != model.id) && !update_alias {
            return Err(StateError::Conflict(format!(
                "alias {alias:?} already points to another immutable model; use --update-alias"
            )));
        }
    }
    let aliases = std::mem::take(&mut model.aliases);
    transaction
        .execute(
            "INSERT INTO models(id,repository,commit_sha,metadata_json) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET metadata_json=excluded.metadata_json",
            params![model.id, model.repository, model.commit, json(&model)?],
        )
        .map_err(state_sql)?;
    for alias in aliases {
        transaction
            .execute(
                "INSERT INTO aliases(name,model_id) VALUES(?1,?2) ON CONFLICT(name) DO UPDATE SET model_id=excluded.model_id",
                params![alias, model.id],
            )
            .map_err(state_sql)?;
    }
    let promoted = read_model_tx(&transaction, &model.id)?;
    append_audit(
        &transaction,
        "system",
        "model.promote",
        Some(&model.id),
        "succeeded",
        serde_json::json!({"repository":model.repository,"commit":model.commit}),
    )?;
    transaction.commit().map_err(state_sql)?;
    Ok(promoted)
}

fn list_models(connection: &Connection) -> Result<Vec<ModelDocument>, StateError> {
    let mut statement = connection
        .prepare("SELECT id FROM models ORDER BY repository,commit_sha")
        .map_err(state_sql)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    ids.iter().map(|id| read_model(connection, id)).collect()
}

fn read_model(connection: &Connection, reference: &str) -> Result<ModelDocument, StateError> {
    let alias: Option<String> = connection
        .query_row(
            "SELECT model_id FROM aliases WHERE name=?1",
            [reference],
            |row| row.get(0),
        )
        .optional()
        .map_err(state_sql)?;
    if let Some(id) = alias {
        return read_model_row(connection, &id);
    }
    let mut statement = connection
        .prepare("SELECT id FROM models WHERE id=?1 OR metadata_json->>'canonical'=?1 OR repository=?1 ORDER BY commit_sha")
        .map_err(state_sql)?;
    let ids = statement
        .query_map([reference], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    match ids.as_slice() {
        [id] => read_model_row(connection, id),
        [] => Err(StateError::NotFound),
        _ => Err(StateError::Conflict(
            "model reference is ambiguous; use an alias or canonical identity".into(),
        )),
    }
}

fn read_model_tx(transaction: &Transaction<'_>, id: &str) -> Result<ModelDocument, StateError> {
    let metadata: Option<String> = transaction
        .query_row(
            "SELECT metadata_json FROM models WHERE id=?1",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(state_sql)?;
    let mut model: ModelDocument = serde_json::from_str(&metadata.ok_or(StateError::NotFound)?)
        .map_err(|error| StateError::Unavailable(format!("decode model state: {error}")))?;
    model.aliases = query_strings_tx(
        transaction,
        "SELECT name FROM aliases WHERE model_id=?1 ORDER BY name",
        id,
    )?;
    model.active_instances = query_strings_tx(
        transaction,
        "SELECT name FROM instances WHERE model_id=?1 AND desired_state='running' ORDER BY name",
        id,
    )?;
    Ok(model)
}

fn read_model_row(connection: &Connection, id: &str) -> Result<ModelDocument, StateError> {
    let metadata: Option<String> = connection
        .query_row(
            "SELECT metadata_json FROM models WHERE id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(state_sql)?;
    let mut model: ModelDocument = serde_json::from_str(&metadata.ok_or(StateError::NotFound)?)
        .map_err(|error| StateError::Unavailable(format!("decode model state: {error}")))?;
    model.aliases = query_strings(
        connection,
        "SELECT name FROM aliases WHERE model_id=?1 ORDER BY name",
        id,
    )?;
    model.active_instances = query_strings(
        connection,
        "SELECT name FROM instances WHERE model_id=?1 AND desired_state='running' ORDER BY name",
        id,
    )?;
    Ok(model)
}

fn query_strings(
    connection: &Connection,
    sql: &str,
    value: &str,
) -> Result<Vec<String>, StateError> {
    let mut statement = connection.prepare(sql).map_err(state_sql)?;
    let values = statement
        .query_map([value], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    Ok(values)
}

fn query_strings_tx(
    transaction: &Transaction<'_>,
    sql: &str,
    value: &str,
) -> Result<Vec<String>, StateError> {
    let mut statement = transaction.prepare(sql).map_err(state_sql)?;
    let values = statement
        .query_map([value], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    Ok(values)
}

fn remove_model(connection: &mut Connection, id: &str) -> Result<(), StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let active: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM instances WHERE model_id=?1",
            [id],
            |row| row.get(0),
        )
        .map_err(state_sql)?;
    if active != 0 {
        return Err(StateError::Conflict(
            "model is referenced by an instance".into(),
        ));
    }
    transaction
        .execute("DELETE FROM aliases WHERE model_id=?1", [id])
        .map_err(state_sql)?;
    if transaction
        .execute("DELETE FROM models WHERE id=?1", [id])
        .map_err(state_sql)?
        != 1
    {
        return Err(StateError::NotFound);
    }
    transaction.commit().map_err(state_sql)
}

fn begin_serve(
    connection: &mut Connection,
    mut requested: InstanceDocument,
) -> Result<AcceptedInstance, StateError> {
    validate_instance(&requested)?;
    let transaction = connection.transaction().map_err(state_sql)?;
    let prior: Option<String> = transaction
        .query_row(
            "SELECT metadata_json FROM instances WHERE name=?1",
            [&requested.name],
            |row| row.get(0),
        )
        .optional()
        .map_err(state_sql)?;
    if let Some(prior) = prior {
        let current: InstanceDocument = from_json(&prior)?;
        if current.desired == InstanceDesiredState::Running && !current.restart_suppressed {
            if same_serve_identity(&current, &requested) {
                transaction.commit().map_err(state_sql)?;
                return Ok(AcceptedInstance {
                    instance: current,
                    reused: true,
                });
            }
            return Err(StateError::Conflict(format!(
                "instance {} already has different running intent; stop it or choose another name",
                requested.name
            )));
        }
        requested.id = current.id;
        requested.generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| StateError::Conflict("instance generation is exhausted".into()))?;
    } else {
        requested.generation = 1;
    }
    requested.desired = InstanceDesiredState::Running;
    requested.observed = InstanceObservedState::Creating;
    requested.endpoint = None;
    requested.healthy = false;
    requested.started_at = None;
    requested.last_failure = None;
    let metadata = json(&requested)?;
    transaction
        .execute(
            "INSERT INTO instances(id,name,model_id,generation,desired_state,observed_state,metadata_json) VALUES(?1,?2,?3,?4,'running','creating',?5) ON CONFLICT(name) DO UPDATE SET model_id=excluded.model_id,generation=excluded.generation,desired_state='running',observed_state='creating',metadata_json=excluded.metadata_json",
            params![requested.id, requested.name, requested.model_id, i64::try_from(requested.generation).map_err(|_| StateError::Invalid("instance generation is out of range".into()))?, metadata],
        )
        .map_err(state_sql)?;
    transaction
        .execute(
            "INSERT INTO instance_resources(instance_id,cold_start_peak_bytes,steady_peak_bytes,incremental_start_peak_bytes,phase,started_sequence,current_memory_bytes,previous_memory_bytes,restart_suppressed,suppression_cause) VALUES(?1,?2,?3,?2,'starting',?4,0,0,0,NULL) ON CONFLICT(instance_id) DO UPDATE SET cold_start_peak_bytes=excluded.cold_start_peak_bytes,steady_peak_bytes=excluded.steady_peak_bytes,incremental_start_peak_bytes=excluded.incremental_start_peak_bytes,phase='starting',started_sequence=excluded.started_sequence,current_memory_bytes=0,previous_memory_bytes=0,restart_suppressed=0,suppression_cause=NULL",
            params![requested.id, i64::try_from(requested.resources.startup_peak_bytes).map_err(|_| StateError::Invalid("startup resource envelope is out of range".into()))?, i64::try_from(requested.resources.steady_peak_bytes).map_err(|_| StateError::Invalid("steady resource envelope is out of range".into()))?, i64::try_from(requested.generation).map_err(|_| StateError::Invalid("instance generation is out of range".into()))?],
        )
        .map_err(state_sql)?;
    transaction.commit().map_err(state_sql)?;
    Ok(AcceptedInstance {
        instance: requested,
        reused: false,
    })
}

fn same_serve_identity(left: &InstanceDocument, right: &InstanceDocument) -> bool {
    left.model_id == right.model_id
        && left.model_commit == right.model_commit
        && left.recipe_id == right.recipe_id
        && left.recipe_fingerprint == right.recipe_fingerprint
        && left.objective == right.objective
}

fn list_instances(connection: &Connection) -> Result<Vec<InstanceDocument>, StateError> {
    let mut statement = connection
        .prepare("SELECT metadata_json FROM instances ORDER BY name LIMIT 256")
        .map_err(state_sql)?;
    let instances = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .map(|row| row.map_err(state_sql).and_then(|value| from_json(&value)))
        .collect();
    instances
}

fn read_instance(connection: &Connection, reference: &str) -> Result<InstanceDocument, StateError> {
    let values = {
        let mut statement = connection
            .prepare("SELECT metadata_json FROM instances WHERE id=?1 OR name=?1 LIMIT 2")
            .map_err(state_sql)?;
        let values = statement
            .query_map([reference], |row| row.get::<_, String>(0))
            .map_err(state_sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(state_sql)?;
        values
    };
    match values.as_slice() {
        [value] => from_json(value),
        [] => Err(StateError::NotFound),
        _ => Err(StateError::Conflict(
            "instance reference is ambiguous".into(),
        )),
    }
}

fn set_instance_observed(
    connection: &mut Connection,
    id: &str,
    generation: u64,
    observed: InstanceObservedState,
    endpoint: Option<String>,
    failure: Option<String>,
    startup_milliseconds: Option<u64>,
) -> Result<InstanceDocument, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let mut instance = read_instance_tx(&transaction, id)?;
    if instance.generation != generation {
        return Err(StateError::Conflict("stale instance generation".into()));
    }
    if endpoint
        .as_ref()
        .is_some_and(|value| value.len() > 256 || value.contains("://") || value.contains("172."))
    {
        return Err(StateError::Invalid("published endpoint is invalid".into()));
    }
    instance.observed = observed;
    instance.healthy = observed == InstanceObservedState::Healthy;
    instance.endpoint = endpoint;
    instance.last_failure = failure;
    if let Some(measurement) = startup_milliseconds {
        instance.startup_milliseconds = Some(measurement);
    }
    if observed == InstanceObservedState::Healthy && instance.started_at.is_none() {
        instance.started_at = Some(now());
    }
    if observed == InstanceObservedState::Healthy {
        instance.restart_failures = 0;
        instance.restart_suppressed = false;
        transaction
            .execute(
                "DELETE FROM restart_failures WHERE instance_id=?1 AND generation=?2",
                params![
                    id,
                    i64::try_from(generation).map_err(|_| StateError::Invalid(
                        "instance generation is out of range".into()
                    ))?
                ],
            )
            .map_err(state_sql)?;
        transaction
            .execute(
                "UPDATE instance_resources SET restart_suppressed=0,suppression_cause=NULL WHERE instance_id=?1",
                [id],
            )
            .map_err(state_sql)?;
    }
    persist_instance(&transaction, &instance)?;
    transaction.commit().map_err(state_sql)?;
    Ok(instance)
}

fn begin_stop(
    connection: &mut Connection,
    reference: &str,
) -> Result<InstanceDocument, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let mut instance = read_instance_tx(&transaction, reference)?;
    instance.desired = InstanceDesiredState::Stopped;
    instance.endpoint = None;
    instance.healthy = false;
    if instance.observed != InstanceObservedState::Absent {
        instance.observed = InstanceObservedState::Stopping;
    }
    persist_instance(&transaction, &instance)?;
    transaction.commit().map_err(state_sql)?;
    Ok(instance)
}

fn record_restart_failure(
    connection: &mut Connection,
    id: &str,
    generation: u64,
    failed_at_unix_seconds: u64,
) -> Result<InstanceDocument, StateError> {
    let transaction = connection.transaction().map_err(state_sql)?;
    let mut instance = read_instance_tx(&transaction, id)?;
    if instance.generation != generation {
        return Err(StateError::Conflict("stale instance generation".into()));
    }
    let generation = i64::try_from(generation)
        .map_err(|_| StateError::Invalid("instance generation is out of range".into()))?;
    let failed_at = i64::try_from(failed_at_unix_seconds)
        .map_err(|_| StateError::Invalid("restart failure time is out of range".into()))?;
    let window_start = failed_at_unix_seconds.saturating_sub(RESTART_WINDOW_SECONDS);
    let window_start = i64::try_from(window_start)
        .map_err(|_| StateError::Invalid("restart failure window is out of range".into()))?;
    transaction
        .execute(
            "INSERT OR IGNORE INTO restart_failures(instance_id,generation,failed_at_unix_seconds) VALUES(?1,?2,?3)",
            params![id, generation, failed_at],
        )
        .map_err(state_sql)?;
    transaction
        .execute(
            "DELETE FROM restart_failures WHERE instance_id=?1 AND generation=?2 AND failed_at_unix_seconds<?3",
            params![id, generation, window_start],
        )
        .map_err(state_sql)?;
    let failures: u32 = transaction
        .query_row(
            "SELECT COUNT(*) FROM restart_failures WHERE instance_id=?1 AND generation=?2",
            params![id, generation],
            |row| row.get(0),
        )
        .map_err(state_sql)?;
    instance.restart_failures = failures;
    instance.restart_suppressed = failures >= RESTART_FAILURE_LIMIT as u32;
    instance.observed = if instance.restart_suppressed {
        InstanceObservedState::Failed
    } else {
        InstanceObservedState::Degraded
    };
    instance.healthy = false;
    instance.endpoint = None;
    instance.last_failure = Some("managed engine restart failed".into());
    persist_instance(&transaction, &instance)?;
    transaction
        .execute(
            "UPDATE instance_resources SET restart_suppressed=?2,suppression_cause=?3 WHERE instance_id=?1",
            params![id, instance.restart_suppressed, instance.restart_suppressed.then_some("restart-failure-budget")],
        )
        .map_err(state_sql)?;
    transaction.commit().map_err(state_sql)?;
    Ok(instance)
}

fn mark_quarantine(
    connection: &mut Connection,
    id: &str,
    generation: u64,
    cause: &str,
) -> Result<InstanceDocument, StateError> {
    if cause.is_empty() || cause.len() > 128 {
        return Err(StateError::Invalid("quarantine cause is invalid".into()));
    }
    let transaction = connection.transaction().map_err(state_sql)?;
    let mut instance = read_instance_tx(&transaction, id)?;
    if instance.generation != generation {
        return Err(StateError::Conflict("stale instance generation".into()));
    }
    instance.quarantine = Some(cause.into());
    instance.observed = InstanceObservedState::Failed;
    instance.healthy = false;
    instance.endpoint = None;
    persist_instance(&transaction, &instance)?;
    transaction.commit().map_err(state_sql)?;
    Ok(instance)
}

fn record_quarantine(
    connection: &mut Connection,
    evidence: &QuarantineEvidence,
) -> Result<(), StateError> {
    if evidence.container_id.len() != 64
        || !evidence
            .container_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || evidence
            .instance_id
            .as_ref()
            .is_some_and(|id| id.len() > 128)
        || evidence.cause.is_empty()
        || evidence.cause.len() > 128
    {
        return Err(StateError::Invalid("quarantine evidence is invalid".into()));
    }
    let generation = evidence
        .generation
        .map(i64::try_from)
        .transpose()
        .map_err(|_| StateError::Invalid("quarantine generation is out of range".into()))?;
    connection
        .execute(
            "INSERT INTO quarantine_evidence(container_id,instance_id,generation,cause,observed_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(container_id) DO UPDATE SET instance_id=excluded.instance_id,generation=excluded.generation,cause=excluded.cause,observed_at=excluded.observed_at",
            params![evidence.container_id, evidence.instance_id, generation, evidence.cause, now()],
        )
        .map_err(state_sql)
        .map(|_| ())
}

fn list_quarantine(connection: &Connection) -> Result<Vec<QuarantineEvidence>, StateError> {
    let mut statement = connection
        .prepare("SELECT container_id,instance_id,generation,cause FROM quarantine_evidence ORDER BY observed_at DESC,container_id LIMIT 256")
        .map_err(state_sql)?;
    let evidence = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(state_sql)?
        .map(|row| {
            let (container_id, instance_id, generation, cause) = row.map_err(state_sql)?;
            Ok(QuarantineEvidence {
                container_id,
                instance_id,
                generation: generation.map(u64::try_from).transpose().map_err(|_| {
                    StateError::Unavailable("stored quarantine generation is invalid".into())
                })?,
                cause,
            })
        })
        .collect();
    evidence
}

fn read_instance_tx(
    transaction: &Transaction<'_>,
    reference: &str,
) -> Result<InstanceDocument, StateError> {
    let metadata: Option<String> = transaction
        .query_row(
            "SELECT metadata_json FROM instances WHERE id=?1 OR name=?1 LIMIT 1",
            [reference],
            |row| row.get(0),
        )
        .optional()
        .map_err(state_sql)?;
    from_json(&metadata.ok_or(StateError::NotFound)?)
}

fn persist_instance(
    transaction: &Transaction<'_>,
    instance: &InstanceDocument,
) -> Result<(), StateError> {
    transaction
        .execute(
            "UPDATE instances SET desired_state=?2,observed_state=?3,metadata_json=?4 WHERE id=?1 AND generation=?5",
            params![instance.id, desired_text(instance.desired), observed_text(instance.observed), json(instance)?, i64::try_from(instance.generation).map_err(|_| StateError::Invalid("instance generation is out of range".into()))?],
        )
        .map_err(state_sql)
        .and_then(|changed| (changed == 1).then_some(()).ok_or(StateError::NotFound))
}

fn validate_instance(instance: &InstanceDocument) -> Result<(), StateError> {
    let valid_id = instance.id.len() == 34
        && instance.id.starts_with("i_")
        && instance.id[2..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    let valid_name = !instance.name.is_empty()
        && instance.name.len() <= 63
        && instance.name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        });
    let valid_recipe_fingerprint = instance
        .recipe_fingerprint
        .strip_prefix("sha256:")
        .is_some_and(|value| {
            value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        });
    if instance.schema != INSTANCE_SCHEMA
        || !valid_id
        || !valid_name
        || instance.model_id.is_empty()
        || instance.model_commit.len() != 40
        || instance.recipe_id.is_empty()
        || !valid_recipe_fingerprint
        || !matches!(
            instance.objective.as_str(),
            "agent" | "interactive" | "throughput" | "long-context"
        )
    {
        return Err(StateError::Invalid("instance intent is invalid".into()));
    }
    Ok(())
}

fn desired_text(value: InstanceDesiredState) -> &'static str {
    match value {
        InstanceDesiredState::Running => "running",
        InstanceDesiredState::Stopped => "stopped",
    }
}

fn observed_text(value: InstanceObservedState) -> &'static str {
    match value {
        InstanceObservedState::Absent => "absent",
        InstanceObservedState::Creating => "creating",
        InstanceObservedState::Warming => "warming",
        InstanceObservedState::Healthy => "healthy",
        InstanceObservedState::Degraded => "degraded",
        InstanceObservedState::Stopping => "stopping",
        InstanceObservedState::Failed => "failed",
    }
}

fn list_operations(connection: &Connection) -> Result<Vec<OperationDocument>, StateError> {
    let mut statement = connection
        .prepare("SELECT id FROM operations ORDER BY created_at DESC, id DESC LIMIT 256")
        .map_err(state_sql)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    ids.iter()
        .map(|id| read_operation(connection, id))
        .collect()
}

fn read_operation(connection: &Connection, id: &str) -> Result<OperationDocument, StateError> {
    read_operation_row(connection, id)
}

fn read_operation_tx(
    transaction: &Transaction<'_>,
    id: &str,
) -> Result<OperationDocument, StateError> {
    read_operation_row(transaction, id)
}

fn read_operation_row(connection: &Connection, id: &str) -> Result<OperationDocument, StateError> {
    connection.query_row(
        "SELECT id,kind,actor_token_id,target,state,progress_json,created_at,updated_at,result_json,problem_json FROM operations WHERE id=?1",
        [id],
        |row| {
            let state: String = row.get(4)?;
            let progress: String = row.get(5)?;
            let result: Option<String> = row.get(8)?;
            let problem: Option<String> = row.get(9)?;
            Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,state,progress,row.get(6)?,row.get(7)?,result,problem))
        },
    ).optional().map_err(state_sql)?
        .ok_or(StateError::NotFound)
        .and_then(|(id,kind,actor_token_id,target,state,progress,created_at,updated_at,result,problem)| Ok(OperationDocument {
            schema: OPERATION_SCHEMA.into(), id, kind, actor_token_id, target,
            state: parse_state(&state)?, progress: from_json(&progress)?, created_at, updated_at,
            result: result.map(|value| from_json(&value)).transpose()?,
            problem: problem.map(|value| from_json(&value)).transpose()?,
        }))
}

fn read_events(
    connection: &Connection,
    id: &str,
    after: u64,
) -> Result<Vec<OperationEvent>, StateError> {
    read_operation(connection, id)?;
    let mut statement = connection.prepare(
        "SELECT sequence,state,progress_json,occurred_at FROM operation_events WHERE operation_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 1024"
    ).map_err(state_sql)?;
    let after = i64::try_from(after)
        .map_err(|_| StateError::Invalid("event cursor is out of range".into()))?;
    let rows = statement
        .query_map(params![id, after], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(state_sql)?;
    rows.map(|row| {
        let (sequence, state, progress, occurred_at) = row.map_err(state_sql)?;
        Ok(OperationEvent {
            schema: OPERATION_EVENT_SCHEMA.into(),
            id: u64::try_from(sequence)
                .map_err(|_| StateError::Unavailable("stored event sequence is invalid".into()))?,
            operation_id: id.into(),
            state: parse_state(&state)?,
            progress: from_json(&progress)?,
            occurred_at,
        })
    })
    .collect()
}

fn list_tokens(connection: &Connection) -> Result<Vec<TokenDocument>, StateError> {
    let mut statement = connection
        .prepare("SELECT id FROM token_metadata ORDER BY created_at DESC, id DESC LIMIT 1024")
        .map_err(state_sql)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(state_sql)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(state_sql)?;
    ids.iter().map(|id| read_token(connection, id)).collect()
}

fn read_token(connection: &Connection, id: &str) -> Result<TokenDocument, StateError> {
    connection.query_row(
        "SELECT id,name,scopes_json,allowed_cidrs_json,expires_at,max_concurrent_inference,created_at,last_used_at,revoked_at FROM token_metadata WHERE id=?1",
        [id],
        |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?,row.get::<_, String>(3)?,row.get::<_, Option<String>>(4)?,row.get::<_, u32>(5)?,row.get::<_, String>(6)?,row.get::<_, Option<String>>(7)?,row.get::<_, Option<String>>(8)?)),
    ).optional().map_err(state_sql)?.ok_or(StateError::NotFound).and_then(|(id,name,scopes,cidrs,expires_at,max_concurrent_inference,created_at,last_used_at,revoked_at)| Ok(TokenDocument {
        schema: TOKEN_SCHEMA.into(), id, name, scopes: from_json(&scopes)?, allowed_cidrs: from_json(&cidrs)?, expires_at, max_concurrent_inference, created_at, last_used_at, revoked_at,
    }))
}

fn load_snapshot(connection: &Connection) -> Result<AuthSnapshot, StateError> {
    let mut statement = connection.prepare(
        "SELECT id,name,verifier,scopes_json,allowed_cidrs_json,expires_at,max_concurrent_inference,created_at,last_used_at,revoked_at FROM token_metadata WHERE revoked_at IS NULL AND (expires_at IS NULL OR julianday(expires_at)>julianday('now')) LIMIT 1024"
    ).map_err(state_sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, u32>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })
        .map_err(state_sql)?;
    let mut tokens = BTreeMap::new();
    for row in rows {
        let (
            id,
            name,
            verifier,
            scopes,
            cidrs,
            expires_at,
            max_concurrent_inference,
            created_at,
            last_used_at,
            revoked_at,
        ) = row.map_err(state_sql)?;
        let verifier: [u8; 32] = verifier.try_into().map_err(|_| {
            StateError::Unavailable("stored token verifier has invalid length".into())
        })?;
        let token = TokenDocument {
            schema: TOKEN_SCHEMA.into(),
            id: id.clone(),
            name,
            scopes: from_json(&scopes)?,
            allowed_cidrs: from_json(&cidrs)?,
            expires_at,
            max_concurrent_inference,
            created_at,
            last_used_at,
            revoked_at,
        };
        tokens.insert(id, TokenVerifier { token, verifier });
    }
    Ok(AuthSnapshot { tokens })
}

fn append_event(
    transaction: &Transaction<'_>,
    operation_id: &str,
    state: OperationState,
    progress: &OperationProgress,
    occurred_at: &str,
) -> Result<(), StateError> {
    transaction.execute(
        "INSERT INTO operation_events(operation_id,sequence,state,progress_json,occurred_at) VALUES(?1,COALESCE((SELECT MAX(sequence)+1 FROM operation_events WHERE operation_id=?1),1),?2,?3,?4)",
        params![operation_id, state_text(state), json(progress)?, occurred_at],
    ).map_err(state_sql)?;
    Ok(())
}

fn append_audit(
    transaction: &Transaction<'_>,
    actor: &str,
    action: &str,
    target: Option<&str>,
    outcome: &str,
    metadata: serde_json::Value,
) -> Result<(), StateError> {
    transaction.execute(
        "INSERT INTO audit(occurred_at,actor_token_id,action,target,outcome,metadata_json) VALUES(?1,?2,?3,?4,?5,?6)",
        params![now(), actor, action, target, outcome, json(&metadata)?],
    ).map_err(state_sql)?;
    Ok(())
}

fn validate_idempotency(key: &str, hash: &str) -> Result<(), StateError> {
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(StateError::Invalid(
            "Idempotency-Key must contain 1..128 visible ASCII bytes".into(),
        ));
    }
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(StateError::Invalid(
            "canonical request hash is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_token_request(request: &TokenCreateRequest) -> Result<(), StateError> {
    if request.name.trim().is_empty() || request.name.len() > 80 {
        return Err(StateError::Invalid(
            "token name must contain 1..80 bytes".into(),
        ));
    }
    if request.scopes.is_empty() || request.scopes.len() > 11 {
        return Err(StateError::Invalid(
            "token must contain 1..11 scopes".into(),
        ));
    }
    if request.allowed_cidrs.len() > 16
        || request.allowed_cidrs.iter().any(|value| !valid_cidr(value))
    {
        return Err(StateError::Invalid("token CIDR policy is invalid".into()));
    }
    if request.max_concurrent_inference == 0 || request.max_concurrent_inference > 64 {
        return Err(StateError::Invalid(
            "token inference concurrency must be in 1..64".into(),
        ));
    }
    if let Some(expiry) = &request.expires_at {
        let expiry = chrono::DateTime::parse_from_rfc3339(expiry)
            .map_err(|_| StateError::Invalid("token expiry must be RFC 3339".into()))?;
        if expiry <= chrono::Utc::now() {
            return Err(StateError::Invalid(
                "token expiry must be in the future".into(),
            ));
        }
    }
    Ok(())
}

fn desired_resource_envelopes(
    connection: &Connection,
) -> Result<Vec<DeclaredEnvelope>, StateError> {
    let mut statement = connection
        .prepare(
            "SELECT r.instance_id,i.name,r.cold_start_peak_bytes FROM instance_resources r JOIN instances i ON i.id=r.instance_id WHERE i.desired_state='running' ORDER BY r.instance_id",
        )
        .map_err(state_sql)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(state_sql)?;
    rows.map(|row| {
        let (instance_id, instance_name, cold_start_peak_bytes) = row.map_err(state_sql)?;
        Ok(DeclaredEnvelope {
            instance_id,
            instance_name,
            cold_start_peak_bytes: cold_start_peak_bytes.try_into().map_err(|_| {
                StateError::Unavailable("stored resource envelope is invalid".into())
            })?,
        })
    })
    .collect()
}

fn import_emergency(
    connection: &mut Connection,
    record: &EmergencyRecord,
) -> Result<bool, StateError> {
    if record.schema != "sy.spark.emergency-record/v1"
        || record.event_id.parse::<ulid::Ulid>().is_err()
        || record.decision.instance_id.is_empty()
        || record.decision.cause.is_empty()
    {
        return Err(StateError::Invalid("emergency record is invalid".into()));
    }
    let generation: i64 = record
        .decision
        .generation
        .try_into()
        .map_err(|_| StateError::Invalid("emergency generation is out of range".into()))?;
    let transaction = connection.transaction().map_err(state_sql)?;
    let inserted = transaction
        .execute(
            "INSERT OR IGNORE INTO emergency_records(event_id,instance_id,generation,cause,evidence_json,imported_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![record.event_id, record.decision.instance_id, generation, record.decision.cause, json(record)?, now()],
        )
        .map_err(state_sql)?
        == 1;
    if !inserted {
        transaction.rollback().map_err(state_sql)?;
        return Ok(false);
    }
    let mut instance = read_instance_tx(&transaction, &record.decision.instance_id)?;
    if instance.generation != record.decision.generation {
        append_audit(
            &transaction,
            "root-executor",
            "memory.emergency",
            Some(&record.decision.instance_id),
            "ignored-stale-generation",
            serde_json::json!({
                "record": record,
                "current_generation": instance.generation,
            }),
        )?;
        transaction.commit().map_err(state_sql)?;
        return Ok(true);
    }
    transaction
        .execute(
            "UPDATE instance_resources SET restart_suppressed=1,suppression_cause=?2 WHERE instance_id=?1",
            params![record.decision.instance_id, record.decision.cause],
        )
        .map_err(state_sql)?;
    instance.observed = InstanceObservedState::Failed;
    instance.healthy = false;
    instance.endpoint = None;
    instance.restart_suppressed = true;
    instance.last_failure = Some(record.decision.cause.clone());
    persist_instance(&transaction, &instance)?;
    append_audit(
        &transaction,
        "root-executor",
        "memory.emergency",
        Some(&record.decision.instance_id),
        "restart-suppressed",
        serde_json::to_value(record).map_err(|_| StateError::Invalid("encode emergency".into()))?,
    )?;
    transaction.commit().map_err(state_sql)?;

    let active = {
        let mut statement = connection
            .prepare("SELECT id FROM operations WHERE target=?1 AND state IN ('accepted','running') ORDER BY created_at")
            .map_err(state_sql)?;
        let rows = statement
            .query_map(params![record.decision.instance_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(state_sql)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(state_sql)?
    };
    for operation_id in active {
        let current = read_operation(connection, &operation_id)?;
        if current.state == OperationState::Accepted {
            transition(
                connection,
                &operation_id,
                OperationState::Running,
                OperationProgress {
                    stage: "emergency".into(),
                    current: None,
                    total: None,
                    unit: None,
                    message: "root memory guard suppressed restart".into(),
                },
                None,
                None,
            )?;
        }
        transition(
            connection,
            &operation_id,
            OperationState::Failed,
            OperationProgress {
                stage: "failed".into(),
                current: None,
                total: None,
                unit: None,
                message: "instance stopped by the root memory guard".into(),
            },
            None,
            Some(ProblemDocument {
                schema: PROBLEM_SCHEMA.into(),
                r#type: "https://sy.local/problems/spark-memory-emergency".into(),
                code: "spark.memory.emergency-shed".into(),
                status: 503,
                detail: record.decision.cause.clone(),
                remediation: vec!["restore memory headroom before serving again".into()],
                operation_id: Some(operation_id.clone()),
            }),
        )?;
    }
    Ok(inserted)
}

fn valid_cidr(value: &str) -> bool {
    if value.len() > 64 {
        return false;
    }
    let Some((address, prefix)) = value.split_once('/') else {
        return false;
    };
    let Ok(address) = address.parse::<std::net::IpAddr>() else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u8>() else {
        return false;
    };
    prefix <= if address.is_ipv4() { 32 } else { 128 }
}

fn token_hmac(pepper: &SecretString, id: &str, secret: &str) -> Result<[u8; 32], StateError> {
    let mut mac = HmacSha256::new_from_slice(pepper.expose_secret().as_bytes())
        .map_err(|_| StateError::Unavailable("token pepper is invalid".into()))?;
    mac.update(id.as_bytes());
    mac.update(secret.as_bytes());
    Ok(mac.finalize().into_bytes().into())
}

fn random_secret() -> Result<String, StateError> {
    use std::io::Read;
    let mut bytes = [0_u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|_| StateError::Unavailable("operating-system random source failed".into()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn state_text(state: OperationState) -> &'static str {
    match state {
        OperationState::Accepted => "accepted",
        OperationState::Running => "running",
        OperationState::Succeeded => "succeeded",
        OperationState::Failed => "failed",
        OperationState::Cancelled => "cancelled",
    }
}

fn parse_state(value: &str) -> Result<OperationState, StateError> {
    match value {
        "accepted" => Ok(OperationState::Accepted),
        "running" => Ok(OperationState::Running),
        "succeeded" => Ok(OperationState::Succeeded),
        "failed" => Ok(OperationState::Failed),
        "cancelled" => Ok(OperationState::Cancelled),
        _ => Err(StateError::Unavailable(
            "stored operation state is invalid".into(),
        )),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn json<T: serde::Serialize>(value: &T) -> Result<String, StateError> {
    serde_json::to_string(value)
        .map_err(|error| StateError::Unavailable(format!("encode database value: {error}")))
}
fn optional_json<T: serde::Serialize>(value: Option<&T>) -> Result<Option<String>, StateError> {
    value.map(json).transpose()
}
fn from_json<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, StateError> {
    serde_json::from_str(value)
        .map_err(|error| StateError::Unavailable(format!("decode database value: {error}")))
}
fn state_sql(error: rusqlite::Error) -> StateError {
    StateError::Unavailable(format!("database failure: {error}"))
}
fn state_migration(error: impl std::fmt::Display) -> StateError {
    StateError::Unavailable(format!("database migration failure: {error}"))
}
fn state_io(error: std::io::Error) -> StateError {
    StateError::Unavailable(format!("database filesystem failure: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spark::wire::TokenScope;

    const QUEUE_CAPACITY: usize = 4;
    const PEPPER: &str = "test-only-pepper-with-at-least-256-bits-of-random-material";

    fn actor(root: &Path) -> DbActor {
        DbActor::open(
            root.join("state.sqlite3"),
            root.join("backups"),
            QUEUE_CAPACITY,
            7,
            SecretString::from(PEPPER),
        )
        .unwrap()
    }

    fn model_document(id: &str, alias: &str) -> ModelDocument {
        ModelDocument {
            schema: crate::spark::wire::MODEL_SCHEMA.into(),
            id: id.into(),
            canonical: format!("huggingface:ornith-ai/Ornith-1.5-9B@{}", "a".repeat(40)),
            repository: "ornith-ai/Ornith-1.5-9B".into(),
            commit: "a".repeat(40),
            snapshot: format!(
                "models--ornith-ai--Ornith-1.5-9B/snapshots/{}",
                "a".repeat(40)
            ),
            logical_bytes: 1,
            unique_bytes: 1,
            aliases: vec![alias.into()],
            active_instances: Vec::new(),
            transport: "fixture".into(),
            verified_at: "2026-08-24T00:00:00Z".into(),
            gated: false,
            license: Some("MIT".into()),
        }
    }

    fn instance_document(model: &ModelDocument, name: &str, recipe: &str) -> InstanceDocument {
        InstanceDocument {
            schema: INSTANCE_SCHEMA.into(),
            id: format!("i_{}", "1".repeat(32)),
            name: name.into(),
            model_id: model.id.clone(),
            model: model.canonical.clone(),
            model_commit: model.commit.clone(),
            recipe_id: recipe.into(),
            recipe_fingerprint: format!("sha256:{}", "b".repeat(64)),
            objective: "agent".into(),
            resources: crate::spark::wire::RecipeResourceEnvelopeDocument {
                image_bytes: 1,
                startup_peak_bytes: 2,
                steady_peak_bytes: 1,
                compile_cache_bytes: 1,
            },
            generation: 0,
            desired: InstanceDesiredState::Running,
            observed: InstanceObservedState::Creating,
            endpoint: None,
            healthy: false,
            started_at: None,
            startup_milliseconds: None,
            last_failure: None,
            restart_failures: 0,
            restart_suppressed: false,
            quarantine: None,
        }
    }

    #[tokio::test]
    async fn actor_sets_wal_full_foreign_keys_and_owns_one_connection() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let health = actor.health().await.unwrap();
        assert_eq!(
            (
                health.journal_mode.as_str(),
                health.synchronous.as_str(),
                health.foreign_keys,
                health.queue_capacity
            ),
            ("wal", "FULL", true, QUEUE_CAPACITY)
        );
        actor.shutdown().unwrap();
    }

    #[test]
    fn migrations_are_valid_checksummed_and_n_minus_one_readable() {
        assert_eq!(
            format!("{:x}", Sha256::digest(MIGRATION_1)),
            "8d2012730fac1a45326920ca6baefcf18b58328293d4322760b576d1d278744b"
        );
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("state.sqlite3");
        let mut connection = Connection::open(&database).unwrap();
        Migrations::new(vec![M::up(MIGRATION_1), M::up(MIGRATION_2)])
            .to_latest(&mut connection)
            .unwrap();
        drop(connection);
        let actor = actor(root.path());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        assert_eq!(
            runtime.block_on(actor.list_operations()).unwrap(),
            Vec::new()
        );
        assert!(fs::read_dir(root.path().join("backups"))
            .unwrap()
            .next()
            .is_some());
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn terminal_operations_and_transitions_are_immutable() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let accepted = actor
            .accept_operation("admin", "test", "one", &"a".repeat(64), None)
            .await
            .unwrap()
            .operation;
        let running = actor
            .transition(
                &accepted.id,
                OperationState::Running,
                accepted.progress.clone(),
                None,
                None,
            )
            .await
            .unwrap();
        let succeeded = actor
            .transition(
                &running.id,
                OperationState::Succeeded,
                running.progress.clone(),
                Some(serde_json::json!({"ok":true})),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            actor.cancel(&succeeded.id, "admin").await,
            Err(StateError::Conflict(_))
        ));
        assert_eq!(
            actor
                .transition(
                    &succeeded.id,
                    OperationState::Succeeded,
                    succeeded.progress.clone(),
                    None,
                    None
                )
                .await
                .unwrap(),
            succeeded
        );
        actor.shutdown().unwrap();
    }

    #[test]
    fn operation_transition_table_covers_all_state_pairs() {
        let states = [
            OperationState::Accepted,
            OperationState::Running,
            OperationState::Succeeded,
            OperationState::Failed,
            OperationState::Cancelled,
        ];
        let legal = states
            .into_iter()
            .flat_map(|current| {
                states
                    .into_iter()
                    .filter(move |next| legal_operation_transition(current, *next))
            })
            .count();
        assert_eq!(legal, 6);
    }

    #[tokio::test]
    async fn instance_intent_is_generation_safe_and_conflicts_do_not_replace() {
        let root = tempfile::tempdir().unwrap();
        let actor = DbActor::open(
            root.path().join("state.sqlite3"),
            root.path().join("backups"),
            8,
            2,
            SecretString::from("pepper"),
        )
        .unwrap();
        let model = model_document("m_0123456789abcdef0123456789abcdef", "ornith-1.5:9b");
        actor.promote_model(model.clone(), false).await.unwrap();
        let first = actor
            .begin_serve(instance_document(&model, "ornith", "recipe-a"))
            .await
            .unwrap()
            .instance;
        let duplicate = actor
            .begin_serve(instance_document(&model, "ornith", "recipe-a"))
            .await
            .unwrap()
            .instance;
        let conflict = actor
            .begin_serve(instance_document(&model, "ornith", "recipe-b"))
            .await;

        assert_eq!((first.generation, duplicate.generation), (1, 1));
        assert!(matches!(conflict, Err(StateError::Conflict(_))));
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn fifth_restart_failure_in_ten_minutes_suppresses_exact_generation() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let model = model_document("m_0123456789abcdef0123456789abcdef", "ornith-1.5:9b");
        actor.promote_model(model.clone(), false).await.unwrap();
        let instance = actor
            .begin_serve(instance_document(&model, "ornith", "recipe-a"))
            .await
            .unwrap()
            .instance;
        for second in 0..5 {
            actor
                .record_restart_failure(&instance.id, instance.generation, 1_000 + second)
                .await
                .unwrap();
        }
        let failed = actor.instance(&instance.id).await.unwrap();
        assert_eq!(
            (failed.restart_failures, failed.restart_suppressed),
            (5, true)
        );
        let explicit = actor
            .begin_serve(instance_document(&model, "ornith", "recipe-a"))
            .await
            .unwrap()
            .instance;
        assert_eq!(
            (explicit.generation, explicit.restart_suppressed),
            (2, false)
        );
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn quarantine_evidence_survives_without_a_matching_sqlite_instance() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let evidence = QuarantineEvidence {
            container_id: "a".repeat(64),
            instance_id: Some(format!("i_{}", "f".repeat(32))),
            generation: Some(99),
            cause: "future-generation".into(),
        };
        actor.record_quarantine(evidence.clone()).await.unwrap();
        assert_eq!(actor.list_quarantine().await.unwrap(), vec![evidence]);
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn emergency_record_suppresses_restart_and_fails_operation() {
        let root = tempfile::tempdir().unwrap();
        let database_path = root.path().join("state.sqlite3");
        let actor = actor(root.path());
        let model = model_document("m_0123456789abcdef0123456789abcdef", "ornith-1.5:9b");
        actor.promote_model(model.clone(), false).await.unwrap();
        let instance = actor
            .begin_serve(instance_document(&model, "instance", "recipe-a"))
            .await
            .unwrap()
            .instance;
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute(
                "UPDATE instance_resources SET cold_start_peak_bytes=32,steady_peak_bytes=24,incremental_start_peak_bytes=32,phase='warming',current_memory_bytes=12,previous_memory_bytes=8 WHERE instance_id=?1",
                [&instance.id],
            )
            .unwrap();
        let accepted = actor
            .accept_operation(
                "admin",
                "serve",
                "emergency-test",
                &"a".repeat(64),
                Some(instance.id.clone()),
            )
            .await
            .unwrap()
            .operation;
        actor
            .transition(
                &accepted.id,
                OperationState::Running,
                accepted.progress.clone(),
                None,
                None,
            )
            .await
            .unwrap();
        let record = EmergencyRecord::from_decision(super::super::resources::EmergencyDecision {
            schema: "sy.spark.emergency-decision/v1".into(),
            instance_id: instance.id.clone(),
            generation: 1,
            cause: "memory-available-floor".into(),
            mem_available_bytes: 7,
            memory_full_psi_avg10_percent: 0.0,
        });
        assert!(actor.import_emergency(record.clone()).await.unwrap());
        assert!(!actor.import_emergency(record).await.unwrap());
        assert_eq!(
            actor.operation(&accepted.id).await.unwrap().state,
            OperationState::Failed
        );
        let suppressed: i64 = connection
            .query_row(
                "SELECT restart_suppressed FROM instance_resources WHERE instance_id=?1",
                [&instance.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(suppressed, 1);
        assert_eq!(
            actor.desired_resource_envelopes().await.unwrap(),
            vec![DeclaredEnvelope {
                instance_id: instance.id,
                instance_name: "instance".into(),
                cold_start_peak_bytes: 32,
            }]
        );
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn duplicate_emergency_after_generation_advance_is_a_noop() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let model = model_document("m_0123456789abcdef0123456789abcdef", "ornith-1.5:9b");
        actor.promote_model(model.clone(), false).await.unwrap();
        let first = actor
            .begin_serve(instance_document(&model, "instance", "recipe-a"))
            .await
            .unwrap()
            .instance;
        let record = EmergencyRecord::from_decision(super::super::resources::EmergencyDecision {
            schema: "sy.spark.emergency-decision/v1".into(),
            instance_id: first.id.clone(),
            generation: first.generation,
            cause: "memory-available-floor".into(),
            mem_available_bytes: 7,
            memory_full_psi_avg10_percent: 0.0,
        });
        assert!(actor.import_emergency(record.clone()).await.unwrap());
        let current = actor
            .begin_serve(instance_document(&model, "instance", "recipe-a"))
            .await
            .unwrap()
            .instance;

        assert!(!actor.import_emergency(record).await.unwrap());
        assert_eq!(actor.instance(&current.id).await.unwrap(), current);
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn unseen_stale_emergency_is_recorded_without_mutating_current_generation() {
        let root = tempfile::tempdir().unwrap();
        let database_path = root.path().join("state.sqlite3");
        let actor = actor(root.path());
        let model = model_document("m_0123456789abcdef0123456789abcdef", "ornith-1.5:9b");
        actor.promote_model(model.clone(), false).await.unwrap();
        let first = actor
            .begin_serve(instance_document(&model, "instance", "recipe-a"))
            .await
            .unwrap()
            .instance;
        actor.begin_stop(&first.id).await.unwrap();
        let current = actor
            .begin_serve(instance_document(&model, "instance", "recipe-a"))
            .await
            .unwrap()
            .instance;
        let record = EmergencyRecord::from_decision(super::super::resources::EmergencyDecision {
            schema: "sy.spark.emergency-decision/v1".into(),
            instance_id: first.id,
            generation: first.generation,
            cause: "memory-available-floor".into(),
            mem_available_bytes: 7,
            memory_full_psi_avg10_percent: 0.0,
        });

        assert!(actor.import_emergency(record.clone()).await.unwrap());
        assert_eq!(actor.instance(&current.id).await.unwrap(), current);
        let connection = Connection::open(database_path).unwrap();
        let outcome: String = connection
            .query_row(
                "SELECT outcome FROM audit WHERE action='memory.emergency' AND target=?1",
                [&current.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outcome, "ignored-stale-generation");
        let retained: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM emergency_records WHERE event_id=?1",
                [&record.event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1);
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn token_snapshot_verifies_hmac_without_plaintext_database_storage() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let created = actor
            .create_token(
                "bootstrap",
                "create-one",
                TokenCreateRequest {
                    name: "reader".into(),
                    scopes: vec![TokenScope::ModelsRead],
                    allowed_cidrs: Vec::new(),
                    expires_at: None,
                    max_concurrent_inference: 1,
                },
            )
            .await
            .unwrap();
        let bearer = created.bearer_token.unwrap();
        let secret = bearer.rsplit_once('_').unwrap().1;
        let snapshot = actor.auth_snapshot().await.unwrap();
        assert!(snapshot.tokens[&created.token.id].verify(&SecretString::from(PEPPER), secret));
        assert!(!fs::read(root.path().join("state.sqlite3"))
            .unwrap()
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn partial_snapshot_never_promotes_and_alias_requires_explicit_move() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        assert!(actor.list_models().await.unwrap().is_empty());
        let first = crate::spark::wire::ModelDocument {
            schema: crate::spark::wire::MODEL_SCHEMA.into(),
            id: "m_first".into(),
            canonical: "huggingface:owner/model@1111111111111111111111111111111111111111".into(),
            repository: "owner/model".into(),
            commit: "1111111111111111111111111111111111111111".into(),
            snapshot: "models--owner--model/snapshots/1111111111111111111111111111111111111111"
                .into(),
            logical_bytes: 10,
            unique_bytes: 10,
            aliases: vec!["model:latest".into()],
            active_instances: Vec::new(),
            transport: "rust-xet".into(),
            verified_at: "2026-08-24T00:00:00Z".into(),
            gated: false,
            license: Some("mit".into()),
        };
        actor.promote_model(first.clone(), false).await.unwrap();
        assert_eq!(actor.list_models().await.unwrap(), vec![first.clone()]);
        let mut second = first;
        second.id = "m_second".into();
        second.commit = "2222222222222222222222222222222222222222".into();
        second.canonical = format!("huggingface:owner/model@{}", second.commit);
        assert!(matches!(
            actor.promote_model(second.clone(), false).await,
            Err(StateError::Conflict(_))
        ));
        actor.promote_model(second.clone(), true).await.unwrap();
        assert_eq!(actor.model("model:latest").await.unwrap().id, second.id);
        actor.shutdown().unwrap();
    }

    #[tokio::test]
    async fn selected_functional_evidence_is_durable_and_objective_scoped() {
        let root = tempfile::tempdir().unwrap();
        let actor = actor(root.path());
        let evaluation = CompatibilityEvaluationDocument {
            schema: crate::spark::wire::COMPATIBILITY_EVALUATION_SCHEMA.into(),
            id: "evidence-1".into(),
            model_id: "model-1".into(),
            repository: "owner/model".into(),
            commit: "1".repeat(40),
            objective: "agent".into(),
            selected_recipe_id: Some("recipe-a".into()),
            selected_fingerprint: Some(format!("sha256:{}", "a".repeat(64))),
            fallback_recipe_id: Some("vllm".into()),
            candidates: Vec::new(),
            created_at: "2026-08-25T00:00:00Z".into(),
            invalidated_reason: None,
        };
        actor.store_evaluation(evaluation.clone()).await.unwrap();
        assert_eq!(
            actor.selected_evaluation("model-1", "agent").await.unwrap(),
            Some(evaluation.clone())
        );
        let mut invalidated = evaluation;
        invalidated.invalidated_reason = Some("fingerprint changed".into());
        actor.store_evaluation(invalidated).await.unwrap();
        assert_eq!(
            actor.selected_evaluation("model-1", "agent").await.unwrap(),
            None
        );
        actor.shutdown().unwrap();
    }
}

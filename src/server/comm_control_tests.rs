
use super::{handle_comm_assign_next, handle_comm_assign_task, handle_comm_task_control};
use crate::agent::Agent;
use crate::message::{Message, StreamEvent, ToolDefinition};
use crate::plan::PlanItem;
use crate::protocol::ServerEvent;
use crate::provider::{EventStream, Provider};
use crate::server::comm_await::{CommAwaitMembersContext, handle_comm_await_members};
use crate::server::{
    AwaitMembersRuntime, SwarmEvent, SwarmEventType, SwarmMember, SwarmMutationRuntime,
    VersionedPlan,
};
use crate::tool::Registry;
use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};

struct RuntimeEnvGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
    prev_runtime: Option<std::ffi::OsString>,
}

impl RuntimeEnvGuard {
    fn new() -> (Self, tempfile::TempDir) {
        let guard = crate::storage::lock_test_env();
        let temp = tempfile::TempDir::new().expect("create runtime dir");
        let prev_runtime = std::env::var_os("JCODE_RUNTIME_DIR");
        crate::env::set_var("JCODE_RUNTIME_DIR", temp.path());
        (
            Self {
                _guard: guard,
                prev_runtime,
            },
            temp,
        )
    }
}

impl Drop for RuntimeEnvGuard {
    fn drop(&mut self) {
        if let Some(prev_runtime) = self.prev_runtime.take() {
            crate::env::set_var("JCODE_RUNTIME_DIR", prev_runtime);
        } else {
            crate::env::remove_var("JCODE_RUNTIME_DIR");
        }
    }
}

fn member(session_id: &str, swarm_id: &str, status: &str) -> SwarmMember {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    SwarmMember {
        session_id: session_id.to_string(),
        event_tx,
        event_txs: HashMap::new(),
        working_dir: None,
        swarm_id: Some(swarm_id.to_string()),
        swarm_enabled: true,
        status: status.to_string(),
        detail: None,
        friendly_name: Some(session_id.to_string()),
        report_back_to_session_id: None,
        role: "agent".to_string(),
        joined_at: Instant::now(),
        last_status_change: Instant::now(),
        is_headless: false,
    }
}

fn plan_item(id: &str, status: &str, priority: &str, blocked_by: &[&str]) -> PlanItem {
    PlanItem {
        content: format!("task {id}"),
        status: status.to_string(),
        priority: priority.to_string(),
        id: id.to_string(),
        subsystem: None,
        file_scope: Vec::new(),
        blocked_by: blocked_by.iter().map(|value| value.to_string()).collect(),
        assigned_to: None,
    }
}

fn swarm_event(session_id: &str, swarm_id: &str, event: SwarmEventType) -> SwarmEvent {
    SwarmEvent {
        id: 1,
        session_id: session_id.to_string(),
        session_name: Some(session_id.to_string()),
        swarm_id: Some(swarm_id.to_string()),
        event,
        timestamp: Instant::now(),
        absolute_time: SystemTime::now(),
    }
}

#[derive(Default)]
struct TestProvider;

#[async_trait]
impl Provider for TestProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        Ok(Box::pin(stream::iter(vec![Ok(StreamEvent::MessageEnd {
            stop_reason: None,
        })])))
    }

    fn name(&self) -> &str {
        "test"
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(Self)
    }
}

async fn test_agent() -> Arc<Mutex<Agent>> {
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let registry = Registry::new(provider.clone()).await;
    Arc::new(Mutex::new(Agent::new(provider, registry)))
}

#[tokio::test]
async fn assign_task_without_task_id_picks_highest_priority_runnable_task() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-assign";
    let requester = "coord";
    let worker = "worker";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (worker.to_string(), member(worker, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), worker.to_string()]),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![
                plan_item("done", "completed", "high", &[]),
                plan_item("blocked", "queued", "high", &["high-ready"]),
                plan_item("low-ready", "queued", "low", &["done"]),
                plan_item("high-ready", "queued", "high", &["done"]),
            ],
            version: 1,
            participants: HashSet::from([requester.to_string(), worker.to_string()]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    handle_comm_assign_task(
        77,
        requester.to_string(),
        Some(worker.to_string()),
        None,
        Some("Pick the next task".to_string()),
        &client_tx,
        &sessions,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mutation_runtime,
    )
    .await;

    let response = client_rx.recv().await.expect("response");
    match response {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 77);
            assert_eq!(task_id, "high-ready");
            assert_eq!(target_session, worker);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }

    let plans = swarm_plans.read().await;
    let plan = plans.get(swarm_id).expect("plan exists");
    let selected = plan
        .items
        .iter()
        .find(|item| item.id == "high-ready")
        .expect("selected task exists");
    assert_eq!(selected.assigned_to.as_deref(), Some(worker));
    assert_eq!(selected.status, "queued");

    let blocked = plan
        .items
        .iter()
        .find(|item| item.id == "blocked")
        .expect("blocked task exists");
    assert!(
        blocked.assigned_to.is_none(),
        "blocked task should not be auto-assigned"
    );
}

#[tokio::test]
async fn assign_task_rejects_explicit_blocked_task() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-blocked";
    let requester = "coord";
    let worker = "worker";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let worker_agent = test_agent().await;
    let sessions = Arc::new(RwLock::new(HashMap::from([(
        worker.to_string(),
        worker_agent,
    )])));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (worker.to_string(), member(worker, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), worker.to_string()]),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![
                plan_item("setup", "completed", "high", &[]),
                plan_item("blocked", "queued", "high", &["missing-prereq"]),
            ],
            version: 1,
            participants: HashSet::from([requester.to_string(), worker.to_string()]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    handle_comm_assign_task(
        88,
        requester.to_string(),
        Some(worker.to_string()),
        Some("blocked".to_string()),
        None,
        &client_tx,
        &sessions,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::Error { message, .. } => {
            assert!(message.contains("missing dependencies") || message.contains("blocked"));
        }
        other => panic!("expected error for blocked task assignment, got {other:?}"),
    }

    let plans = swarm_plans.read().await;
    let blocked = plans[swarm_id]
        .items
        .iter()
        .find(|item| item.id == "blocked")
        .expect("blocked task exists");
    assert!(
        blocked.assigned_to.is_none(),
        "blocked task should stay unassigned"
    );
}

#[tokio::test]
async fn assign_task_without_target_picks_ready_agent() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-auto-target";
    let requester = "coord";
    let ready_worker = "worker-ready";
    let completed_worker = "worker-completed";
    let running_worker = "worker-running";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            ready_worker.to_string(),
            member(ready_worker, swarm_id, "ready"),
        ),
        (
            completed_worker.to_string(),
            member(completed_worker, swarm_id, "completed"),
        ),
        (
            running_worker.to_string(),
            member(running_worker, swarm_id, "running"),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            ready_worker.to_string(),
            completed_worker.to_string(),
            running_worker.to_string(),
        ]),
    )])));
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![
                plan_item("setup", "completed", "high", &[]),
                plan_item("next", "queued", "high", &["setup"]),
            ],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                ready_worker.to_string(),
                completed_worker.to_string(),
                running_worker.to_string(),
            ]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    handle_comm_assign_task(
        99,
        requester.to_string(),
        None,
        None,
        Some("Pick a task and worker".to_string()),
        &client_tx,
        &sessions,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 99);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, ready_worker);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn assign_task_without_target_prefers_less_loaded_ready_agent() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-auto-target-load";
    let requester = "coord";
    let less_loaded = "worker-light";
    let more_loaded = "worker-busy";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            less_loaded.to_string(),
            member(less_loaded, swarm_id, "ready"),
        ),
        (
            more_loaded.to_string(),
            member(more_loaded, swarm_id, "ready"),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            less_loaded.to_string(),
            more_loaded.to_string(),
        ]),
    )])));
    let mut busy_existing = plan_item("busy-existing", "running", "high", &[]);
    busy_existing.assigned_to = Some(more_loaded.to_string());
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![
                plan_item("setup", "completed", "high", &[]),
                busy_existing,
                plan_item("next", "queued", "high", &["setup"]),
            ],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                less_loaded.to_string(),
                more_loaded.to_string(),
            ]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    handle_comm_assign_task(
        100,
        requester.to_string(),
        None,
        None,
        Some("Pick the least-loaded worker".to_string()),
        &client_tx,
        &sessions,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 100);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, less_loaded);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn task_control_wake_returns_structured_response_with_plan_summary() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-task-control";
    let requester = "coord";
    let worker = "worker";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let worker_agent = test_agent().await;
    let sessions = Arc::new(RwLock::new(HashMap::from([(
        worker.to_string(),
        worker_agent,
    )])));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (worker.to_string(), member(worker, swarm_id, "ready")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), worker.to_string()]),
    )])));
    let mut assigned = plan_item("active-task", "queued", "high", &[]);
    assigned.assigned_to = Some(worker.to_string());
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![assigned, plan_item("next", "queued", "high", &[])],
            version: 1,
            participants: HashSet::from([requester.to_string(), worker.to_string()]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();

    handle_comm_task_control(
        101,
        requester.to_string(),
        "wake".to_string(),
        "active-task".to_string(),
        Some(worker.to_string()),
        Some("continue".to_string()),
        &client_tx,
        &sessions,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommTaskControlResponse {
            id,
            action,
            task_id,
            target_session,
            status,
            summary,
        } => {
            assert_eq!(id, 101);
            assert_eq!(action, "wake");
            assert_eq!(task_id, "active-task");
            assert_eq!(target_session.as_deref(), Some(worker));
            assert_eq!(status, "running");
            assert_eq!(summary.item_count, 2);
            assert!(summary.ready_ids.contains(&"next".to_string()));
        }
        other => panic!("expected CommTaskControlResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn assign_next_prefers_worker_with_dependency_context() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-context-score";
    let requester = "coord";
    let context_worker = "worker-context";
    let other_worker = "worker-other";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            context_worker.to_string(),
            member(context_worker, swarm_id, "ready"),
        ),
        (
            other_worker.to_string(),
            member(other_worker, swarm_id, "ready"),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            context_worker.to_string(),
            other_worker.to_string(),
        ]),
    )])));
    let mut dependency = plan_item("dep", "completed", "high", &[]);
    dependency.assigned_to = Some(context_worker.to_string());
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![dependency, plan_item("next", "queued", "high", &["dep"])],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                context_worker.to_string(),
                other_worker.to_string(),
            ]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    handle_comm_assign_next(
        102,
        requester.to_string(),
        None,
        None,
        None,
        None,
        &client_tx,
        &sessions,
        &global_session_id,
        &provider,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mcp_pool,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 102);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, context_worker);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn assign_next_prefers_worker_with_matching_subsystem_metadata() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-metadata-score";
    let requester = "coord";
    let metadata_worker = "worker-metadata";
    let other_worker = "worker-other";
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let sessions = Arc::new(RwLock::new(HashMap::new()));
    let soft_interrupt_queues = Arc::new(RwLock::new(HashMap::new()));
    let client_connections = Arc::new(RwLock::new(HashMap::new()));
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), {
            let mut member = member(requester, swarm_id, "ready");
            member.role = "coordinator".to_string();
            member
        }),
        (
            metadata_worker.to_string(),
            member(metadata_worker, swarm_id, "ready"),
        ),
        (
            other_worker.to_string(),
            member(other_worker, swarm_id, "ready"),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            metadata_worker.to_string(),
            other_worker.to_string(),
        ]),
    )])));
    let mut prior = plan_item("prior", "completed", "high", &[]);
    prior.subsystem = Some("parser".to_string());
    prior.file_scope = vec!["src/parser.rs".to_string()];
    prior.assigned_to = Some(metadata_worker.to_string());
    let mut next = plan_item("next", "queued", "high", &[]);
    next.subsystem = Some("parser".to_string());
    next.file_scope = vec!["src/parser.rs".to_string()];
    let swarm_plans = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        VersionedPlan {
            items: vec![prior, next],
            version: 1,
            participants: HashSet::from([
                requester.to_string(),
                metadata_worker.to_string(),
                other_worker.to_string(),
            ]),
            task_progress: HashMap::new(),
        },
    )])));
    let swarm_coordinators = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        requester.to_string(),
    )])));
    let event_history = Arc::new(RwLock::new(VecDeque::new()));
    let event_counter = Arc::new(AtomicU64::new(1));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);
    let mutation_runtime = SwarmMutationRuntime::default();
    let provider: Arc<dyn Provider> = Arc::new(TestProvider);
    let global_session_id = Arc::new(RwLock::new(String::new()));
    let mcp_pool = Arc::new(crate::mcp::SharedMcpPool::from_default_config());

    handle_comm_assign_next(
        103,
        requester.to_string(),
        None,
        None,
        None,
        None,
        &client_tx,
        &sessions,
        &global_session_id,
        &provider,
        &soft_interrupt_queues,
        &client_connections,
        &swarm_members,
        &swarms_by_id,
        &swarm_plans,
        &swarm_coordinators,
        &event_history,
        &event_counter,
        &swarm_event_tx,
        &mcp_pool,
        &mutation_runtime,
    )
    .await;

    match client_rx.recv().await.expect("response") {
        ServerEvent::CommAssignTaskResponse {
            id,
            task_id,
            target_session,
        } => {
            assert_eq!(id, 103);
            assert_eq!(task_id, "next");
            assert_eq!(target_session, metadata_worker);
        }
        other => panic!("expected CommAssignTaskResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn await_members_includes_late_joiners_when_watching_swarm() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-a";
    let requester = "req";
    let initial_peer = "peer-1";
    let late_peer = "peer-2";
    let await_runtime = AwaitMembersRuntime::default();

    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (
            initial_peer.to_string(),
            member(initial_peer, swarm_id, "running"),
        ),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), initial_peer.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);

    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        None,
        Some(2),
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    {
        let mut members = swarm_members.write().await;
        members.insert(
            late_peer.to_string(),
            member(late_peer, swarm_id, "running"),
        );
    }
    {
        let mut swarms = swarms_by_id.write().await;
        swarms
            .get_mut(swarm_id)
            .expect("swarm exists")
            .insert(late_peer.to_string());
    }
    let _ = swarm_event_tx.send(swarm_event(
        late_peer,
        swarm_id,
        SwarmEventType::MemberChange {
            action: "joined".to_string(),
        },
    ));

    {
        let mut members = swarm_members.write().await;
        members
            .get_mut(initial_peer)
            .expect("initial peer exists")
            .status = "completed".to_string();
    }
    let _ = swarm_event_tx.send(swarm_event(
        initial_peer,
        swarm_id,
        SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "completed".to_string(),
        },
    ));

    {
        let mut members = swarm_members.write().await;
        members.get_mut(late_peer).expect("late peer exists").status = "completed".to_string();
    }
    let _ = swarm_event_tx.send(swarm_event(
        late_peer,
        swarm_id,
        SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "completed".to_string(),
        },
    ));

    let response = tokio::time::timeout(std::time::Duration::from_secs(1), client_rx.recv())
        .await
        .expect("response should arrive")
        .expect("channel should stay open");

    match response {
        ServerEvent::CommAwaitMembersResponse {
            completed, members, ..
        } => {
            assert!(completed, "await should complete after both peers finish");
            let watched: HashSet<String> = members.into_iter().map(|m| m.session_id).collect();
            assert!(watched.contains(initial_peer));
            assert!(watched.contains(late_peer));
        }
        other => panic!("expected CommAwaitMembersResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn await_members_stops_when_requesting_client_disconnects() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-b";
    let requester = "req";
    let peer = "peer-1";
    let await_runtime = AwaitMembersRuntime::default();

    let (client_tx, client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer.to_string(), member(peer, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), peer.to_string()]),
    )])));
    let (swarm_event_tx, swarm_event_rx) = broadcast::channel(32);
    drop(swarm_event_rx);
    let baseline_receivers = swarm_event_tx.receiver_count();

    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        None,
        Some(60),
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    drop(client_rx);

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if swarm_event_tx.receiver_count() == baseline_receivers {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("await task should unsubscribe promptly after client disconnect");
}

#[tokio::test]
async fn await_members_any_mode_returns_after_first_match() {
    let (_env, _runtime) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-any";
    let requester = "req";
    let peer_a = "peer-a";
    let peer_b = "peer-b";
    let await_runtime = AwaitMembersRuntime::default();

    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer_a.to_string(), member(peer_a, swarm_id, "running")),
        (peer_b.to_string(), member(peer_b, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([
            requester.to_string(),
            peer_a.to_string(),
            peer_b.to_string(),
        ]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);

    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        Some("any".to_string()),
        Some(60),
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    {
        let mut members = swarm_members.write().await;
        members.get_mut(peer_a).expect("peer a exists").status = "completed".to_string();
    }
    let _ = swarm_event_tx.send(swarm_event(
        peer_a,
        swarm_id,
        SwarmEventType::StatusChange {
            old_status: "running".to_string(),
            new_status: "completed".to_string(),
        },
    ));

    let response = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
        .await
        .expect("response should arrive")
        .expect("channel should stay open");

    match response {
        ServerEvent::CommAwaitMembersResponse {
            completed,
            members,
            summary,
            ..
        } => {
            assert!(
                completed,
                "await any should complete after first member matches"
            );
            assert!(
                summary.contains("peer-a"),
                "summary should mention matched member"
            );
            let done_members: Vec<_> = members.into_iter().filter(|member| member.done).collect();
            assert_eq!(done_members.len(), 1);
            assert_eq!(done_members[0].session_id, peer_a);
        }
        other => panic!("expected CommAwaitMembersResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn await_members_reuses_persisted_deadline_after_reload_retry() {
    let (_env, _runtime_dir) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-c";
    let requester = "req";
    let peer = "peer-1";
    let key = crate::server::await_members_state::request_key(
        requester,
        swarm_id,
        &[],
        &["completed".to_string()],
        None,
    );
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    crate::server::await_members_state::save_state(
        &crate::server::await_members_state::PersistedAwaitMembersState {
            key,
            session_id: requester.to_string(),
            swarm_id: swarm_id.to_string(),
            target_status: vec!["completed".to_string()],
            requested_ids: vec![],
            mode: None,
            created_at_unix_ms: now_ms,
            deadline_unix_ms: now_ms + 150,
            final_response: None,
        },
    );

    let await_runtime = AwaitMembersRuntime::default();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([
        (requester.to_string(), member(requester, swarm_id, "ready")),
        (peer.to_string(), member(peer, swarm_id, "running")),
    ])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string(), peer.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);

    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        None,
        Some(60),
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    let started = Instant::now();
    let response = tokio::time::timeout(Duration::from_secs(1), client_rx.recv())
        .await
        .expect("response should arrive")
        .expect("channel should stay open");

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "persisted deadline should win over new timeout"
    );

    match response {
        ServerEvent::CommAwaitMembersResponse {
            completed, summary, ..
        } => {
            assert!(!completed, "persisted expired wait should time out");
            assert!(summary.contains("Timed out"));
        }
        other => panic!("expected CommAwaitMembersResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn await_members_returns_persisted_final_response_after_reload_retry() {
    let (_env, _runtime_dir) = RuntimeEnvGuard::new();
    let swarm_id = "swarm-d";
    let requester = "req";
    let key = crate::server::await_members_state::request_key(
        requester,
        swarm_id,
        &[],
        &["completed".to_string()],
        None,
    );
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    crate::server::await_members_state::save_state(
        &crate::server::await_members_state::PersistedAwaitMembersState {
            key,
            session_id: requester.to_string(),
            swarm_id: swarm_id.to_string(),
            target_status: vec!["completed".to_string()],
            requested_ids: vec![],
            mode: None,
            created_at_unix_ms: now_ms,
            deadline_unix_ms: now_ms + 60_000,
            final_response: Some(
                crate::server::await_members_state::PersistedAwaitMembersResult {
                    completed: true,
                    members: vec![crate::protocol::AwaitedMemberStatus {
                        session_id: "peer-1".to_string(),
                        friendly_name: Some("peer-1".to_string()),
                        status: "completed".to_string(),
                        done: true,
                    }],
                    summary: "All 1 members are done: peer-1".to_string(),
                    resolved_at_unix_ms: now_ms,
                },
            ),
        },
    );

    let await_runtime = AwaitMembersRuntime::default();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel();
    let swarm_members = Arc::new(RwLock::new(HashMap::from([(
        requester.to_string(),
        member(requester, swarm_id, "ready"),
    )])));
    let swarms_by_id = Arc::new(RwLock::new(HashMap::from([(
        swarm_id.to_string(),
        HashSet::from([requester.to_string()]),
    )])));
    let (swarm_event_tx, _swarm_event_rx) = broadcast::channel(32);

    handle_comm_await_members(
        1,
        requester.to_string(),
        vec!["completed".to_string()],
        vec![],
        None,
        Some(60),
        CommAwaitMembersContext {
            client_event_tx: &client_tx,
            swarm_members: &swarm_members,
            swarms_by_id: &swarms_by_id,
            swarm_event_tx: &swarm_event_tx,
            await_members_runtime: &await_runtime,
        },
    )
    .await;

    match client_rx.recv().await.expect("response should arrive") {
        ServerEvent::CommAwaitMembersResponse {
            completed,
            summary,
            members,
            ..
        } => {
            assert!(completed);
            assert_eq!(summary, "All 1 members are done: peer-1");
            assert_eq!(members.len(), 1);
            assert_eq!(members[0].session_id, "peer-1");
        }
        other => panic!("expected CommAwaitMembersResponse, got {other:?}"),
    }
}

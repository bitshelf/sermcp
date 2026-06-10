//! Thin project policy around rmcp's SEP-2663 task runtime.
//!
//! rmcp owns the task state machine, TTL handling, input routing, and
//! cooperative cancellation semantics.  This wrapper adds only the policy
//! that each DUT may run at most one long-running operation.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use rmcp::{
    model::{CreateTaskResult, GetTaskResult},
    task_manager::{TaskContext, TaskExit, TaskManager as RmcpTaskManager, TaskOptions},
};
use tokio::sync::Semaphore;

pub const TASK_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const TASK_POLL_INTERVAL_MS: u64 = 1_000;

pub struct TaskManager {
    inner: RmcpTaskManager,
    slots: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl TaskManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RmcpTaskManager::new(),
            slots: Mutex::new(HashMap::new()),
        })
    }

    /// Materialize an asynchronously executing task, or return `None` when
    /// the default DUT already has a long-running operation.
    pub fn spawn<F, Fut>(
        &self,
        status_message: impl Into<String>,
        operation: F,
    ) -> Option<CreateTaskResult>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<rmcp::model::CallToolResult, TaskExit>> + Send + 'static,
    {
        self.spawn_for("default", status_message, operation)
    }

    /// Independent per-DUT permits share one task store for polling/cancellation.
    pub fn spawn_for<F, Fut>(
        &self,
        dut: &str,
        status_message: impl Into<String>,
        operation: F,
    ) -> Option<CreateTaskResult>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<rmcp::model::CallToolResult, TaskExit>> + Send + 'static,
    {
        let slot = self
            .slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(dut.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone();
        let permit = slot.try_acquire_owned().ok()?;
        let options = TaskOptions::new()
            .with_ttl_ms(TASK_TTL.as_millis() as u64)
            .with_poll_interval_ms(TASK_POLL_INTERVAL_MS)
            .with_status_message(status_message);
        let task = self.inner.spawn(options, move |context| {
            Box::pin(async move {
                let _permit = permit;
                operation(context).await
            })
        });
        Some(CreateTaskResult::new(task))
    }

    pub fn get(&self, task_id: &str) -> Result<GetTaskResult, rmcp::ErrorData> {
        self.inner.get_task(task_id).map(GetTaskResult::new)
    }

    pub fn update<I>(&self, task_id: &str, input_responses: I) -> Result<(), rmcp::ErrorData>
    where
        I: IntoIterator<Item = (String, serde_json::Value)>,
    {
        self.inner.update_task(task_id, input_responses)
    }

    pub fn cancel(&self, task_id: &str) -> Result<(), rmcp::ErrorData> {
        self.inner.cancel_task(task_id)
    }

    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::{
        model::{CallToolResult, ContentBlock, TaskStatus},
        task_manager::TaskExit,
    };

    #[tokio::test]
    async fn independent_duts_can_run_tasks_concurrently() {
        let tasks = TaskManager::new();
        let first = tasks
            .spawn_for("board-a", "a", |context| async move {
                context.cancelled().await;
                Err(TaskExit::Cancelled)
            })
            .unwrap();
        let second = tasks
            .spawn_for("board-b", "b", |context| async move {
                context.cancelled().await;
                Err(TaskExit::Cancelled)
            })
            .unwrap();
        assert!(
            tasks
                .spawn_for("board-a", "duplicate", |_| async { unreachable!() })
                .is_none()
        );
        tasks.cancel(&first.task.task_id).unwrap();
        assert!(
            !tasks
                .get(&second.task.task_id)
                .unwrap()
                .task
                .status()
                .is_terminal()
        );
        tasks.shutdown();
    }

    #[tokio::test]
    async fn one_running_task_per_dut_and_cooperative_cancel() {
        let tasks = TaskManager::new();
        let created = tasks
            .spawn("working", |context| async move {
                context.cancelled().await;
                Err(TaskExit::Cancelled)
            })
            .unwrap();
        assert!(
            tasks
                .spawn("second", |_| async { unreachable!() })
                .is_none()
        );

        tasks.cancel(&created.task.task_id).unwrap();
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let current = tasks.get(&created.task.task_id).unwrap();
            if current.task.status().is_terminal() {
                assert_eq!(current.task.status(), TaskStatus::Cancelled);
                return;
            }
        }
        panic!("cancelled task did not settle");
    }

    #[tokio::test]
    async fn unknown_input_keys_are_ignored() {
        let tasks = TaskManager::new();
        let created = tasks
            .spawn("working", |_| async {
                Ok(CallToolResult::success(vec![ContentBlock::text("done")]))
            })
            .unwrap();
        tasks
            .update(
                &created.task.task_id,
                [("not-outstanding".to_string(), serde_json::json!({}))],
            )
            .unwrap();
    }
}

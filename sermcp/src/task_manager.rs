//! Thin project policy around rmcp's SEP-2663 task runtime.
//!
//! rmcp owns the task state machine, TTL handling, input routing, and
//! cooperative cancellation semantics.  This wrapper adds only the policy
//! that one MCP process may run at most one long-running DUT operation.

use std::{future::Future, sync::Arc, time::Duration};

use rmcp::{
    model::{CreateTaskResult, GetTaskResult},
    task_manager::{TaskContext, TaskExit, TaskManager as RmcpTaskManager, TaskOptions},
};
use tokio::sync::Semaphore;

pub const TASK_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const TASK_POLL_INTERVAL_MS: u64 = 1_000;

pub struct TaskManager {
    inner: RmcpTaskManager,
    slot: Arc<Semaphore>,
}

impl TaskManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RmcpTaskManager::new(),
            slot: Arc::new(Semaphore::new(1)),
        })
    }

    /// Materialize an asynchronously executing task, or return `None` when
    /// this DUT already has a long-running operation.  Each MCP process owns
    /// one selected DUT, so a process-local permit is also the per-DUT guard.
    pub fn spawn<F, Fut>(
        &self,
        status_message: impl Into<String>,
        operation: F,
    ) -> Option<CreateTaskResult>
    where
        F: FnOnce(TaskContext) -> Fut + Send + 'static,
        Fut: Future<Output = Result<rmcp::model::CallToolResult, TaskExit>> + Send + 'static,
    {
        let permit = Arc::clone(&self.slot).try_acquire_owned().ok()?;
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

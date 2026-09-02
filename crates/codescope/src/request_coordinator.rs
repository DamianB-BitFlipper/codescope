//! FIFO lifetime management for active AI requests.
//!
//! Only the debounced current selection starts inference. Moving to another selection does
//! not cancel work already sent to the provider: completed plans are cached under their
//! original selection. The active window is bounded to 16 requests; admitting another
//! request aborts the oldest active generation.

use std::collections::HashMap;

use tokio::task::AbortHandle;

/// Maximum number of provider requests that may remain active at once.
const DEFAULT_MAX_ACTIVE: usize = 16;

#[derive(Debug)]
struct ActiveRequest {
    order: u64,
    abort: AbortHandle,
}

/// One coordinator owns every active AI request and its cancellation handle.
#[derive(Debug)]
pub(crate) struct RequestCoordinator {
    max_active: usize,
    active: HashMap<u64, ActiveRequest>,
    next_order: u64,
}

impl Default for RequestCoordinator {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ACTIVE)
    }
}

impl Drop for RequestCoordinator {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl RequestCoordinator {
    fn new(max_active: usize) -> Self {
        Self {
            max_active: max_active.max(1),
            active: HashMap::new(),
            next_order: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.active.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// Make room for one new request. Returns the generation cancelled when the active
    /// window was already full. Requests below the cap are never cancelled merely because
    /// focus moved elsewhere.
    pub(crate) fn admit(&mut self) -> Option<u64> {
        if self.active.len() < self.max_active {
            return None;
        }
        let oldest = self
            .active
            .iter()
            .min_by_key(|(_, request)| request.order)
            .map(|(generation, _)| *generation);
        if let Some(generation) = oldest {
            self.abort(generation);
        }
        oldest
    }

    pub(crate) fn register(&mut self, generation: u64, abort: AbortHandle) {
        debug_assert!(self.active.len() < self.max_active);
        self.next_order = self.next_order.saturating_add(1);
        self.active.insert(
            generation,
            ActiveRequest {
                order: self.next_order,
                abort,
            },
        );
    }

    pub(crate) fn complete(&mut self, generation: u64) {
        self.active.remove(&generation);
    }

    pub(crate) fn abort(&mut self, generation: u64) {
        if let Some(active) = self.active.remove(&generation) {
            active.abort.abort();
        }
    }

    pub(crate) fn abort_all(&mut self) {
        for (_, active) in self.active.drain() {
            active.abort.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_task() -> tokio::task::JoinHandle<()> {
        tokio::spawn(std::future::pending::<()>())
    }

    #[tokio::test]
    async fn focus_changes_do_not_cancel_requests_below_the_cap() {
        let mut coordinator = RequestCoordinator::new(3);
        let first = pending_task();
        assert_eq!(coordinator.admit(), None);
        coordinator.register(1, first.abort_handle());

        let second = pending_task();
        assert_eq!(coordinator.admit(), None);
        coordinator.register(2, second.abort_handle());

        assert_eq!(coordinator.len(), 2);
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        coordinator.abort_all();
    }

    #[tokio::test]
    async fn seventeenth_request_cancels_the_oldest_active_generation() {
        let mut coordinator = RequestCoordinator::default();
        let mut tasks = Vec::new();
        for generation in 1..=16 {
            let task = pending_task();
            assert_eq!(coordinator.admit(), None);
            coordinator.register(generation, task.abort_handle());
            tasks.push(task);
        }

        assert_eq!(coordinator.admit(), Some(1));
        assert_eq!(coordinator.len(), 15);
        let newest = pending_task();
        coordinator.register(17, newest.abort_handle());
        assert_eq!(coordinator.len(), 16);
        assert!(tasks.remove(0).await.unwrap_err().is_cancelled());
        for task in tasks {
            assert!(!task.is_finished());
        }
        coordinator.abort_all();
    }

    #[tokio::test]
    async fn completion_frees_capacity_without_cancelling_anything() {
        let mut coordinator = RequestCoordinator::new(2);
        let first = pending_task();
        coordinator.register(1, first.abort_handle());
        coordinator.complete(1);

        assert_eq!(coordinator.admit(), None);
        assert!(coordinator.is_empty());
        first.abort();
    }
}

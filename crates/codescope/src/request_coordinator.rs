//! Central admission and cancellation policy for asynchronous AI plan requests.
//!
//! The target is intentionally soft: interactive work may burst above it, and focused
//! work may preempt the oldest lower-priority overflow request. The absolute ceiling is a
//! final process-safety bound, not the normal operating concurrency.

use std::collections::HashMap;

use tokio::task::AbortHandle;

/// Lower values are more important.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum RequestPriority {
    Focused = 0,
    Interactive = 1,
    Background = 2,
}

/// Normal concurrency, interactive burst capacity, and the absolute process ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestLimits {
    pub(crate) target_in_flight: usize,
    pub(crate) overflow_in_flight: usize,
    pub(crate) absolute_max: usize,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            target_in_flight: 4,
            overflow_in_flight: 12,
            absolute_max: 64,
        }
    }
}

#[derive(Debug)]
struct ActiveRequest {
    priority: RequestPriority,
    order: u64,
    overflow: bool,
    abort: AbortHandle,
}

/// Result of asking the coordinator for capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Admitted { preempted: Option<u64> },
    Queued,
}

/// One coordinator owns all active AI requests and their cancellation handles.
#[derive(Debug)]
pub(crate) struct RequestCoordinator {
    limits: RequestLimits,
    active: HashMap<u64, ActiveRequest>,
    next_order: u64,
}

impl Default for RequestCoordinator {
    fn default() -> Self {
        Self::new(RequestLimits::default())
    }
}

impl Drop for RequestCoordinator {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl RequestCoordinator {
    pub(crate) fn new(mut limits: RequestLimits) -> Self {
        limits.target_in_flight = limits.target_in_flight.max(1);
        limits.absolute_max = limits.absolute_max.max(limits.target_in_flight);
        limits.overflow_in_flight = limits
            .overflow_in_flight
            .max(limits.target_in_flight)
            .min(limits.absolute_max);
        Self {
            limits,
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

    /// Reserve capacity conceptually. If focused work reaches the burst boundary, cancel
    /// the oldest lower-priority request that was itself admitted as overflow (FIFO
    /// dequeue). Only an all-focused workload may grow beyond that boundary, up to 64.
    pub(crate) fn admit(&mut self, priority: RequestPriority) -> Admission {
        let active = self.active.len();
        if active < self.limits.target_in_flight {
            return Admission::Admitted { preempted: None };
        }
        if priority <= RequestPriority::Interactive && active < self.limits.overflow_in_flight {
            return Admission::Admitted { preempted: None };
        }
        if priority != RequestPriority::Focused {
            return Admission::Queued;
        }

        if let Some(victim) = self.oldest_lower_priority(priority, true) {
            self.abort(victim);
            return Admission::Admitted {
                preempted: Some(victim),
            };
        }
        if active < self.limits.absolute_max {
            return Admission::Admitted { preempted: None };
        }
        if let Some(victim) = self.oldest_lower_priority(priority, false) {
            self.abort(victim);
            return Admission::Admitted {
                preempted: Some(victim),
            };
        }
        Admission::Queued
    }

    pub(crate) fn register(&mut self, id: u64, priority: RequestPriority, abort: AbortHandle) {
        debug_assert!(self.active.len() < self.limits.absolute_max);
        self.next_order = self.next_order.saturating_add(1);
        self.active.insert(
            id,
            ActiveRequest {
                priority,
                order: self.next_order,
                overflow: self.active.len() >= self.limits.target_in_flight,
                abort,
            },
        );
    }

    pub(crate) fn reprioritize(&mut self, id: u64, priority: RequestPriority) {
        if let Some(active) = self.active.get_mut(&id) {
            active.priority = priority;
        }
    }

    pub(crate) fn complete(&mut self, id: u64) {
        self.active.remove(&id);
    }

    pub(crate) fn abort(&mut self, id: u64) {
        if let Some(active) = self.active.remove(&id) {
            active.abort.abort();
        }
    }

    pub(crate) fn abort_all(&mut self) {
        for (_, active) in self.active.drain() {
            active.abort.abort();
        }
    }

    fn oldest_lower_priority(&self, priority: RequestPriority, overflow_only: bool) -> Option<u64> {
        self.active
            .iter()
            .filter(|(_, request)| {
                request.priority > priority && (!overflow_only || request.overflow)
            })
            .min_by_key(|(_, request)| request.order)
            .map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active_handle() -> AbortHandle {
        tokio::spawn(std::future::pending::<()>()).abort_handle()
    }

    #[tokio::test]
    async fn background_stops_at_target_and_interactive_bursts_to_three_times_target() {
        let mut coordinator = RequestCoordinator::default();
        for id in 1..=4 {
            assert_eq!(
                coordinator.admit(RequestPriority::Background),
                Admission::Admitted { preempted: None }
            );
            coordinator.register(id, RequestPriority::Background, active_handle());
        }
        assert_eq!(
            coordinator.admit(RequestPriority::Background),
            Admission::Queued
        );
        for id in 5..=12 {
            assert_eq!(
                coordinator.admit(RequestPriority::Interactive),
                Admission::Admitted { preempted: None }
            );
            coordinator.register(id, RequestPriority::Interactive, active_handle());
        }
        assert_eq!(coordinator.len(), 12);
        assert_eq!(
            coordinator.admit(RequestPriority::Interactive),
            Admission::Queued
        );
        coordinator.abort_all();
    }

    #[tokio::test]
    async fn focused_request_dequeues_oldest_lower_priority_overflow() {
        let mut coordinator = RequestCoordinator::default();
        for id in 1..=4 {
            assert!(matches!(
                coordinator.admit(RequestPriority::Background),
                Admission::Admitted { .. }
            ));
            coordinator.register(id, RequestPriority::Background, active_handle());
        }
        let oldest_overflow = tokio::spawn(std::future::pending::<()>());
        assert!(matches!(
            coordinator.admit(RequestPriority::Interactive),
            Admission::Admitted { .. }
        ));
        coordinator.register(
            5,
            RequestPriority::Interactive,
            oldest_overflow.abort_handle(),
        );
        for id in 6..=12 {
            assert!(matches!(
                coordinator.admit(RequestPriority::Interactive),
                Admission::Admitted { .. }
            ));
            coordinator.register(id, RequestPriority::Interactive, active_handle());
        }
        assert_eq!(
            coordinator.admit(RequestPriority::Focused),
            Admission::Admitted { preempted: Some(5) }
        );
        assert_eq!(coordinator.len(), 11);
        assert!(
            oldest_overflow.await.unwrap_err().is_cancelled(),
            "dequeue must abort the provider task, not merely discard its metadata"
        );
        coordinator.abort_all();
    }

    #[tokio::test]
    async fn all_focused_work_can_burst_but_never_exceed_absolute_max() {
        let mut coordinator = RequestCoordinator::new(RequestLimits {
            target_in_flight: 1,
            overflow_in_flight: 3,
            absolute_max: 4,
        });
        for id in 1..=4 {
            assert!(matches!(
                coordinator.admit(RequestPriority::Focused),
                Admission::Admitted { .. }
            ));
            coordinator.register(id, RequestPriority::Focused, active_handle());
        }
        assert_eq!(
            coordinator.admit(RequestPriority::Focused),
            Admission::Queued
        );
        coordinator.abort_all();
        assert!(coordinator.is_empty());
    }
}

//! Stable in-process priority queue for host-owned work items.
//!
//! [`QueueManager`](crate::QueueManager) owns and executes async commands. Some
//! hosts instead need to keep typed payloads locally and decide when execution
//! starts, while still sharing Lane's priority and FIFO semantics. This module
//! provides that smaller scheduling primitive without forcing payloads through
//! JSON or an async command adapter.

use crate::Priority;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// One queued value together with its stable scheduling metadata.
#[derive(Debug)]
pub struct PriorityItem<T> {
    priority: Priority,
    sequence: u64,
    value: T,
}

impl<T> PriorityItem<T> {
    /// Lower values are scheduled first.
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// Monotonic insertion order used for FIFO scheduling within a priority.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Borrow the queued value.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Mutably borrow the queued value.
    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Consume the scheduling metadata and return the queued value.
    pub fn into_value(self) -> T {
        self.value
    }
}

impl<T> PartialEq for PriorityItem<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.sequence == other.sequence
    }
}

impl<T> Eq for PriorityItem<T> {}

impl<T> Ord for PriorityItem<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap. Reverse both keys so lower priority values
        // run first and equal-priority values remain FIFO.
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl<T> PartialOrd for PriorityItem<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Typed, stable priority queue for host-controlled execution.
///
/// The queue never executes values itself. Lower numeric priorities are
/// scheduled first, and insertion order is preserved within each priority.
#[derive(Debug)]
pub struct PriorityQueue<T> {
    entries: BinaryHeap<PriorityItem<T>>,
    next_sequence: u64,
}

impl<T> Default for PriorityQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PriorityQueue<T> {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self {
            entries: BinaryHeap::new(),
            next_sequence: 0,
        }
    }

    /// Number of pending values.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue has no pending values.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Submit a value and return its stable insertion sequence.
    pub fn push(&mut self, priority: Priority, value: T) -> u64 {
        self.ensure_sequence_capacity();
        self.next_sequence += 1;
        let sequence = self.next_sequence;
        self.entries.push(PriorityItem {
            priority,
            sequence,
            value,
        });
        sequence
    }

    /// Claim the highest-priority pending value.
    pub fn pop(&mut self) -> Option<PriorityItem<T>> {
        self.entries.pop()
    }

    /// Return a previously claimed item without changing its original FIFO
    /// position. This is intended for admission failures and other cases where
    /// execution never acquired ownership of the work.
    pub fn restore(&mut self, item: PriorityItem<T>) {
        self.next_sequence = self.next_sequence.max(item.sequence);
        self.entries.push(item);
    }

    /// Borrow pending items in the exact order in which they will be claimed.
    pub fn ordered(&self) -> Vec<&PriorityItem<T>> {
        let mut entries = self.entries.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.sequence.cmp(&right.sequence))
        });
        entries
    }

    /// Remove all pending values.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_sequence = 0;
    }

    fn ensure_sequence_capacity(&mut self) {
        if self.next_sequence != u64::MAX {
            return;
        }

        // Rebase only the FIFO key. Priority remains authoritative, and
        // ordering within every priority is preserved by the old sequence.
        let mut entries = self.entries.drain().collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.sequence);
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.sequence = index as u64 + 1;
        }
        self.next_sequence = entries.len() as u64;
        self.entries.extend(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_priority_runs_first_and_equal_priority_is_fifo() {
        let mut queue = PriorityQueue::new();
        queue.push(5, "background");
        queue.push(1, "first user");
        queue.push(1, "second user");
        queue.push(3, "continuation");

        let drained =
            std::iter::from_fn(|| queue.pop().map(PriorityItem::into_value)).collect::<Vec<_>>();

        assert_eq!(
            drained,
            ["first user", "second user", "continuation", "background"]
        );
    }

    #[test]
    fn restore_preserves_original_fifo_position() {
        let mut queue = PriorityQueue::new();
        queue.push(1, "first");
        queue.push(1, "second");

        let first = queue.pop().unwrap();
        queue.push(1, "third");
        queue.restore(first);

        let drained =
            std::iter::from_fn(|| queue.pop().map(PriorityItem::into_value)).collect::<Vec<_>>();
        assert_eq!(drained, ["first", "second", "third"]);
    }

    #[test]
    fn ordered_matches_claim_order_without_mutating_the_queue() {
        let mut queue = PriorityQueue::new();
        queue.push(4, "later");
        queue.push(0, "first");
        queue.push(4, "last");

        let ordered = queue
            .ordered()
            .into_iter()
            .map(|entry| *entry.value())
            .collect::<Vec<_>>();

        assert_eq!(ordered, ["first", "later", "last"]);
        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn clear_resets_pending_values_and_sequence() {
        let mut queue = PriorityQueue::new();
        assert_eq!(queue.push(1, "old"), 1);
        queue.clear();

        assert!(queue.is_empty());
        assert_eq!(queue.push(1, "new"), 1);
    }
}

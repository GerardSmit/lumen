//! Language-neutral toggle notification coalescing for host-owned task queues.
//!
//! The first transition supplies the old state; later transitions for the same
//! key replace the new state and source, including transitions back to the old
//! state. Hosts admit one task per new entry and cancel failed admissions.
//! This module never schedules or dispatches.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareError {
    SequenceExhausted,
    AllocationFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Prepared {
    Coalesced,
    New(u64),
}

pub struct PendingToggle<K, S, P> {
    pub id: u64,
    pub key: K,
    pub old_state: S,
    pub new_state: S,
    pub source: P,
}

/// An owner's pending notifications. Keys can include node and kind, keeping
/// independent trackers distinct without host-specific knowledge.
pub struct ToggleTasks<K, S, P> {
    next_id: u64,
    pending: Vec<PendingToggle<K, S, P>>,
}

impl<K, S, P> Default for ToggleTasks<K, S, P> {
    fn default() -> Self {
        Self { next_id: 0, pending: Vec::new() }
    }
}

impl<K: PartialEq, S, P> ToggleTasks<K, S, P> {
    /// Prepare before host task admission. Coalescing consumes no sequence
    /// number and requires no allocation, even at sequence exhaustion.
    pub fn prepare(&mut self, key: K, old_state: S, new_state: S, source: P)
        -> Result<Prepared, PrepareError>
    {
        if let Some(existing) = self.pending.iter_mut().find(|task| task.key == key) {
            existing.new_state = new_state;
            existing.source = source;
            return Ok(Prepared::Coalesced);
        }
        let id = self.next_id.checked_add(1).ok_or(PrepareError::SequenceExhausted)?;
        self.pending.try_reserve(1).map_err(|_| PrepareError::AllocationFailed)?;
        self.next_id = id;
        self.pending.push(PendingToggle { id, key, old_state, new_state, source });
        Ok(Prepared::New(id))
    }

    /// Remove before dispatch so a reentrant transition can prepare a new task
    /// for the same key, with its own first old state.
    pub fn take(&mut self, id: u64) -> Option<PendingToggle<K, S, P>> {
        self.pending.iter().position(|task| task.id == id)
            .map(|index| self.pending.remove(index))
    }

    /// Roll back failed admission or cancel a queued notification. IDs are never
    /// reused: a stale callback cannot consume a replacement task.
    pub fn cancel(&mut self, id: u64) -> Option<PendingToggle<K, S, P>> {
        self.take(id)
    }

    /// A host may move a pending target without moving/requeueing its task.
    /// Update that exact notification, preserving its first old state and id.
    pub fn update(&mut self, id: u64, new_state: S, source: P) -> bool {
        let Some(task) = self.pending.iter_mut().find(|task| task.id == id) else { return false; };
        task.new_state = new_state;
        task.source = source;
        true
    }

    pub fn is_empty(&self) -> bool { self.pending.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_id(prepared: Result<Prepared, PrepareError>) -> u64 {
        match prepared.unwrap() { Prepared::New(id) => id, _ => panic!("expected new task") }
    }

    #[test]
    fn first_old_and_latest_new_source_survive_equal_state_coalescing() {
        let mut tasks = ToggleTasks::default();
        let id = new_id(tasks.prepare((7,0),"closed","open","initial"));
        assert_eq!(tasks.prepare((7,0),"open","closed","latest"),Ok(Prepared::Coalesced));
        let task = tasks.take(id).unwrap();
        assert_eq!((task.old_state,task.new_state,task.source),("closed","closed","latest"));
        assert!(tasks.is_empty());
    }

    #[test]
    fn adopted_notification_updates_exact_id_preserving_first_state() {
        let mut tasks = ToggleTasks::default();
        let id = new_id(tasks.prepare(7, "closed", "open", "initial"));
        let peer = new_id(tasks.prepare(8, "closed", "open", "peer"));
        assert!(tasks.update(id, "closed", "adopted"));
        assert!(!tasks.update(id + 99, "open", "stale"));
        let task = tasks.take(id).unwrap();
        assert_eq!((task.old_state, task.new_state, task.source), ("closed", "closed", "adopted"));
        assert!(!tasks.update(id, "open", "consumed"));
        assert_eq!(tasks.take(peer).unwrap().source, "peer");
    }

    #[test]
    fn node_and_kind_are_independent_and_removal_preserves_other_tasks() {
        let mut tasks = ToggleTasks::default();
        let dialog = new_id(tasks.prepare((7,0),false,true,1));
        let popover = new_id(tasks.prepare((7,1),false,true,2));
        let peer = new_id(tasks.prepare((8,0),true,false,3));
        assert_eq!(tasks.prepare((7,0),true,false,4),Ok(Prepared::Coalesced));
        assert_eq!(tasks.take(popover).unwrap().source,2);
        assert_eq!(tasks.take(dialog).unwrap().source,4);
        assert_eq!(tasks.take(peer).unwrap().source,3);
        assert!(tasks.is_empty());
    }

    #[test]
    fn removal_allows_reentrant_task_and_stale_callback_is_inert() {
        let mut tasks = ToggleTasks::default();
        let first = new_id(tasks.prepare(7,"closed","open",()));
        let dispatched = tasks.take(first).unwrap();
        let second = new_id(tasks.prepare(7,"open","closed",()));
        assert!(second > first);
        assert!(tasks.take(first).is_none());
        assert_eq!(dispatched.old_state,"closed");
        assert_eq!(tasks.take(second).unwrap().old_state,"open");
    }

    #[test]
    fn failed_admission_rolls_back_only_new_entry_without_reusing_id() {
        let mut tasks = ToggleTasks::default();
        let admitted = new_id(tasks.prepare(1,false,true,"kept"));
        let failed = new_id(tasks.prepare(2,false,true,"failed"));
        // An admission error from the host queue rolls back this preparation.
        assert_eq!(tasks.cancel(failed).unwrap().source,"failed");
        let retried = new_id(tasks.prepare(2,true,false,"retry"));
        assert!(retried > failed);
        assert!(tasks.take(failed).is_none());
        assert_eq!(tasks.take(admitted).unwrap().source,"kept");
        assert_eq!(tasks.cancel(retried).unwrap().source,"retry");
        assert!(tasks.is_empty());
    }

    #[test]
    fn exhausted_sequence_keeps_pending_and_allows_coalescing() {
        let mut tasks = ToggleTasks::default();
        tasks.next_id = u64::MAX - 1;
        let id = new_id(tasks.prepare(1,false,true,1));
        assert_eq!(id,u64::MAX);
        assert_eq!(tasks.prepare(2,false,true,2),Err(PrepareError::SequenceExhausted));
        assert_eq!(tasks.prepare(1,true,false,3),Ok(Prepared::Coalesced));
        let task = tasks.take(id).unwrap();
        assert_eq!((task.old_state,task.new_state,task.source),(false,false,3));
        assert!(tasks.is_empty());
    }
}

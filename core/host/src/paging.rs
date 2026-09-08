//! The resume policy (spec/observation.md §5): what to send for a
//! resource's next page, including after an interruption -- an adapter
//! crash and a fresh, memory-less relaunch -- mid-read.
//!
//! Public and reusable on purpose (frozen contract, section (g)): the
//! conformance runner calls this rather than reimplementing resumption, so
//! A5 (exact-resume) checks one implementation instead of trusting two to
//! agree.
//!
//! `sumer refresh` is this module's caller inside the host: it drives a
//! resource's page loop through one [`ResumeState`], which is also what
//! makes "a resumed sweep never retracts" checkable -- a sweep that did
//! not begin at `page: None` fails the first gate in
//! `spec/observation.md` §8.1.
//!
//! The three `cursor_resumable` families answer one question differently:
//! *what do I resend if I have to start this read over right now?* That is
//! the only question [`ResumeState::next_request`] answers -- a page loop
//! advances on the reply's own `next`, because two of the three families
//! have a durable point that never moves.
//!
//! - `exact` -- the durable point is the `next` from the last **completed**
//!   page. Every completed page is a safe place to resume from.
//! - `batch_restart` -- the durable point is the batch's start and never
//!   moves. Every resume -- no matter how many pages already succeeded --
//!   resends the value the batch *started* with, and a drained batch
//!   (`next: None`) has nothing to advance *to*: `PageRequest` has no slot
//!   for "where a future batch should start," so there is no terminal
//!   resume state for this family. Deferred until a real paginating
//!   provider defines it (spec/observation.md 5).
//! - `none` -- there is no durable point at all; every resume resends the
//!   original `Window`, relying on `local_id` dedup (`sumer_host::fold`) to
//!   avoid double-counting.

use sumer_wire::{CursorResumable, PageRequest};

/// Tracks how to resume one resource's paginated read, across any number of
/// pages and (if it happens) an interruption.
#[derive(Debug, Clone)]
pub struct ResumeState {
    /// What the read started from. `None` per Ruling A8 (Contract
    /// Amendment 1): "from the start of available history."
    start: Option<PageRequest>,
    resumable: Option<CursorResumable>,
    /// The durable resume point under `exact` only -- the most recent
    /// `next` a completed page returned.
    exact_next: Option<PageRequest>,
}

impl ResumeState {
    #[must_use]
    pub fn new(start: Option<PageRequest>) -> ResumeState {
        ResumeState {
            start,
            resumable: None,
            exact_next: None,
        }
    }

    /// **The durable resume point**: what to resend if this read had to be
    /// started over right now, on a fresh connection with no memory of it.
    /// That is the one question this answers, and it is the one the caller
    /// persists between pages.
    ///
    /// It is **not** "the next page of an uninterrupted read". An earlier
    /// version of this comment claimed both questions get the same answer;
    /// its first real caller (`sumer-store`'s sweep) says otherwise and is
    /// right. Under `batch_restart` and `none` the durable point never
    /// moves, so a page loop driven by this value asks for the batch start
    /// for ever and never advances. A loop advances on the page reply's own
    /// `status.page.next`; it consults this only for the cursor it writes
    /// down.
    ///
    /// `None` once an `exact` read has fully drained (a page returned
    /// `next: None`) -- the only family with a terminal state here.
    /// `batch_restart` always resends its batch start and `none` always
    /// resends its original window, drained or not.
    #[must_use]
    pub fn next_request(&self) -> Option<PageRequest> {
        match self.resumable {
            None => self.start.clone(),
            Some(CursorResumable::Exact) => self.exact_next.clone(),
            Some(CursorResumable::BatchRestart | CursorResumable::None) => self.start.clone(),
        }
    }

    /// Records a page reply that was received in full (never call this for
    /// a page an interruption cut short -- that is exactly what
    /// `next_request` is for instead).
    pub fn record(&mut self, resumable: CursorResumable, next: Option<PageRequest>) {
        self.resumable = Some(resumable);
        if resumable == CursorResumable::Exact {
            self.exact_next = next;
        }
        // `BatchRestart`/`None`: `next` is never adopted as the resume
        // point at all -- intermediate or final. Only `exact` has a durable
        // point that moves, and only it has a drained/terminal state.
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn cursor(s: &str) -> PageRequest {
        PageRequest::Cursor {
            cursor: s.to_owned(),
        }
    }

    #[test]
    fn absent_start_means_from_the_beginning_of_history() {
        let state = ResumeState::new(None);
        assert!(state.next_request().is_none());
    }

    #[test]
    fn exact_resumes_from_the_last_completed_pages_next() {
        let mut state = ResumeState::new(Some(cursor("height-0")));
        state.record(CursorResumable::Exact, Some(cursor("height-100")));
        // A crash right here resumes from the last *completed* page.
        match state.next_request() {
            Some(PageRequest::Cursor { cursor }) => assert_eq!(cursor, "height-100"),
            other => panic!("expected a cursor, got {other:?}"),
        }
    }

    #[test]
    fn exact_reaches_a_terminal_none_when_drained() {
        let mut state = ResumeState::new(Some(cursor("height-0")));
        state.record(CursorResumable::Exact, Some(cursor("height-100")));
        state.record(CursorResumable::Exact, None);
        assert!(
            state.next_request().is_none(),
            "fully drained, nothing left to fetch"
        );
    }

    #[test]
    fn batch_restart_always_resends_the_original_start_mid_batch() {
        let mut state = ResumeState::new(Some(cursor("batch-start")));
        // Several pages succeed, each with a non-null `next` -- none of
        // that is ever trusted as a resume point.
        state.record(
            CursorResumable::BatchRestart,
            Some(cursor("intermediate-1")),
        );
        state.record(
            CursorResumable::BatchRestart,
            Some(cursor("intermediate-2")),
        );
        match state.next_request() {
            Some(PageRequest::Cursor { cursor }) => assert_eq!(cursor, "batch-start"),
            other => panic!("expected the original batch start, got {other:?}"),
        }
    }

    #[test]
    fn batch_restart_still_resends_start_once_the_batch_finishes() {
        // `next: null` marks the batch done, but there is no "next batch"
        // concept in this milestone's PageRequest -- the durable point for
        // *this* batch stays its own start. This is the one rule
        // spec/observation.md 5, the module docs above, and `record` all
        // state; it is pinned here so they cannot drift apart again.
        let mut state = ResumeState::new(Some(cursor("batch-start")));
        state.record(CursorResumable::BatchRestart, Some(cursor("intermediate")));
        state.record(CursorResumable::BatchRestart, None);
        match state.next_request() {
            Some(PageRequest::Cursor { cursor }) => assert_eq!(cursor, "batch-start"),
            other => panic!("expected batch start, got {other:?}"),
        }
    }

    #[test]
    fn none_resumable_always_resends_the_original_window() {
        let window = PageRequest::Window {
            resource_id: "acct1".to_owned(),
            asset: None,
            start: sumer_wire::Rfc3339::new("2026-01-01T00:00:00Z").unwrap(),
            end: sumer_wire::Rfc3339::new("2026-02-01T00:00:00Z").unwrap(),
        };
        let mut state = ResumeState::new(Some(window.clone()));
        state.record(CursorResumable::None, None);
        match state.next_request() {
            Some(PageRequest::Window { resource_id, .. }) => assert_eq!(resource_id, "acct1"),
            other => panic!("expected the original window, got {other:?}"),
        }
        let _ = window;
    }
}

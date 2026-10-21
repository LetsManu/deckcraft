//! Live co-editing in the session (see `deckcraft_collab`).
//!
//! The host (the web app inside OfficeHub) connects the active presentation to a shared session:
//! the first person [seeds](Session::collab_seed) it, everyone else [joins](Session::collab_join).
//! After that the host sends what [`Session::collab_outgoing`] returns and hands every update from
//! the others to [`Session::collab_incoming`]. Commands, undo and the UI work as before; undo and
//! redo take back only this person's own steps.

use std::sync::Arc;

use deckcraft_collab::Collab;

use crate::{EngineError, Result, Session, clamp_selection};

/// A presentation's co-editing state. Copies of a document state do not take part in the session.
#[derive(Default)]
pub struct Shared {
    pub(crate) collab: Option<Box<Collab>>,
    /// Updates made by commands (undo, finished steps) waiting for the host to send them.
    pub(crate) outbox: Vec<Vec<u8>>,
}

impl Clone for Shared {
    fn clone(&self) -> Self {
        Shared::default()
    }
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").field("active", &self.collab.is_some()).field("outbox", &self.outbox.len()).finish()
    }
}

impl Shared {
    pub fn is_active(&self) -> bool {
        self.collab.is_some()
    }
}

/// End an undo step: write it to the shared document now and start the next one.
pub(crate) fn end_step(st: &mut crate::DocState) {
    let doc = st.doc.clone();
    if let Some(c) = st.shared.collab.as_mut() {
        if let Some(u) = c.local(&doc) {
            st.shared.outbox.push(u);
        }
        c.step();
    }
}

/// Undo or redo in a shared session; `None` when there is no session.
pub(crate) fn undo(st: &mut crate::DocState, redo: bool) -> Option<Result<bool>> {
    let doc = st.doc.clone();
    let c = st.shared.collab.as_mut()?;
    Some(match c.undo(&doc, redo) {
        Ok(Some((p, update))) => {
            st.doc = p;
            st.shared.outbox.push(update);
            st.interaction = None;
            st.revision += 1;
            clamp_selection(st);
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(e) => Err(EngineError::Other(e.to_string())),
    })
}

impl Session {
    /// Whether the active presentation is shared right now.
    pub fn collab_active(&self) -> bool {
        self.active().is_some_and(|d| d.shared.is_active())
    }

    /// Start a shared session with the active presentation as it is (the first person). Returns
    /// the update that describes it, for the server to hand to everyone who joins.
    pub fn collab_seed(&mut self, slot: u32, client: u64) -> Result<Vec<u8>> {
        let st = self.active_mut().ok_or(EngineError::NoDocument)?;
        let mut c = Box::new(Collab::new(slot, client));
        let update = c.seed(&st.doc);
        let mut p = (*st.doc).clone();
        p.next_id = c.next_id();
        let p = Arc::new(p);
        // What the shared document now says, so the next diff is only the next edit.
        c.local(&p);
        c.step();
        st.doc = p.clone();
        st.saved_doc = p;
        st.history = Default::default();
        st.shared = crate::collab::Shared { collab: Some(c), outbox: Vec::new() };
        Ok(update)
    }

    /// Join a shared session: the active presentation becomes the shared one (`updates` is
    /// everything the server kept so far).
    pub fn collab_join(&mut self, slot: u32, client: u64, updates: &[Vec<u8>]) -> Result<()> {
        let st = self.active_mut().ok_or(EngineError::NoDocument)?;
        let mut c = Box::new(Collab::new(slot, client));
        let p = Arc::new(c.join(updates).map_err(|e| EngineError::Other(e.to_string()))?);
        st.doc = p.clone();
        st.saved_doc = p;
        st.history = Default::default();
        st.interaction = None;
        st.selection = Default::default();
        st.revision += 1;
        clamp_selection(st);
        st.shared = Shared { collab: Some(c), outbox: Vec::new() };
        Ok(())
    }

    /// Leave the shared session (the presentation stays as it is).
    pub fn collab_leave(&mut self) {
        if let Some(st) = self.active_mut() {
            st.shared = Shared::default();
        }
    }

    /// What to send to the others now (local edits since the last call), if anything. Call it
    /// every frame; during a drag the others see the shape move.
    pub fn collab_outgoing(&mut self) -> Option<Vec<u8>> {
        let st = self.active_mut()?;
        let doc = st.doc.clone();
        let mut out = std::mem::take(&mut st.shared.outbox);
        if let Some(u) = st.shared.collab.as_mut()?.local(&doc) {
            out.push(u);
        }
        (!out.is_empty()).then(|| deckcraft_collab::merge(&out))
    }

    /// Merge an update from someone else into the active presentation.
    pub fn collab_incoming(&mut self, update: &[u8]) -> Result<()> {
        let st = self.active_mut().ok_or(EngineError::NoDocument)?;
        let doc = st.doc.clone();
        let Some(c) = st.shared.collab.as_mut() else { return Ok(()) };
        let (p, pending) = c.remote(update, &doc).map_err(|e| EngineError::Other(e.to_string()))?;
        if let Some(u) = pending {
            st.shared.outbox.push(u);
        }
        if !Arc::ptr_eq(&p, &st.doc) {
            // The undo snapshots no longer match; shared undo works on the shared document.
            st.doc = p;
            st.revision += 1;
            clamp_selection(st);
        }
        Ok(())
    }

    /// The whole shared document as one update (the server keeps it instead of the many small
    /// ones it received).
    pub fn collab_state(&self) -> Option<Vec<u8>> {
        self.active()?.shared.collab.as_ref().map(|c| c.state())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn two() -> (Session, Session) {
        let mut a = Session::new();
        crate::sample::open_sample(&mut a).unwrap();
        let seed = a.collab_seed(0, 11).unwrap();
        let mut b = Session::new();
        crate::sample::open_sample(&mut b).unwrap();
        b.collab_join(1, 12, &[seed]).unwrap();
        (a, b)
    }

    fn sync(from: &mut Session, to: &mut Session) {
        if let Some(u) = from.collab_outgoing() {
            to.collab_incoming(&u).unwrap();
        }
    }

    fn same(a: &Session, b: &Session) -> bool {
        let mut x = (*a.active().unwrap().doc).clone();
        let mut y = (*b.active().unwrap().doc).clone();
        x.next_id = 0;
        y.next_id = 0;
        x == y
    }

    #[test]
    fn commands_on_both_sides_converge() {
        let (mut a, mut b) = two();
        assert!(same(&a, &b));
        a.execute("slide.new", &json!({})).unwrap();
        a.execute("shape.insert", &json!({"preset": "star5", "rect": [100, 100, 200, 200]})).unwrap();
        b.execute("shape.insert", &json!({"preset": "rect", "rect": [300, 100, 100, 100]})).unwrap();
        sync(&mut a, &mut b);
        sync(&mut b, &mut a);
        assert!(same(&a, &b));
    }

    #[test]
    fn undo_takes_back_only_my_command() {
        let (mut a, mut b) = two();
        let shapes = |s: &Session| s.active().unwrap().doc.slides.iter().map(|sl| sl.shapes.len()).sum::<usize>();
        let start = shapes(&a);
        a.execute("shape.insert", &json!({"preset": "star5", "rect": [100, 100, 200, 200]})).unwrap();
        sync(&mut a, &mut b);
        b.execute("shape.insert", &json!({"preset": "rect", "rect": [300, 100, 100, 100]})).unwrap();
        sync(&mut b, &mut a);
        assert_eq!(shapes(&a), start + 2);
        a.execute("edit.undo", &json!({})).unwrap();
        sync(&mut a, &mut b);
        assert!(same(&a, &b));
        assert_eq!(shapes(&a), start + 1, "B's shape stays");
        a.execute("edit.redo", &json!({})).unwrap();
        sync(&mut a, &mut b);
        assert!(same(&a, &b));
        assert_eq!(shapes(&b), start + 2);
    }
}

//! In memory lifecycle for policy tests and shell wiring.
//! The GTK shell mirrors these transitions onto real WebViews.

use crate::tab::{TabId, TabState};
use std::collections::HashMap;

/// Tracks tab states. No WebKit calls here.
#[derive(Debug, Default)]
pub struct Lifecycle {
    states: HashMap<u64, TabState>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, id: TabId) {
        self.states.insert(id.0, TabState::Standby);
    }

    pub fn mark_active(&mut self, id: TabId) {
        self.states.insert(id.0, TabState::Active);
    }

    pub fn freeze(&mut self, id: TabId) {
        if self.is_tracked(id) && self.state(id) != TabState::Parked {
            self.states.insert(id.0, TabState::Idle);
        }
    }

    pub fn park(&mut self, id: TabId) {
        if self.is_tracked(id) {
            self.states.insert(id.0, TabState::Parked);
        }
    }

    pub fn restore(&mut self, id: TabId) {
        if self.is_tracked(id) {
            self.states.insert(id.0, TabState::Active);
        }
    }

    pub fn state(&self, id: TabId) -> TabState {
        self.states.get(&id.0).copied().unwrap_or(TabState::Standby)
    }

    fn is_tracked(&self, id: TabId) -> bool {
        self.states.contains_key(&id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_through_states() {
        let mut lc = Lifecycle::new();
        let id = TabId(7);
        lc.add(id);
        lc.mark_active(id);
        assert_eq!(lc.state(id), TabState::Active);
        lc.freeze(id);
        assert_eq!(lc.state(id), TabState::Idle);
        lc.park(id);
        assert_eq!(lc.state(id), TabState::Parked);
        lc.restore(id);
        assert_eq!(lc.state(id), TabState::Active);
    }

    #[test]
    fn freeze_never_drops_to_parked_directly() {
        let mut lc = Lifecycle::new();
        let id = TabId(3);
        lc.add(id);
        lc.mark_active(id);
        lc.freeze(id);
        assert_ne!(lc.state(id), TabState::Parked);
    }

    #[test]
    fn unknown_tab_defaults_to_standby() {
        let lc = Lifecycle::new();
        assert_eq!(lc.state(TabId(99)), TabState::Standby);
    }

    #[test]
    fn transitions_ignore_unknown_tabs() {
        let mut lc = Lifecycle::new();
        let id = TabId(42);
        lc.freeze(id);
        lc.park(id);
        lc.restore(id);
        assert_eq!(lc.state(id), TabState::Standby);
    }
}

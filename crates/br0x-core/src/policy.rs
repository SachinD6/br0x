//! Eviction policy: pure rules, no I/O.
//! Exemptions always beat timeouts.

use crate::tab::{Action, Exemption, PolicyParams, SysState, TabId, TabSnapshot};

/// One sweep over background tabs. The shell calls this on a 5s tick
/// and applies freeze/park to real WebViews. Pure, no I/O.
pub fn sweep(tabs: &[TabSnapshot], sys: &SysState) -> Vec<(TabId, Action)> {
    tabs.iter().map(|t| (t.id, decide(t, sys))).filter(|(_, a)| *a != Action::Keep).collect()
}

fn bucket_params(tab_count: usize) -> PolicyParams {
    if tab_count <= 5 {
        PolicyParams { standby_secs: 30, freeze_secs: 300, park_secs: 900 }
    } else if tab_count <= 15 {
        PolicyParams { standby_secs: 30, freeze_secs: 120, park_secs: 300 }
    } else {
        PolicyParams { standby_secs: 15, freeze_secs: 60, park_secs: 180 }
    }
}

/// Default numbers for the 9 tab target. Used for UI and logs.
pub fn describe() -> PolicyParams {
    bucket_params(9)
}

/// Bucket numbers for a given tab count. Exposed for UI tuning.
pub fn params_for(tab_count: usize) -> PolicyParams {
    bucket_params(tab_count)
}

fn is_recently_restored(tab: &TabSnapshot) -> bool {
    tab.restored_secs_ago.is_some_and(|s| s < 60)
}

/// All exemptions that apply to this tab right now.
pub fn list_exemptions(tab: &TabSnapshot) -> Vec<Exemption> {
    let mut out = Vec::with_capacity(3);
    if tab.audible {
        out.push(Exemption::Audible);
    }
    if tab.capturing {
        out.push(Exemption::Capturing);
    }
    if tab.downloading {
        out.push(Exemption::Downloading);
    }
    if tab.form_dirty {
        out.push(Exemption::FormDirty);
    }
    if tab.pinned {
        out.push(Exemption::Pinned);
    }
    if tab.keep_alive {
        out.push(Exemption::KeepAlive);
    }
    if is_recently_restored(tab) {
        out.push(Exemption::RecentlyRestored);
    }
    out
}

fn blocks_freeze(tab: &TabSnapshot) -> bool {
    tab.audible || tab.capturing
}

fn blocks_park(tab: &TabSnapshot) -> bool {
    !list_exemptions(tab).is_empty()
}

fn pressure_scale(mem_used_percent: f64) -> f64 {
    if mem_used_percent >= 85.0 {
        0.25
    } else if mem_used_percent >= 70.0 {
        0.5
    } else {
        1.0
    }
}

fn scaled(base: u64, scale: f64) -> u64 {
    ((base as f64 * scale) as u64).max(15)
}

/// Pure decision. The caller passes a snapshot and obeys the action.
pub fn decide(tab: &TabSnapshot, sys: &SysState) -> Action {
    let base = bucket_params(sys.tab_count);
    let scale = pressure_scale(sys.mem_used_percent);
    let freeze_at = scaled(base.freeze_secs, scale);
    let park_at = scaled(base.park_secs, scale);
    decide_at(tab, sys, freeze_at, park_at)
}

fn decide_at(tab: &TabSnapshot, sys: &SysState, freeze_at: u64, park_at: u64) -> Action {
    if blocks_freeze(tab) {
        return Action::Keep;
    }
    if is_critical_park(tab, sys) {
        return Action::Park;
    }
    if tab.last_active_secs_ago >= park_at && !blocks_park(tab) {
        return Action::Park;
    }
    if tab.last_active_secs_ago >= freeze_at {
        return Action::Freeze;
    }
    Action::Keep
}

fn is_critical_park(tab: &TabSnapshot, sys: &SysState) -> bool {
    sys.mem_used_percent >= 85.0 && tab.last_active_secs_ago >= 60 && !blocks_park(tab)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tab::TabId;

    fn snap(idle_secs: u64) -> TabSnapshot {
        TabSnapshot {
            id: TabId(1),
            last_active_secs_ago: idle_secs,
            audible: false,
            capturing: false,
            downloading: false,
            form_dirty: false,
            pinned: false,
            keep_alive: false,
            restored_secs_ago: None,
        }
    }

    fn sys() -> SysState {
        SysState { tab_count: 9, mem_used_percent: 40.0 }
    }

    #[test]
    fn keeps_recent_tabs() {
        assert_eq!(decide(&snap(10), &sys()), Action::Keep);
    }

    #[test]
    fn freezes_idle_tabs() {
        assert_eq!(decide(&snap(200), &sys()), Action::Freeze);
    }

    #[test]
    fn parks_old_tabs() {
        assert_eq!(decide(&snap(1000), &sys()), Action::Park);
    }

    #[test]
    fn audible_beats_timeout() {
        let mut t = snap(5000);
        t.audible = true;
        assert_eq!(decide(&t, &sys()), Action::Keep);
        assert!(list_exemptions(&t).contains(&Exemption::Audible));
    }

    #[test]
    fn dirty_form_blocks_park_but_allows_freeze() {
        let mut t = snap(1000);
        t.form_dirty = true;
        assert_eq!(decide(&t, &sys()), Action::Freeze);
    }

    #[test]
    fn warn_pressure_halves_timeouts() {
        let sys = SysState { tab_count: 9, mem_used_percent: 75.0 };
        assert_eq!(decide(&snap(100), &sys), Action::Freeze);
    }

    #[test]
    fn critical_pressure_parks_early() {
        let sys = SysState { tab_count: 9, mem_used_percent: 90.0 };
        assert_eq!(decide(&snap(70), &sys), Action::Park);
    }

    #[test]
    fn recently_restored_blocks_park() {
        let mut t = snap(1000);
        t.restored_secs_ago = Some(10);
        assert_eq!(decide(&t, &sys()), Action::Freeze);
    }

    #[test]
    fn sweep_returns_only_actionable_tabs() {
        let tabs = vec![snap(10), snap(200), snap(1000)];
        let out = sweep(&tabs, &sys());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|(_, a)| *a != Action::Keep));
    }
}

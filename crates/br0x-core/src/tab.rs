//! Shared tab types for policy and lifecycle.
//! No I/O here. Plain data only.

use serde::{Deserialize, Serialize};

/// Opaque tab handle. The shell owns the WebView, core owns the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TabId(pub u64);

/// Lifecycle stage of a tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TabState {
    Active,
    Standby,
    Idle,
    Parked,
    /// Time-based release without pressure. Same mechanism as Parked.
    Sleeping,
}

/// What the policy wants the caller to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Keep,
    Freeze,
    Park,
    /// Release the web process like Park, reported as sleeping.
    Sleep,
}

/// User visible reason that blocks park. Only audible and capturing tabs
/// also block freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exemption {
    Audible,
    Capturing,
    Downloading,
    FormDirty,
    Pinned,
    KeepAlive,
    RecentlyRestored,
}

/// Minimal snapshot the UI passes to `policy::decide`.
/// Times are seconds to keep the interface small and testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabSnapshot {
    pub id: TabId,
    pub last_active_secs_ago: u64,
    pub audible: bool,
    pub capturing: bool,
    pub downloading: bool,
    pub form_dirty: bool,
    pub pinned: bool,
    pub keep_alive: bool,
    /// Seconds since this tab was restored, if recently restored.
    pub restored_secs_ago: Option<u64>,
}

/// System state that scales timeouts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SysState {
    pub tab_count: usize,
    pub mem_used_percent: f64,
}

/// Active numbers for UI and logs. Tunable without code change later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolicyParams {
    pub standby_secs: u64,
    pub freeze_secs: u64,
    pub park_secs: u64,
    /// Wall-clock idle time before Sleep. Not pressure-scaled.
    pub sleep_secs: u64,
}

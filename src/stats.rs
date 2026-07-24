use std::collections::HashMap;

use serde::Serialize;

/// A point-in-time snapshot of the engine's memory table.
///
/// Every field is computed fresh from SQLite at the moment `MemoryEngine::stats()`
/// is called — there is no caching or background bookkeeping, so this always
/// reflects the current on-disk state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MemoryStats {
    /// Total rows in the `memories` table (active, superseded, and expired
    /// rows are all counted here).
    pub total_memories: usize,

    /// Count of memories grouped by their `type` field
    /// (`"semantic"`, `"episodic"`, `"procedural"`, `"working"`).
    pub by_type: HashMap<String, usize>,

    /// Count of memories grouped by their `confidence` field
    /// (`"explicit"`, `"inferred"`, `"reinforced"`).
    pub by_confidence: HashMap<String, usize>,

    /// Number of memories with a non-NULL `superseded_by` — i.e. memories
    /// that have been replaced by a newer version via `update()` or
    /// consolidated by `compress_old_memories()`.
    pub superseded_count: usize,

    /// Number of memories whose `expires_at` is in the past (and which have
    /// not yet been removed by `purge_expired()`).
    pub expired_count: usize,

    /// Average `importance` across all memories. `0.0` if there are no rows.
    pub average_importance: f32,

    /// Average `access_count` across all memories. `0.0` if there are no rows.
    pub average_access_count: f32,
}
//! Review-lease side of `CaptureController`: leasing segments for the review
//! UI, serving their bytes, and serving thumbnail sidecars. Split out of
//! `capture.rs`, which keeps the record/stop lifecycle.

use std::sync::atomic::Ordering;

use crate::ring_buffer;

use super::{CaptureController, SessionLease};

impl CaptureController {
    // review lease契約: lease は特定 session の特定 index 集合にのみ紐づく(session 全体への
    // アクセス権ではない)。`read_review_segment` は lease に含まれる index 以外を 403 で拒否する。
    // session catalog(`sessions`)に存在しない session の lease は取得できない。
    pub fn acquire_review_lease(
        &self,
        session_id: Option<String>,
        indexes: Vec<u64>,
    ) -> Result<String, String> {
        if indexes.is_empty() {
            return Err("select a segment first".into());
        }
        // Resolved *before* `sessions` is taken (task1990): the fallback reaches
        // for `active`, and `start` holds `active` while it takes `sessions`, so
        // pairing them the other way round here would be a deadlock. It used to
        // be safe only because the running session's id lived in a mutex of its
        // own.
        let resolved = match session_id {
            Some(session_id) => session_id,
            None => self
                .last_timeline
                .lock()
                .ok()
                .and_then(|timeline| timeline.as_ref().map(|m| m.session_id.clone()))
                .or_else(|| self.last_started_session())
                .ok_or_else(|| "there is no session".to_owned())?,
        };
        let (lease, session_id) = {
            // The wait for the mutex, scoped apart from everything it guards:
            // the index writer holds it across its container I/O, so a review
            // read can be slow without touching a disk itself (task3460).
            let mut sessions = {
                crate::insight_scope!("review_sessions_lock");
                self.sessions
                    .lock()
                    .map_err(|_| "the session catalog lock is poisoned".to_owned())?
            };
            let ring = sessions
                .get_mut(&resolved)
                .ok_or_else(|| "no such session".to_owned())?;
            if indexes.iter().any(|index| !ring.contains(*index)) {
                return Err("the selected segment is not in that session".into());
            }
            for index in &indexes {
                ring.lease(*index);
            }
            let lease = format!(
                "{}-{}",
                ring.manifest().session_id,
                self.next_review_lease.fetch_add(1, Ordering::Relaxed)
            );
            (lease, resolved)
        };
        let active_count = {
            let mut leases = self
                .review_leases
                .lock()
                .map_err(|_| "the review lease lock is poisoned".to_owned())?;
            leases.insert(
                lease.clone(),
                SessionLease {
                    session_id,
                    indexes,
                },
            );
            leases.len()
        };
        tracing::info!(active_count, "review lease acquired");
        Ok(lease)
    }

    pub fn release_review_lease(&self, lease: &str) {
        let Ok(mut leases) = self.review_leases.lock() else {
            return;
        };
        let Some(record) = leases.remove(lease) else {
            return;
        };
        let active_count = leases.len();
        drop(leases);
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(ring) = sessions.get_mut(&record.session_id) {
                for index in record.indexes {
                    ring.release(index);
                }
            }
        }
        tracing::info!(active_count, "review lease released");
    }

    pub fn read_review_segment(&self, lease: &str, index: u64) -> Result<Vec<u8>, String> {
        let authorized = self
            .review_leases
            .lock()
            .map_err(|_| "the review lease lock is poisoned".to_owned())?
            .get(lease)
            .is_some_and(|record| record.indexes.contains(&index));
        if !authorized {
            return Err("segment access denied".into());
        }
        let session_id = self
            .review_leases
            .lock()
            .map_err(|_| "the review lease lock is poisoned".to_owned())?
            .get(lease)
            .map(|record| record.session_id.clone())
            .ok_or_else(|| "segment access denied".to_owned())?;
        let target = {
            let sessions = {
                crate::insight_scope!("review_sessions_lock");
                self.sessions
                    .lock()
                    .map_err(|_| "the session catalog lock is poisoned".to_owned())?
            };
            sessions
                .get(&session_id)
                .ok_or_else(|| "no such session".to_owned())?
                .segment_target(index)
                .ok_or_else(|| "segment not found".to_owned())?
        };
        // `sessions`/`review_leases` are released above: disk I/O must never run while
        // holding a lock that other review-lease/segment operations also need, or one
        // slow read serializes every concurrent segment fetch behind it. The
        // target resolved inside the lock is a description, not the bytes.
        //
        // Resolution and the traversal guard are one call (task400), now
        // inside `read_target` so the container arm -- which has no path to
        // traverse, only an index that is or is not in the container -- shares
        // the entry point (task440).
        ring_buffer::read_target(&target, "mp4", false)
            .map_err(|_| "segment read failed".to_owned())
    }

    /// Serves a segment's thumbnail sidecar. Unlike `read_review_segment`,
    /// this deliberately does **not** require (or accept) a review lease:
    /// review leases pin their segments against prune (Task050 fought a
    /// prune-blocked-by-lease bug), and a thumbnail is displayed for the
    /// *whole* timeline while hovering — leasing every segment just to show
    /// its thumbnail would defeat prune almost entirely. Authorization for a
    /// catalog-registered session is instead just "this session exists in the
    /// in-memory catalog" (i.e. is loaded/active), the same bar
    /// `session_manifest`/`get_timeline` use.
    ///
    /// Task069: a session absent from the catalog (unclosed, or crash-
    /// recovered — `load_closed_sessions` skips open manifests and
    /// `recover_session` never inserts into `self.sessions`, same precedent
    /// as `discard_session_from_disk`) falls back to reading its
    /// `manifest.json` straight off disk so its row in the session list can
    /// still show a thumbnail. Both paths funnel through the same
    /// `validate_thumbnail_path` check before any bytes are read, so the
    /// fallback gets identical traversal/extension/existence guarantees.
    pub fn read_review_thumbnail(&self, session_id: &str, index: u64) -> Result<Vec<u8>, String> {
        self.read_review_thumbnail_at(&Self::buffer_root(), session_id, index)
    }

    // `disk_root` is a parameter (rather than always `Self::buffer_root()`) purely so
    // unit tests can exercise the disk-fallback branch without writing into the real
    // %LOCALAPPDATA%\Liveback\buffer -- same rationale as `discard_session_from_disk`'s
    // `root: &Path` parameter.
    pub(super) fn read_review_thumbnail_at(
        &self,
        disk_root: &std::path::Path,
        session_id: &str,
        index: u64,
    ) -> Result<Vec<u8>, String> {
        if session_id.contains(['/', '\\']) || session_id.contains("..") {
            return Err("invalid session id".into());
        }
        let catalog_target = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
            sessions
                .get(session_id)
                .map(|ring| ring.thumbnail_target(index))
        };
        let target = match catalog_target {
            Some(target) => target.ok_or_else(|| "segment has no thumbnail".to_owned())?,
            None => {
                // A session absent from the catalog is read straight off disk,
                // in whichever shape it has. The container branch resolves the
                // same way the catalog one would, so a `.lvb` row can show its
                // thumbnail without being loaded first (task069's contract).
                let container = disk_root
                    .join(session_id)
                    .with_extension(ring_buffer::CONTAINER_EXTENSION);
                if container.is_file() {
                    ring_buffer::RingBuffer::open_container(container)
                        .map_err(|_| "no such session".to_owned())?
                        .thumbnail_target(index)
                        .ok_or_else(|| "segment has no thumbnail".to_owned())?
                } else {
                    let root = disk_root.join(session_id);
                    let manifest = ring_buffer::SessionStore::new(root.clone())
                        .read_manifest_opt()
                        .ok_or_else(|| "no such session".to_owned())?;
                    let segment = manifest
                        .segments
                        .iter()
                        .find(|segment| segment.index == index)
                        .ok_or_else(|| "segment not found".to_owned())?;
                    let thumbnail_path = segment
                        .thumbnail_path
                        .clone()
                        .ok_or_else(|| "segment has no thumbnail".to_owned())?;
                    ring_buffer::ReadTarget::Directory {
                        root,
                        relative: thumbnail_path,
                    }
                }
            }
        };
        // Every branch above ends here, so every one gets the same resolution
        // and the same traversal/extension/existence guard -- the store's,
        // shared with the segment route (task400) -- or, for a container, the
        // index lookup that replaces it (task440).
        ring_buffer::read_target(&target, "jpg", true)
            .map_err(|_| "thumbnail read failed".to_owned())
    }

    // `validate_thumbnail_path` lived here: the traversal/extension/existence
    // guard both branches of `read_review_thumbnail_at` funnel through. It is
    // `ring_buffer::store::SessionStore::resolve_recorded` now (task400),
    // together with the identical guard `read_review_segment` and
    // `export::plan::validate_segment_paths` each kept a copy of.
}

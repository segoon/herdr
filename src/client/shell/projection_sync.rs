//! Ordering rules for optional client projections accompanying a snapshot.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProjectionVersion<'a> {
    pub(super) generation: Option<u64>,
    pub(super) boot_id: &'a str,
    pub(super) revision: u64,
}

impl ProjectionVersion<'_> {
    fn newer_than(self, other: Self) -> bool {
        self.generation == other.generation
            && self.boot_id == other.boot_id
            && self.revision > other.revision
    }
}

pub(super) trait RevisionBound {
    fn version(&self) -> ProjectionVersion<'_>;
}

pub(super) enum ReplacementPolicy {
    KeepNewest,
    LastArrival,
}

/// Returns false when receipt was rejected. Payload-specific presentation stays
/// with the caller, including whether accepting a value reapplies the snapshot.
pub(super) fn receive<T: RevisionBound>(
    current: &mut Option<T>,
    pending: &mut Option<T>,
    next: T,
    snapshot: Option<ProjectionVersion<'_>>,
    policy: ReplacementPolicy,
) -> bool {
    let version = next.version();
    if snapshot.is_some_and(|snapshot| snapshot.newer_than(version)) {
        return false;
    }
    let slot = if snapshot == Some(version) {
        current
    } else {
        pending
    };
    if matches!(policy, ReplacementPolicy::KeepNewest)
        && slot.as_ref().is_some_and(|current| {
            current.version() == version || current.version().newer_than(version)
        })
    {
        return false;
    }
    *slot = Some(next);
    true
}

pub(super) fn synchronize<T: RevisionBound>(
    current: &mut Option<T>,
    pending: &mut Option<T>,
    snapshot: ProjectionVersion<'_>,
) {
    // Unmatched pending values, including future revisions, are discarded.
    if let Some(next) = pending.take().filter(|next| next.version() == snapshot) {
        *current = Some(next);
    } else if current
        .as_ref()
        .is_some_and(|current| current.version() != snapshot)
    {
        *current = None;
    }
}

pub(super) fn matching<'a, T: RevisionBound>(
    current: &'a Option<T>,
    snapshot: Option<ProjectionVersion<'_>>,
) -> Option<&'a T> {
    current
        .as_ref()
        .filter(|current| Some(current.version()) == snapshot)
}

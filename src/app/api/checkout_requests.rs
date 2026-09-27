use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckoutRequestState {
    Running,
    OutcomeUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CheckoutRequest {
    path: PathBuf,
    workspace_id: Option<String>,
    state: CheckoutRequestState,
}

/// Owns mutation reservations shared by Git worktrees and external VCS
/// checkouts. Paths passed to this type must already be canonicalized with the
/// repository's normal `canonical_or_original` policy.
#[derive(Debug, Default)]
pub(crate) struct CheckoutRequests {
    next_id: u64,
    operations: HashMap<u64, CheckoutRequest>,
    creates_by_path: HashMap<PathBuf, u64>,
    removes_by_path: HashMap<PathBuf, u64>,
    removes_by_workspace: HashMap<String, u64>,
}

impl CheckoutRequests {
    pub(crate) fn new() -> Self {
        Self {
            next_id: 1,
            ..Self::default()
        }
    }

    pub(crate) fn has_path_conflict(&self, path: &PathBuf) -> bool {
        self.creates_by_path.contains_key(path) || self.removes_by_path.contains_key(path)
    }

    pub(crate) fn has_remove_conflict(&self, workspace_id: &str, path: &PathBuf) -> bool {
        self.removes_by_workspace.contains_key(workspace_id) || self.has_path_conflict(path)
    }

    pub(crate) fn reserve_create(&mut self, path: PathBuf) -> Result<u64, ()> {
        if self.has_path_conflict(&path) {
            return Err(());
        }
        let operation_id = self.next_operation_id();
        self.creates_by_path.insert(path.clone(), operation_id);
        self.operations.insert(
            operation_id,
            CheckoutRequest {
                path,
                workspace_id: None,
                state: CheckoutRequestState::Running,
            },
        );
        Ok(operation_id)
    }

    pub(crate) fn reserve_remove(
        &mut self,
        workspace_id: String,
        path: PathBuf,
    ) -> Result<u64, ()> {
        if self.has_remove_conflict(&workspace_id, &path) {
            return Err(());
        }
        let operation_id = self.next_operation_id();
        self.removes_by_path.insert(path.clone(), operation_id);
        self.removes_by_workspace
            .insert(workspace_id.clone(), operation_id);
        self.operations.insert(
            operation_id,
            CheckoutRequest {
                path,
                workspace_id: Some(workspace_id),
                state: CheckoutRequestState::Running,
            },
        );
        Ok(operation_id)
    }

    pub(crate) fn matches_create(&self, operation_id: u64, path: &PathBuf) -> bool {
        self.creates_by_path.get(path) == Some(&operation_id)
            && self
                .operations
                .get(&operation_id)
                .is_some_and(|request| request.path == *path && request.workspace_id.is_none())
    }

    pub(crate) fn matches_remove(
        &self,
        operation_id: u64,
        workspace_id: &str,
        path: &PathBuf,
    ) -> bool {
        self.removes_by_path.get(path) == Some(&operation_id)
            && self.removes_by_workspace.get(workspace_id) == Some(&operation_id)
            && self.operations.get(&operation_id).is_some_and(|request| {
                request.path == *path && request.workspace_id.as_deref() == Some(workspace_id)
            })
    }

    pub(crate) fn finish_create(&mut self, operation_id: u64, path: &PathBuf) -> bool {
        if !self.matches_create(operation_id, path) {
            return false;
        }
        self.creates_by_path.remove(path);
        self.operations.remove(&operation_id);
        true
    }

    pub(crate) fn finish_remove(
        &mut self,
        operation_id: u64,
        workspace_id: &str,
        path: &PathBuf,
    ) -> bool {
        if !self.matches_remove(operation_id, workspace_id, path) {
            return false;
        }
        self.removes_by_path.remove(path);
        self.removes_by_workspace.remove(workspace_id);
        self.operations.remove(&operation_id);
        true
    }

    pub(crate) fn mark_outcome_unknown(&mut self, operation_id: u64) -> bool {
        let Some(request) = self.operations.get_mut(&operation_id) else {
            return false;
        };
        request.state = CheckoutRequestState::OutcomeUnknown;
        true
    }

    #[cfg(test)]
    pub(crate) fn state(&self, operation_id: u64) -> Option<CheckoutRequestState> {
        self.operations
            .get(&operation_id)
            .map(|request| request.state)
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn insert_create_for_test(&mut self, path: PathBuf, operation_id: u64) {
        self.creates_by_path.insert(path.clone(), operation_id);
        self.operations.insert(
            operation_id,
            CheckoutRequest {
                path,
                workspace_id: None,
                state: CheckoutRequestState::Running,
            },
        );
        self.next_id = self.next_id.max(operation_id.saturating_add(1));
    }

    #[cfg(test)]
    pub(crate) fn insert_remove_for_test(
        &mut self,
        workspace_id: String,
        path: PathBuf,
        operation_id: u64,
    ) {
        self.removes_by_path.insert(path.clone(), operation_id);
        self.removes_by_workspace
            .insert(workspace_id.clone(), operation_id);
        self.operations.insert(
            operation_id,
            CheckoutRequest {
                path,
                workspace_id: Some(workspace_id),
                state: CheckoutRequestState::Running,
            },
        );
        self.next_id = self.next_id.max(operation_id.saturating_add(1));
    }

    fn next_operation_id(&mut self) -> u64 {
        let operation_id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        operation_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_reject_path_and_workspace_conflicts() {
        let mut requests = CheckoutRequests::new();
        let path = PathBuf::from("/checkout");
        let create = requests.reserve_create(path.clone()).unwrap();
        assert!(requests.reserve_create(path.clone()).is_err());
        assert!(requests
            .reserve_remove("workspace".into(), path.clone())
            .is_err());
        assert!(requests.finish_create(create, &path));

        let remove = requests
            .reserve_remove("workspace".into(), path.clone())
            .unwrap();
        assert!(requests
            .reserve_remove("workspace".into(), PathBuf::from("/other"))
            .is_err());
        assert!(requests.finish_remove(remove, "workspace", &path));
        assert!(requests.is_empty());
    }

    #[test]
    fn unknown_outcome_keeps_the_reservation() {
        let mut requests = CheckoutRequests::new();
        let path = PathBuf::from("/checkout");
        let operation = requests.reserve_create(path.clone()).unwrap();

        assert!(requests.mark_outcome_unknown(operation));
        assert_eq!(
            requests.state(operation),
            Some(CheckoutRequestState::OutcomeUnknown)
        );
        assert!(requests.reserve_create(path).is_err());
    }
}

# Unified VCS workspace API investigation

## Status and scope

This document records the design investigation for unifying built-in Git
worktrees and out-of-tree VCS checkouts in Herdr.

It is a design document, not an implemented contract. Existing public socket
methods, event names, provider protocol operations, endpoint codecs, and
persistence shapes remain unchanged until an implementation is reviewed and
landed.

The initial internal refactor described by this investigation has been
implemented without adding the proposed public contract:

- Git and external checkouts share workspace reuse/creation and finalization;
- both adapters share checkout response record construction while retaining
  their legacy response variants;
- checkout runtime shutdown and recovery have a neutral owner;
- recovery receives an explicit backend identity, so a workspace carrying both
  legacy memberships cannot restore to the wrong checkout path;
- internal removal state and recovery events use checkout-neutral names;
- Git-specific shutdown policy and legacy event behavior remain in the Git API
  adapter.

Source selection, backend execution, public `workspace.vcs.*` methods, neutral
events, endpoint projection, and persistence migration remain future phases.

The goals are:

- give the first-party client one provider-neutral workspace workflow;
- share common orchestration instead of maintaining parallel Git and external
  VCS state machines;
- preserve Git-specific behavior where it is genuinely Git-specific;
- support providers with different capabilities and native data models;
- preserve old plugins and clients that only understand Git;
- keep the proprietary VCS implementation out of the Herdr repository.

## Conclusions

Common code can and should be unified. The largest reusable area is everything
that happens around a VCS command: selecting a source, reserving a target,
opening or reusing a workspace, attaching provenance, closing or restoring
runtime state, emitting neutral lifecycle results, and completing the API
request.

The backend command layer should remain polymorphic. Git and an external
provider do not need to accept the same native arguments, produce the same
native output, or implement the same capabilities. Forcing them into the
current Git-shaped `WorktreeInfo` would produce false data and a brittle
lowest-common-denominator abstraction.

The recommended end state is:

1. one neutral internal checkout lifecycle;
2. one built-in Git backend and one external-provider backend;
3. a new `workspace.vcs.*` public method family for new clients;
4. unchanged `worktree.*` and `checkout.*` compatibility adapters;
5. neutral events for new consumers, with legacy Git events still emitted for
   Git outcomes;
6. a staged persistence migration rather than changing snapshot state in the
   first refactor.

The old `checkout.*` socket methods can disappear from first-party client code,
but cannot be removed from the server or its API documentation without an
explicit breaking-compatibility policy. While supported, they should remain
documented as legacy methods with a pointer to the preferred API. The provider
stdio protocol's `checkout.*` operation names are a separate contract and
should remain v1.

## Current architecture

### Public socket API

The public API currently has two method families:

- `worktree.list`, `worktree.create`, `worktree.open`, and `worktree.remove`
  for built-in Git;
- `checkout.list`, `checkout.create`, `checkout.open`, and `checkout.remove`
  for configured external providers.

These are not equivalent schemas under different names.

The Git request and response types contain Git concepts:

- branch and base revision;
- bare, detached, prunable, and linked-worktree state;
- path-or-branch target selection;
- `trust_repository`;
- Git-specific lifecycle events.

The external types contain provider-neutral facts:

- provider ID and display name;
- opaque checkout ID;
- provider capabilities;
- checkout name, path, and `managed` status.

Existing endpoint request shapes are frozen by
`tests/fixtures/endpoint-method-shapes-v1.json` and the digest test in
`src/server/client_commands.rs`. Adding load-bearing fields to an existing
method, changing a response variant, or changing its meaning would violate the
stable endpoint contract.

The current digest test does not freeze success responses, errors, or event
payloads. Those contracts still must not change, but the implementation phase
should add golden schema/JSON fixtures for legacy `worktree.*` and `checkout.*`
responses and for `worktree.created`, `worktree.opened`, and
`worktree.removed` before routing them through shared code.

### Provider protocol

`src/vcs.rs` owns the external provider boundary. A provider is discovered from
configured markers, launched as a process, and communicates through
stdio-JSON-v1. It negotiates a subset of:

- `inspect`;
- `checkout.list`;
- `checkout.create`;
- `checkout.remove`;
- `checkout.remove.force`.

The provider protocol already isolates proprietary command lines and output
formats. Herdr receives normalized records and exact native paths. Its v1
fixtures should remain frozen independently of any public socket API rename.

### Workspace state

`Workspace` currently carries separate state:

- `worktree_space: Option<WorktreeSpaceMembership>` for Git;
- `checkout_space: Option<CheckoutSpaceMembership>` for external providers;
- cached Git identity/status;
- cached external VCS identity/status.

Both memberships are persisted independently. Git membership records repository
and linked-worktree facts. External membership additionally records provider
identity, opaque checkout identity, managed status, and the source workspace.

The public `WorkspaceInfo.worktree` field only projects Git membership.
External checkout membership currently reaches the client through the endpoint
VCS projection, not through general `WorkspaceInfo`.

### Client behavior

The first-party client presents a shared worktree UI, but branches internally
between `ClientCheckoutBackend::ExternalVcs` and Git. It sends `checkout.*` for
external targets and `worktree.*` for Git targets, then separately matches the
two response families.

This is presentation reuse, not lifecycle reuse. The server still has separate
execution and completion paths.

## Current dependency graph

```text
client worktree UI
  |-- Git branch ----------> worktree.* schema
  |                           -> app/api/worktrees/{reads,deferred}.rs
  |                           -> worktree.rs Git commands
  |                           -> WorktreeSpaceMembership
  |
  `-- external branch -----> checkout.* schema
                              -> app/api/checkouts.rs
                              -> vcs.rs provider runner
                              -> CheckoutSpaceMembership

both mutation paths
  -> app/api/checkout_requests.rs
  -> app/worktrees.rs runtime shutdown/restore helpers
  -> App::create_workspace_with_options
  -> App::emit_workspace_open_events
  -> persisted workspace snapshot
```

The graph shows that common infrastructure already exists below both API
families, but source resolution, operation preparation, completion, membership
attachment, and response construction are still split above it.

## Duplication inventory

### Already shared

The following common behavior already has one implementation:

- canonical path conflict reservations through `CheckoutRequests`;
- distinct `Running` and `OutcomeUnknown` mutation states;
- create/remove conflict detection by path and workspace;
- terminal runtime shutdown before removal;
- terminal runtime restoration after failed or completed removal;
- workspace construction through `create_workspace_with_options`;
- workspace/tab/pane creation event emission;
- session dirty tracking;
- core response encoding;
- canonical-or-original path comparison.

This proves that Git and external operations can safely share infrastructure.
It also exposes naming debt: external code currently calls helpers named
`worktree`, such as runtime restoration and path canonicalization. Those helpers
belong to a neutral checkout/workspace-lifecycle module even though their
behavior is reusable.

### Duplicated and suitable for unification

The following flows are implemented separately but have the same responsibility:

1. Validate that at most one of `workspace_id` and `cwd` is supplied.
2. Resolve the active or selected workspace when neither is supplied.
3. Capture stable source identity before background work.
4. Resolve repository/provider identity.
5. Normalize and reserve a destination path.
6. Carry an operation token through asynchronous execution.
7. Reject stale or superseded completions.
8. Find an already-open workspace for a checkout path.
9. Focus an existing workspace or create a new one.
10. Attach repository and checkout provenance.
11. Apply an optional workspace label.
12. Mark session state dirty and schedule identity refresh.
13. Emit workspace creation/update events.
14. Close a successfully removed workspace only if it still represents the
    checkout that was removed.
15. Restore terminal runtimes after removal failure or after closing the
    removed workspace.
16. Build list/create/open/remove outcomes for an API adapter.

These should become a single prepare/execute/finish state machine.

### Behavior that must remain backend-specific

The following differences are real capabilities or semantics, not accidental
duplication:

- Git discovery and external marker discovery;
- Git's parent checkout and linked-worktree rules;
- branch creation and selection from a base revision;
- bare, detached, and prunable Git entries;
- `trust_repository` and Git safe-directory behavior;
- Git command construction and blocking process execution;
- provider activation, allowlist intersection, timeouts, and protocol checks;
- opaque external checkout IDs;
- provider-managed versus unmanaged checkouts;
- provider configuration generation checks;
- timeout reconciliation through `checkout.list`;
- backend-specific dirty-checkout error recognition;
- backend-specific default destination calculation;
- whether runtimes must be stopped for a non-forced removal.

The shared layer should ask a backend for these decisions or consume their
normalized outcomes. It should not reimplement or infer them.

## Recommended internal boundary

### Three layers

Use three explicit layers rather than placing all behavior behind one large
trait.

```text
wire adapters
  - legacy worktree.*
  - legacy checkout.*
  - new workspace.vcs.*
          |
          v
workspace VCS lifecycle
  - source selection
  - preparation and reservations
  - stale-result policy
  - workspace open/adopt/focus
  - membership and persistence updates
  - removal recovery
  - neutral events/outcomes
          |
          v
backend execution
  - BuiltinGit
  - External(ActivatedProvider)
```

Wire compatibility, workspace orchestration, and native VCS execution are
different responsibilities and should remain different modules.

### Prefer a prepared job over an async trait

Herdr already performs background work and returns results to the main app loop.
The cleanest test seam is therefore a prepared job, not necessarily a dynamic
async trait.

Conceptual types:

```rust
enum VcsBackendId {
    BuiltinGit,
    External(String),
}

struct VcsRepository {
    backend: VcsBackendId,
    key: String,
    display_name: String,
    root: PathBuf,
    source: Option<VcsCheckout>,
    capabilities: VcsCapabilities,
    backend_epoch: Option<u64>,
}

struct VcsCheckout {
    id: String,
    name: String,
    path: PathBuf,
    managed: bool,
    role: CheckoutRole,
    details: CheckoutDetails,
}

enum CheckoutRole {
    Source,
    Child { source_workspace_id: Option<String> },
}

enum CheckoutDetails {
    Git(GitCheckoutDetails),
    External,
}

struct PreparedVcsOperation {
    request: NeutralVcsRequest,
    repository: VcsRepository,
    mutation: Option<MutationReservation>,
    removal_recovery: Option<RemovalRecovery>,
    compatibility: CompatibilityContext,
}

enum VcsBackendJob {
    Git(GitJob),
    External(ExternalJob),
}

struct VcsJobResult {
    prepared: PreparedVcsOperation,
    outcome: Result<NeutralVcsOutcome, VcsFailure>,
}
```

The lifecycle prepares a job on the app thread. An executor performs the Git
command or external provider request without accessing mutable `AppState`. The
app thread then consumes one normalized result and applies it.

This design has several advantages:

- no new async-trait dependency is required;
- backend execution cannot mutate workspace state;
- prepare and finish logic can be unit-tested with synthetic outcomes;
- Git may continue using a blocking worker while providers use Tokio;
- backend-specific context remains typed rather than hidden in unstructured
  metadata;
- stale-result and mutation-reservation rules have one implementation.

A backend trait remains an alternative, but it would need boxed futures or an
additional macro dependency and would tend to mix execution with app-state
orchestration. A small backend enum is simpler while Herdr has only one built-in
backend class and one normalized external-provider class.

### Common lifecycle responsibilities

The new lifecycle module should own:

- resolving `workspace_id` versus `cwd` into a stable source selector;
- applying explicit versus automatic provider selection;
- preparing normalized list/create/open/remove jobs;
- reserving create/remove mutations;
- checking completion tokens;
- applying outcome-aware configuration grandfathering;
- finding a workspace by explicit VCS membership or checkout path;
- creating, focusing, labeling, and closing workspaces;
- setting neutral membership through one workspace helper;
- runtime quiescence and recovery bookkeeping;
- neutral success and failure outcomes;
- scheduling Git or external identity refresh after mutations.

It should not own JSON response construction, Git command arguments, provider
wire messages, or client UI state.

### Backend responsibilities

Each backend should implement the semantic equivalent of:

```text
discover(source) -> repository
list(repository) -> checkouts
create(repository, create specification) -> checkout
remove(repository, checkout, force) -> mutation outcome
```

`open` does not require a native backend mutation. It is a common operation:
list or validate a target, then open/adopt its path as a Herdr workspace.

Backend results need explicit mutation certainty:

```rust
enum MutationCertainty {
    Completed,
    Failed,
    OutcomeUnknown,
}
```

External timeouts can be reconciled to `Completed` or remain
`OutcomeUnknown`. Git currently produces completed/failed results. The shared
reservation layer keeps unknown outcomes quarantined regardless of backend.

## Source and provider selection

The new API should accept a neutral selector:

```text
selection: auto
selection: provider("builtin.git")
selection: provider("configured-provider-id")
```

Automatic selection should centralize the rule already used by external VCS
refresh:

1. select the repository with the deepest matching root;
2. when Git and an external provider have equal-depth roots, prefer Git;
3. allow a more deeply nested external repository to win;
4. reject an unresolved same-priority external-provider tie.

Legacy adapters must bypass automatic selection:

- `worktree.*` always selects `builtin.git`;
- `checkout.*` always selects an external provider according to its current
  discovery behavior.

This is necessary because old callers may rely on `worktree.*` returning
`not_git_worktree` instead of unexpectedly operating on a newly configured
provider.

## Capabilities and request modeling

Not every VCS has the same operations. A neutral API must treat support as data,
not as an assumption.

Recommended neutral capabilities include:

```text
workspace.vcs.inspect
workspace.vcs.list
workspace.vcs.create
workspace.vcs.create.start_point
workspace.vcs.remove
workspace.vcs.remove.force
```

The service maps these onto Git features or external protocol capabilities.
The external provider's v1 capability strings do not need to change.

A neutral create request may contain:

```text
name
destination
start_point
label
focus
```

Rules:

- `name` is a user-facing checkout name, not necessarily a branch;
- `destination` is optional only when the backend has a configured/default
  checkout directory;
- `start_point` is accepted only when the selected backend advertises it;
- unsupported supplied options produce an explicit error;
- unknown fields must not be silently treated as successful behavior by an old
  server;
- provider-specific arbitrary JSON should not be part of the initial public
  contract.

Internally, legacy Git requests may use a richer typed Git create specification
so that `branch`, `base`, generated branch names, and `trust_repository` retain
their exact behavior. Compatibility adapters do not need to restrict themselves
to the new public request's lowest common denominator.

## Public API recommendation

Add new advertised methods:

```text
workspace.vcs.list
workspace.vcs.create
workspace.vcs.open
workspace.vcs.remove
```

Suggested neutral records:

```rust
struct WorkspaceVcsSourceInfo {
    provider_id: String,
    provider_name: String,
    provider_kind: ProviderKind,
    repository_key: String,
    repository_root: String,
    source_workspace_id: Option<String>,
    capabilities: Vec<String>,
}

struct WorkspaceVcsCheckoutInfo {
    id: String,
    name: String,
    path: String,
    managed: bool,
    role: CheckoutRoleInfo,
    open_workspace_id: Option<String>,
    reference: Option<VcsReferenceInfo>,
    git: Option<GitCheckoutInfo>,
}
```

The generic record carries only facts that are valid for every backend. An
optional typed Git extension may expose branch, bare, detached, prunable, and
linked-worktree details without making them mandatory for other providers.

Use an opaque checkout `id` for `workspace.vcs.open`. A path is display and
workspace-launch data, not the universal VCS identity. The built-in Git adapter
can derive a stable request ID from its validated checkout identity while still
preserving the old path-or-branch interface through `worktree.open`.

### Compatibility matrix

| Contract | Git | External provider | Status |
| --- | --- | --- | --- |
| existing `worktree.*` | yes | no | frozen, retained |
| existing `checkout.*` | no | yes | retained compatibility adapter |
| new `workspace.vcs.*` | yes | yes | preferred for new clients |
| provider stdio `checkout.*` | not used | yes | frozen protocol v1 |

Existing request shapes and response variants must not be modified. Request
shapes already have digest enforcement; response and legacy-event fixtures must
be added as described above. New methods are independently advertised and
negotiated. A new client prefers
`workspace.vcs.*`, then falls back to `worktree.*` for Git or `checkout.*` for
external providers.

## Events and old plugins

Add neutral lifecycle events, for example:

```text
workspace.vcs.created
workspace.vcs.opened
workspace.vcs.removed
```

Compatibility behavior:

- a Git result produced through the new API emits the neutral event and the
  corresponding legacy `worktree.*` event;
- a Git result produced through legacy `worktree.*` retains its current legacy
  event behavior and does not emit a neutral event in the initial rollout;
- an external result emits the neutral event only;
- an external result must never be serialized as `WorktreeInfo` or emitted as a
  legacy `worktree.*` event;
- old plugin manifests remain valid and continue to observe Git operations;
- old plugins never receive a new non-Git payload under an event name they
  already understand.

For a new-API Git operation, emit the legacy event in its existing observable
slot before the neutral event. Existing ordering must remain fixed: create
emits workspace/tab/pane/layout events before `worktree.created`; open emits an
applicable `workspace.renamed` before `worktree.opened`; remove emits an
applicable `workspace.closed` before `worktree.removed`. Dual emission must be
centralized in the compatibility/event adapter so a single lifecycle outcome
cannot accidentally produce inconsistent payloads. New event payloads should
carry an operation or correlation ID before any future design allows legacy
methods to emit both families.

## Workspace projection and endpoint compatibility

Keep `WorkspaceInfo.worktree` Git-only. Add an optional neutral VCS projection
for new JSON API consumers rather than reinterpreting the old field.

The stable client endpoint generation-1 codecs must not gain fields or enum
variants. Send neutral workspace VCS state through a new named projection
version or a separately advertised endpoint-control capability. Continue
serving the current snapshot and external VCS projection to old clients.

New clients can normalize old inputs locally during fallback:

- old Git snapshot/worktree facts become a neutral client record;
- the existing external VCS projection becomes a neutral client record;
- a new neutral projection is used directly when advertised.

Missing new methods or projections must disable only the affected action and
must not reject or disconnect an otherwise compatible endpoint.

## Persistence strategy

Do not combine public API unification with an immediate snapshot migration.

Phase one should add a read abstraction over existing state:

```rust
enum VcsWorkspaceMembershipRef<'a> {
    Git(&'a WorktreeSpaceMembership),
    External(&'a CheckoutSpaceMembership),
}
```

Add neutral helpers for:

- reading provider/repository/checkout identity;
- comparing checkout paths;
- finding a source or parent workspace;
- setting or clearing membership through the correct existing field;
- verifying that a removal completion still refers to the same workspace.

This removes orchestration duplication while preserving persisted fields and
restore behavior. A later migration may replace both fields with a tagged
owned `VcsWorkspaceMembership`, but it must include snapshot compatibility,
restore validation, adversarial identity tests, and a deliberate rollback
story.

## Proposed module boundaries

One possible layout is:

```text
src/app/vcs_workspace/
  mod.rs              lifecycle facade
  model.rs            neutral internal types
  selection.rs        source and provider selection
  prepare.rs          validation, reservations, recovery context
  finish.rs           stale checks and workspace state transitions
  membership.rs       neutral access over current persisted memberships
  events.rs           neutral and legacy dual-emission policy

src/app/vcs_workspace/backend/
  mod.rs              backend enum and job/outcome contracts
  git.rs              adapter over existing worktree functions
  external.rs         adapter over ActivatedProvider

src/app/api/
  workspace_vcs.rs    new neutral wire adapter
  worktrees.rs        frozen Git compatibility adapter
  checkouts.rs        frozen external compatibility adapter
  checkout_requests.rs shared mutation reservation registry
```

The lifecycle facade must not become a god object. Selection, preparation,
completion, membership, and event translation should remain individually
testable responsibilities.

Backend adapters should call the existing proven Git and provider primitives at
first. Native command implementations can be moved only after behavior is
protected by characterization tests.

## Outcome-aware configuration grandfathering

The current external path distinguishes reads from mutations when provider
configuration changes during execution:

- a stale read is rejected and can be retried;
- a completed mutation is accepted if its operation reservation is still
  current;
- an unknown mutation outcome retains its reservation and prevents a
  conflicting operation.

Preserve this policy in the shared finish state machine.

Represent backend configuration generation as an optional epoch on the prepared
operation. Git has no external registry epoch. Completion rules are then:

1. reject any superseded mutation token;
2. reject a stale non-mutating result when its backend epoch changed;
3. accept a definite mutation result when its reservation is current, even if
   configuration changed;
4. retain the reservation for an unknown outcome;
5. release the reservation for definite success or definite failure;
6. restore quiesced runtimes after definite failure;
7. close a workspace after success only when its current membership still
   matches the prepared target.

This policy belongs to the lifecycle, not only to the external-provider
adapter.

## Testing strategy

### Characterization before refactoring

Protect current behavior before moving code:

- every existing `worktree.*` request and response shape;
- `not_git_worktree` and linked-source rejection behavior;
- Git create/open/remove events and ordering;
- existing external `checkout.*` responses and error codes;
- external configuration-reload grandfathering;
- unknown-outcome reservation quarantine;
- runtime shutdown and restoration behavior;
- workspace provenance and persistence restore;
- current client method negotiation and fallback.

The Git API currently has substantially more lifecycle coverage than the
external checkout API. External checkout completion has focused tests for stale
reads, successful mutation grandfathering, and unknown outcomes, while provider
protocol and runner behavior are tested in `src/vcs.rs`. The refactor needs
additional external create/open/remove workspace-lifecycle tests before code is
moved.

### Unit tests for the shared lifecycle

Prepared-job boundaries allow genuine unit tests without Git, PTYs, or provider
processes. Feed synthetic outcomes into the finish state machine and verify:

- existing workspace reuse versus new workspace creation;
- focus and label behavior;
- source and child membership attachment;
- stale completion rejection;
- epoch changes for reads and mutations;
- completed, failed, and unknown mutation states;
- removal only closes a still-matching workspace;
- runtime recovery on failure;
- neutral event emission and Git-only legacy dual emission;
- capability rejection before execution;
- nested Git/external automatic selection.

Use `AppState::test_new()` or `Workspace::test_new()` and invariant assertions.
The shared tests should not invoke Git or spawn a provider.

### Backend tests

Keep backend-specific integration coverage separate:

- Git adapter tests use temporary real Git repositories;
- provider adapter tests use a controlled test executable speaking the frozen
  stdio protocol;
- exact-path tests cover Unix non-UTF-8 and Windows path encoding;
- timeout reconciliation tests cover present, absent, and unavailable list
  results;
- capability tests cover every supported subset, not only the full set.

### Compatibility tests

Add explicit assertions that:

- the generation-1 method-shape fixture is unchanged;
- provider protocol v1 fixtures are unchanged;
- old `worktree.*` remains Git-only in an external repository;
- old `checkout.*` remains external-only;
- old `worktree.*` event subscribers receive Git events initiated through the
  new API;
- old subscribers never receive external data under Git event names;
- a new client falls back correctly when `workspace.vcs.*` is not advertised;
- an old client remains connected when the new projection or methods exist.

Because this refactor touches multiple core surfaces, persisted provenance,
workspace identity, events, and endpoint negotiation, it qualifies as a broad
refactor under the repository guidance. Run the required roundtable and use
adversarial workspace identity state before implementation.

## Implementation sequence

1. Add missing characterization tests without changing behavior.
2. Introduce neutral internal types and membership read helpers.
3. Introduce prepared operation/job/result types.
4. Move reservation, stale completion, outcome certainty, runtime recovery,
   and workspace adoption into the shared lifecycle.
5. Add Git and external backend adapters that initially delegate to existing
   primitives.
6. Route legacy `worktree.*` through the lifecycle and confirm byte-compatible
   response/event behavior.
7. Route legacy `checkout.*` through the lifecycle and confirm its behavior.
8. Add `workspace.vcs.*`, neutral records, capabilities, and events.
9. Add a separately negotiated neutral endpoint projection.
10. Migrate the first-party client to the new contract with old-server
    fallback.
11. Mark top-level `checkout.*` as legacy in current documentation, retain its
    complete contract documentation, and keep the server handlers.
12. Rename remaining neutral helpers that still contain `worktree` in their
    names.
13. Consider persisted membership unification as a separate change.

Each routing step should be independently reviewable and should leave the old
wire contracts usable. Do not combine the whole migration into one large
rewrite.

## Alternatives considered

### Make existing `worktree.*` automatically select any provider

Rejected. Existing requests and responses are Git-shaped, old plugins expect
Git events, and old callers may rely on non-Git requests failing. This would be
a semantic breaking change even if JSON parsing continued to succeed.

### Return either `WorktreeInfo` or `CheckoutInfo` from existing methods

Rejected. Older typed clients do not know a new response variant, and method
shape compatibility is frozen. It also leaves two internal domain models.

### Add a provider selector to existing `worktree.*`

Rejected for the stable methods. An old server could ignore an optional,
load-bearing selector and incorrectly execute a Git operation. The endpoint
contract requires a new advertised method for this situation.

### Rename `checkout.*` to `worktree.*.v2`

Possible but not recommended. Versioning would protect compatibility, but the
word worktree still imposes Git terminology on providers that do not implement
worktrees. `workspace.vcs.*` states the Herdr responsibility more accurately.

### Keep both implementations and share only utilities

Low initial risk, but rejected as the final design. It leaves two completion
state machines whose concurrency, recovery, event, and workspace-lifecycle
behavior can drift. Utilities alone do not protect behavioral consistency.

### Put all logic behind one backend trait

Possible, but less attractive initially. A large trait would either expose app
state to backends or accumulate lifecycle methods unrelated to native VCS
execution. Async object safety would also add complexity. Prepared jobs plus a
small backend enum provide the useful seam with less machinery.

### Immediately replace both persisted membership fields

Deferred. It produces a cleaner final state but unnecessarily combines
snapshot migration risk with API and execution refactoring. A neutral accessor
captures most of the benefit first.

## Risks and limits

- A neutral model cannot manufacture capabilities a provider does not expose.
- The first provider protocol version cannot pass Git-only `base` semantics to
  an arbitrary provider.
- Some duplication remains deliberately in compatibility serialization and
  backend execution.
- Dual events require deduplication guidance for new plugins that subscribe to
  both neutral and legacy names.
- A synthetic source-checkout identity may be necessary when a provider cannot
  list or identify the current checkout; this must be marked as synthetic and
  never confused with a provider-issued ID.
- Canonical paths are appropriate for local conflict and workspace matching,
  but must not replace provider-issued checkout identity.
- A provider may disappear after preparation. Reads can retry; current
  mutations must follow outcome-aware completion rules.
- Cross-platform runtime shutdown policy cannot be hidden inside a generic
  `remove` call; it is part of workspace lifecycle preparation.

## Estimated complexity and maintenance impact

This is a substantive multi-phase refactor, not a rename. The internal
prepare/execute/finish extraction and compatibility routing are medium-to-high
complexity. Adding the new public API, endpoint projection, events, fallback,
and documentation is another medium-to-high complexity phase. Persisted-state
unification is a separate high-risk phase.

The short-term maintenance cost is higher because compatibility adapters remain.
The long-term cost is lower because concurrency, recovery, workspace adoption,
and event policy have one implementation, while native VCS semantics remain in
small backend adapters.

The intended maintenance rule is:

- add workspace lifecycle behavior once in the shared lifecycle;
- add native VCS behavior in the relevant backend;
- change a legacy adapter only when preserving its frozen contract;
- add provider capabilities instead of assuming feature parity.

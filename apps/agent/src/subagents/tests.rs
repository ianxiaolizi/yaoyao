use super::*;
use agent_protocol::events::{Actor, EventDraft};
use agent_protocol::ids::{EventId, SchemaVersion, TurnId};
use agent_protocol::permissions::Workspace;
use std::path::Path;

/// A backend that names a program nothing here starts.
///
/// These tests are about which backends a run keeps and where their working
/// directory lands, and no command written here is ever executed — the
/// adapter is not built, only the configuration list is filtered.
fn config(provider: &str, cwd: Option<&str>) -> SubagentConfig {
    SubagentConfig {
        provider: provider.to_string(),
        kind: SubagentKind::Stdio,
        command: Some("unused-by-this-test".to_string()),
        args: Vec::new(),
        env: Vec::new(),
        cwd: cwd.map(str::to_string),
        requires: Vec::new(),
        ceiling_ms: None,
    }
}

fn guard(dir: &Path) -> ResourceGuard {
    ResourceGuard::new(Workspace {
        workspace_root: dir.display().to_string(),
        writable_roots: Vec::new(),
        external_directories: Vec::new(),
    })
}

/// What a sandbox that can start processes advertises. The two capabilities
/// admission turns on are stated here so every test that is not about one of
/// them states them once.
fn spawns() -> Vec<Capability> {
    vec![Capability::new(Capability::PROCESS_SPAWN), Capability::new(Capability::SUBAGENT_SPAWN)]
}

/// An in-process backend asks the sandbox only for the capability that says
/// this build delegates — not for a process. So a run that cannot start a
/// process at all, which is the read-only backend, still admits it, while a
/// stdio backend beside it is refused with the reason (CONTRACT §7 #133).
#[test]
fn an_in_process_backend_is_admitted_by_a_run_that_can_start_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut in_process = config("inner", None);
    in_process.kind = SubagentKind::InProcess;
    in_process.command = None;

    let read_only = [Capability::new(Capability::SUBAGENT_SPAWN)];
    let (admitted, notes) =
        admit_backends(&[config("outer", None), in_process], &guard(dir.path()), &read_only);

    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].provider, "inner");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("process.spawn"), "{:?}", notes[0]);
}

/// A build that does not delegate at all admits no backend of either kind: a
/// backend admitted without `subagent.spawn` would be a tool every call to
/// which the policy refuses.
#[test]
fn a_run_that_does_not_delegate_admits_no_backend_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let mut in_process = config("inner", None);
    in_process.kind = SubagentKind::InProcess;
    in_process.command = None;

    let (admitted, notes) = admit_backends(
        &[config("outer", None), in_process],
        &guard(dir.path()),
        &[Capability::new(Capability::FS_READ)],
    );

    assert!(admitted.is_empty(), "{admitted:?}");
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes.iter().all(|note| note.contains("subagent.spawn")), "{notes:?}");
}

#[test]
fn a_relative_backend_directory_resolves_against_the_workspace_root() {
    // A `cwd` in a configuration file is relative to the workspace root, not
    // to wherever the process was launched — the child would otherwise be
    // started in the launcher's directory, which is the defect this method
    // exists to close. It is resolved to an absolute path here so the adapter
    // never has to know the workspace exists.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let guard = guard(&root);

    // The premise: the process is not sitting in the workspace, so nothing
    // resolves to the right place by accident.
    assert_ne!(std::env::current_dir().unwrap(), root.canonicalize().unwrap());

    let (admitted, notes) = admit_backends(&[config("one", Some("sub"))], &guard, &spawns());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(admitted.len(), 1);
    assert_eq!(
        admitted[0].cwd.as_deref(),
        Some(root.canonicalize().unwrap().join("sub").to_str().unwrap())
    );
}

#[test]
fn a_backend_whose_directory_escapes_the_workspace_is_skipped_with_a_note() {
    // The boundary is the same one every path a tool is handed is held to, so
    // a backend cannot be started outside the workspace by configuration
    // alone — and the run says so rather than starting it in the wrong place.
    let dir = tempfile::tempdir().unwrap();
    let guard = guard(dir.path());

    let (admitted, notes) = admit_backends(&[config("one", Some("../outside"))], &guard, &spawns());
    assert!(admitted.is_empty(), "a backend outside the workspace was admitted");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("one"), "{:?}", notes[0]);
    assert!(notes[0].contains("not started"), "{:?}", notes[0]);
    assert!(notes[0].contains("path outside workspace"), "{:?}", notes[0]);
}

#[test]
fn a_backend_with_no_configured_directory_is_admitted_unchanged() {
    // No `cwd` means the child inherits the process's working directory —
    // unchanged by this pass. The inconsistency between that and the
    // workspace-root rule is the configuration's to resolve, not something a
    // run may silently rewrite.
    let dir = tempfile::tempdir().unwrap();
    let guard = guard(dir.path());

    let (admitted, notes) = admit_backends(&[config("one", None)], &guard, &spawns());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].cwd, None);
}

/// One delegation to a backend nothing ever starts, for the argument checks
/// below. Both refuse before the child would be forked, so the program named
/// here is never executed.
fn provider() -> SubagentToolProvider {
    SubagentToolProvider::new(
        Arc::new(agent_testkit::FakeEventStore::new()),
        Arc::new(agent_testkit::FakeToolProvider::new()),
        SessionId::from_string("parent"),
        &[config("fake", None)],
        Arc::new(Children::new()),
        Delegation::Await,
    )
}

/// The gateway refuses a name outside the backends this run has before the
/// tool is reached, so this is the second line: a caller that bypasses it —
/// or a declaration that stopped excluding the name — still gets a sentence
/// naming what the run does have rather than a bare "unknown".
#[tokio::test]
async fn a_backend_the_run_does_not_have_is_named_with_the_ones_it_has() {
    let error = provider()
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "ghost", "objective": "x" }),
        ))
        .await
        .expect_err("a backend this run does not have was accepted");

    let message = error.to_string();
    assert!(message.contains("'ghost' is not a backend this run has"), "{message}");
    assert!(message.contains("fake"), "it says what the run has: {message}");
}

/// Likewise for an argument the request does not define: the schema says
/// `additionalProperties: false`, and this is the check behind it.
#[tokio::test]
async fn an_argument_the_request_does_not_define_never_reaches_a_child() {
    let error = provider()
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "x", "timeout": "5s" }),
        ))
        .await
        .expect_err("an undefined argument was accepted");

    let message = error.to_string();
    assert!(message.contains("not a subagent request"), "{message}");
    assert!(message.contains("timeout_ms"), "it names the field it wanted: {message}");
}

/// A refusal that says only "cannot honour" reads as a permission the caller
/// must not work around: the live endpoint answered it by abandoning the
/// delegation rather than asking again without the knob it named, and the child
/// it wanted was one call away. So the message says nothing ran.
#[test]
fn a_refusal_that_started_nothing_says_trying_again_is_not_a_bypass() {
    use agent_ports::subagent::{SubagentFeature, SubagentFeatures};

    let error = DelegationError::Unsupported {
        provider: "inner".to_string(),
        features: [SubagentFeature::ToolAllowlist].into_iter().collect::<SubagentFeatures>(),
    };
    let text = delegation_refusal(&error);
    assert!(text.contains("tool_allowlist"), "it names the knob: {text}");
    assert!(text.contains("Nothing was started"), "it says what did not happen: {text}");
    assert!(text.contains("without the feature"), "it says the retry is allowed: {text}");

    // Every other refusal is passed through word for word: the advice is about
    // a request that was refused, and only that one never forked a session.
    let boundary = DelegationError::NoBoundary { reason: "no closed turn".to_string() };
    assert_eq!(delegation_refusal(&boundary), boundary.to_string());
}

#[test]
fn one_refused_backend_does_not_take_the_others_with_it() {
    // A misconfigured provider is skipped, not fatal: a run that has a backend
    // it can reach still has one.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let guard = guard(&root);

    let (admitted, notes) = admit_backends(
        &[config("bad", Some("../outside")), config("good", Some("sub"))],
        &guard,
        &spawns(),
    );
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].provider, "good");
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("bad"), "{:?}", notes[0]);
}

/// A backend that ends a child only when the test says so, and remembers
/// what was done to it.
///
/// The point of a background delegation is that something is still running
/// while the caller goes on, so the test has to be able to hold a child open
/// and then let it go — a backend that answered immediately could not tell a
/// call that waited from one that did not.
struct Releasable {
    released: tokio::sync::Semaphore,
    outcome: SubagentOutcome,
    disposals: Mutex<usize>,
}

impl Releasable {
    fn new(outcome: SubagentOutcome) -> Arc<Self> {
        Arc::new(Self {
            released: tokio::sync::Semaphore::new(0),
            outcome,
            disposals: Mutex::new(0),
        })
    }

    fn release(&self) {
        self.released.add_permits(1);
    }

    fn disposals(&self) -> usize {
        *self.disposals.lock().unwrap()
    }
}

#[async_trait::async_trait]
impl SubagentProvider for Releasable {
    fn name(&self) -> &str {
        "fake"
    }

    async fn spawn(
        &self,
        child_session_id: &SessionId,
        parent_turn_id: Option<&TurnId>,
        _request: SubagentRequest,
    ) -> Result<SubagentHandle, agent_ports::subagent::SubagentError> {
        Ok(SubagentHandle::new("fake", child_session_id.clone(), parent_turn_id.cloned()))
    }

    async fn wait(
        &self,
        _handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
        self.released.acquire().await.expect("the test's semaphore is never closed").forget();
        Ok(self.outcome.clone())
    }

    async fn cancel(
        &self,
        _handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
        Ok(SubagentOutcome::Cancelled)
    }

    async fn dispose(
        &self,
        _handle: &SubagentHandle,
    ) -> Result<(), agent_ports::subagent::SubagentError> {
        *self.disposals.lock().unwrap() += 1;
        Ok(())
    }
}

/// A store, a parent, and a registry — the three things a background
/// delegation leaves behind, without the fork and the process.
fn running(
    outcome: SubagentOutcome,
) -> (Arc<agent_testkit::FakeEventStore>, Arc<Children>, Arc<Releasable>, Arc<Child>) {
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    let child_session = SessionId::from_string("child-1");
    let backend = Releasable::new(outcome);
    let provider: Arc<dyn SubagentProvider> = backend.clone();
    let delegation = Arc::new(SubagentDelegation::new(
        store.clone(),
        Arc::new(agent_testkit::FakeToolProvider::new()),
        provider,
    ));
    let handle = SubagentHandle::new("fake", child_session, None);
    let children = Arc::new(Children::new());
    let child = children.started(delegation, SessionId::from_string("parent"), &handle);
    (store, children, backend, child)
}

async fn subagent_events(store: &agent_testkit::FakeEventStore) -> Vec<String> {
    store
        .read_stream(&SessionId::from_string("parent"), EventSeq::ZERO, usize::MAX)
        .await
        .expect("the parent's log")
        .into_iter()
        .filter(|event| event.event.event_type.as_str().starts_with("subagent/"))
        .map(|event| event.event.event_type.to_string())
        .collect()
}

#[tokio::test]
async fn a_collected_ending_is_recorded_once_and_the_handle_released_once() {
    // `subagent.wait` can be called twice for the same child — a model that
    // reads no answer the first time asks again — and two settlements would
    // be two endings for one child in a log that has to be able to say how
    // it ended.
    let (store, _children, backend, child) =
        running(SubagentOutcome::Completed { summary: "done".to_string(), result: None });
    backend.release();

    child.finish().await;
    child.finish().await;

    assert_eq!(
        subagent_events(store.as_ref()).await,
        vec![EventType::SUBAGENT_COMPLETED.to_string()],
        "the settlement is written once"
    );
    assert_eq!(backend.disposals(), 1, "the handle is released once");
}

#[tokio::test]
async fn draining_waits_for_a_child_that_has_not_ended() {
    // The promise the exit drain makes: a command that started a child
    // leaves nothing unrecorded. It waits — bounded by the child's own
    // ceiling, which the backend enforces — rather than dropping the child
    // when the command's own work is done.
    let (store, children, backend, _child) =
        running(SubagentOutcome::Failed { message: "broke".to_string(), side_effects: Vec::new() });

    let draining = {
        let children = children.clone();
        tokio::spawn(async move { children.drain().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!draining.is_finished(), "draining returned while a child was still running");
    assert!(
        subagent_events(store.as_ref()).await.is_empty(),
        "nothing is recorded before the child ends"
    );

    backend.release();
    let notes = draining.await.expect("the drain task");
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        subagent_events(store.as_ref()).await,
        vec![EventType::SUBAGENT_FAILED.to_string()],
        "the ending is recorded, under the family the outcome belongs to"
    );
    assert_eq!(backend.disposals(), 1);
}

#[tokio::test]
async fn a_backend_that_cannot_say_how_a_child_ended_is_reported_and_not_recorded() {
    // A settlement is a fact about how a child ended. When the backend cannot
    // say, there is no fact — and a log entry claiming one would be worse
    // than the absence, because nothing downstream could tell it was made up.
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    let backend: Arc<dyn SubagentProvider> = Arc::new(Silent);
    let delegation = Arc::new(SubagentDelegation::new(
        store.clone(),
        Arc::new(agent_testkit::FakeToolProvider::new()),
        backend,
    ));
    let handle = SubagentHandle::new("fake", SessionId::from_string("child-1"), None);
    let children = Arc::new(Children::new());
    let child = children.started(delegation, SessionId::from_string("parent"), &handle);

    let notes = child.finish().await;

    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("could not say how it ended"), "{notes:?}");
    assert!(subagent_events(store.as_ref()).await.is_empty(), "nothing may be recorded");
}

/// A backend whose `wait` fails: the child's fate is unknown to the parent.
struct Silent;

#[async_trait::async_trait]
impl SubagentProvider for Silent {
    fn name(&self) -> &str {
        "fake"
    }

    async fn spawn(
        &self,
        child_session_id: &SessionId,
        parent_turn_id: Option<&TurnId>,
        _request: SubagentRequest,
    ) -> Result<SubagentHandle, agent_ports::subagent::SubagentError> {
        Ok(SubagentHandle::new("fake", child_session_id.clone(), parent_turn_id.cloned()))
    }

    async fn wait(
        &self,
        handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
        Err(agent_ports::subagent::SubagentError::Unknown {
            provider: "fake".to_string(),
            child_session_id: handle.child_session_id.clone(),
        })
    }

    async fn cancel(
        &self,
        _handle: &SubagentHandle,
    ) -> Result<SubagentOutcome, agent_ports::subagent::SubagentError> {
        Ok(SubagentOutcome::Cancelled)
    }

    async fn dispose(
        &self,
        _handle: &SubagentHandle,
    ) -> Result<(), agent_ports::subagent::SubagentError> {
        Ok(())
    }
}

/// The wait tool, over a registry the test fills.
fn waiting(
    children: Arc<Children>,
    store: Arc<agent_testkit::FakeEventStore>,
) -> SubagentToolProvider {
    SubagentToolProvider::new(
        store,
        Arc::new(agent_testkit::FakeToolProvider::new()),
        SessionId::from_string("parent"),
        &[],
        children,
        Delegation::Background,
    )
}

#[tokio::test]
async fn waiting_names_the_children_it_has_when_it_is_given_one_it_does_not() {
    // A model that invents an id — or copies one from an earlier run — gets
    // told what this run can be asked about. "Unknown child" alone reads the
    // same whether the id was a typo or the child was in another process.
    let (store, children, backend, child) = running(SubagentOutcome::Cancelled);
    let provider = waiting(children, store);

    let error = provider
        .invoke(&ToolInvocation::proposed(
            WAIT_TOOL_NAME,
            json!({ "child_session_id": "not-a-child" }),
        ))
        .await
        .expect_err("an id this run does not have was accepted");

    let message = error.to_string();
    assert!(message.contains("is not a child this run started"), "{message}");
    assert!(message.contains(child.id().as_str()), "it names what it has: {message}");
    backend.release();
}

#[tokio::test]
async fn waiting_with_no_id_takes_the_one_child_this_run_has_open() {
    // The common case is one delegation, and making the model copy an id it
    // was just handed is a way to lose a child: a mistyped id is a refusal,
    // not an answer.
    let (store, children, backend, child) =
        running(SubagentOutcome::Partial { summary: "two of the three".to_string() });
    backend.release();
    let provider = waiting(children, store);

    let result = provider
        .invoke(&ToolInvocation::proposed(WAIT_TOOL_NAME, json!({})))
        .await
        .expect("waiting for the only child");

    assert_eq!(result.status, ToolStatus::Completed);
    assert!(result.content.contains(child.id().as_str()), "{}", result.content);
    assert!(result.content.contains("partial: two of the three"), "{}", result.content);
}

#[tokio::test]
async fn waiting_with_no_id_refuses_to_choose_between_children() {
    // Two children and no name is an ambiguous question. Picking one would
    // make the wrong answer look like the right one.
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    let children = Arc::new(Children::new());
    let provider = waiting(children.clone(), store.clone());
    for id in ["child-1", "child-2"] {
        let backend = Releasable::new(SubagentOutcome::Cancelled);
        let provider_handle: Arc<dyn SubagentProvider> = backend.clone();
        let delegation = Arc::new(SubagentDelegation::new(
            store.clone(),
            Arc::new(agent_testkit::FakeToolProvider::new()),
            provider_handle,
        ));
        let handle = SubagentHandle::new("fake", SessionId::from_string(id), None);
        children.started(delegation, SessionId::from_string("parent"), &handle);
        backend.release();
    }

    let error = provider
        .invoke(&ToolInvocation::proposed(WAIT_TOOL_NAME, json!({})))
        .await
        .expect_err("an ambiguous wait was answered");

    let message = error.to_string();
    assert!(message.contains("2 children"), "{message}");
    assert!(message.contains("child-1") && message.contains("child-2"), "{message}");
}

#[tokio::test]
async fn status_reads_the_sessions_own_log_including_children_from_earlier_runs() {
    // The report is about the session, not about this process: a child
    // delegated by a previous command is still a fact about the session, and
    // a status that forgot it would read as "you never delegated".
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    let parent = SessionId::from_string("parent");
    append(
        &store,
        &parent,
        EventType::SUBAGENT_SPAWNED,
        json!({
            "child_session_id": "child-1",
            "provider": "fake",
            "request": { "objective": "read the notes" },
        }),
    )
    .await;
    append(
        &store,
        &parent,
        EventType::SUBAGENT_COMPLETED,
        json!({
            "child_session_id": "child-1",
            "provider": "fake",
            "outcome": { "status": "completed", "summary": "read them" },
        }),
    )
    .await;

    let provider = waiting(Arc::new(Children::new()), store);
    let result = provider
        .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
        .await
        .expect("a status report");

    assert!(result.content.contains("child-1 (fake) completed: read them"), "{}", result.content);
    assert!(result.content.contains("read the notes"), "{}", result.content);
    assert_eq!(result.status, ToolStatus::Completed);
}

#[tokio::test]
async fn status_says_so_when_the_session_has_delegated_nothing() {
    let provider =
        waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
    let result = provider
        .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
        .await
        .expect("a status report");
    assert!(result.content.contains("delegated to no child"), "{}", result.content);
    assert!(
        result.content.contains("subagent.run"),
        "it says how to start one: {}",
        result.content
    );
}

#[tokio::test]
async fn status_reports_a_child_that_has_ended_but_not_been_recorded() {
    // The window between a child ending and someone collecting it is real:
    // the status report is the only place a model can see it, and calling it
    // "running" would be wrong in the direction that matters.
    let (store, children, backend, child) =
        running(SubagentOutcome::Completed { summary: "done".to_string(), result: None });
    append(
        &store,
        &SessionId::from_string("parent"),
        EventType::SUBAGENT_SPAWNED,
        json!({
            "child_session_id": child.id().as_str(),
            "provider": "fake",
            "request": { "objective": "do the thing" },
        }),
    )
    .await;
    backend.release();
    // Let the background task publish the ending.
    while child.ending_now().is_none() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let provider = waiting(children, store);
    let result = provider
        .invoke(&ToolInvocation::proposed(STATUS_TOOL_NAME, json!({})))
        .await
        .expect("a status report");

    assert!(result.content.contains("completed: done (not yet recorded)"), "{}", result.content);
}

#[tokio::test]
async fn a_name_this_provider_does_not_implement_names_the_ones_it_does() {
    let provider =
        waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
    let error = provider
        .invoke(&ToolInvocation::proposed("subagent.cancel", json!({})))
        .await
        .expect_err("an unimplemented name was accepted");
    let message = error.to_string();
    assert!(message.contains(TOOL_NAME), "{message}");
    assert!(message.contains(WAIT_TOOL_NAME), "{message}");
    assert!(message.contains(STATUS_TOOL_NAME), "{message}");
}

#[test]
fn the_collection_tools_are_held_to_the_capability_that_allows_delegating() {
    // A run that may not delegate has no delegation of its own to read, and
    // a tool that declares nothing is refused by the policy engine as surely
    // as one whose rules forbid it — so all three name the same capability
    // rather than leaving it to the gateway to notice.
    let provider =
        waiting(Arc::new(Children::new()), Arc::new(agent_testkit::FakeEventStore::new()));
    let definitions = provider.definitions();
    assert_eq!(
        definitions.len(),
        3,
        "{:?}",
        definitions.iter().map(|d| &d.name).collect::<Vec<_>>()
    );
    for definition in &definitions {
        assert_eq!(definition.capability_requirements.len(), 1);
        assert_eq!(definition.capability_requirements[0].as_str(), REQUIRED_CAPABILITY);
    }
    // The collection verbs touch nothing, and say so: the boundary treats
    // "no answer" as a write.
    assert_eq!(provider.side_effect(WAIT_TOOL_NAME), Some(ToolSideEffect::Pure));
    assert_eq!(provider.side_effect(STATUS_TOOL_NAME), Some(ToolSideEffect::Pure));
    assert_eq!(provider.side_effect(TOOL_NAME), Some(ToolSideEffect::Destructive));
}

async fn append(
    store: &agent_testkit::FakeEventStore,
    session: &SessionId,
    event_type: &str,
    payload: serde_json::Value,
) {
    let seq = store
        .read_stream(session, EventSeq::ZERO, usize::MAX)
        .await
        .expect("readable")
        .last()
        .map(|event| event.seq.next())
        .unwrap_or(EventSeq::ZERO);
    let draft = EventDraft {
        event_id: EventId::new(),
        event_type: EventType::new(event_type),
        schema_version: SchemaVersion::current(),
        session_id: session.clone(),
        run_id: None,
        turn_id: None,
        step_id: None,
        actor: Actor::System,
        created_at: chrono::Utc::now(),
        causation_id: None,
        correlation_id: None,
        payload,
        metadata: json!({}),
        ignorable: false,
    };
    store.append(session, seq, vec![draft]).await.expect("an append");
}

/// A provider over a log this test fills, for the admission checks.
///
/// Nothing here ever reaches a child: every case below is decided before
/// the fork, which is the point of admitting a step at all.
fn admitting(store: Arc<agent_testkit::FakeEventStore>) -> SubagentToolProvider {
    SubagentToolProvider::new(
        store,
        Arc::new(agent_testkit::FakeToolProvider::new()),
        SessionId::from_string("parent"),
        &[config("fake", None)],
        Arc::new(Children::new()),
        Delegation::Background,
    )
}

async fn plan_of(store: &agent_testkit::FakeEventStore, plan: serde_json::Value) {
    append(
        store,
        &SessionId::from_string("parent"),
        EventType::PLAN_UPDATED,
        json!({ "plan": plan }),
    )
    .await;
}

/// The step two of the tests below share: open, and waiting on `read`.
fn a_graph() -> serde_json::Value {
    json!({
        "goal": "ship it",
        "items": [
            { "text": "read the failing test", "status": "done", "id": "read" },
            { "text": "fix the tokenizer", "status": "todo", "id": "fix", "depends_on": ["read"] },
            { "text": "run the suite", "status": "todo", "id": "test", "depends_on": ["fix"] },
        ],
    })
}

#[tokio::test]
async fn a_step_is_refused_while_the_step_it_waits_on_is_open() {
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    plan_of(store.as_ref(), a_graph()).await;

    let error = admitting(store)
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "run it", "step": "test" }),
        ))
        .await
        .expect_err("a step the plan has not reached was admitted");

    let message = error.to_string();
    assert!(message.contains("waits on fix"), "{message}");
    assert!(message.contains("not done"), "{message}");
}

#[tokio::test]
async fn a_step_one_worker_already_carries_refuses_a_second() {
    // Two children on one step is the same work done twice, and the second
    // one is invisible in the task layer: the fold keeps the first child's
    // entry and the second only looks like a delegation.
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    plan_of(store.as_ref(), a_graph()).await;
    append(
        store.as_ref(),
        &SessionId::from_string("parent"),
        EventType::SUBAGENT_SPAWNED,
        json!({
            "child_session_id": "child-1",
            "provider": "fake",
            "request": { "objective": "fix it", "step": "fix" },
        }),
    )
    .await;

    let error = admitting(store)
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "fix it again", "step": "fix" }),
        ))
        .await
        .expect_err("a step with a worker was given another");

    let message = error.to_string();
    assert!(message.contains("already has a worker: subagent child-1"), "{message}");
}

#[tokio::test]
async fn a_step_the_plan_does_not_name_lists_the_ones_it_does() {
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    plan_of(store.as_ref(), a_graph()).await;

    let error = admitting(store)
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "x", "step": "deploy" }),
        ))
        .await
        .expect_err("a step the plan does not have was admitted");

    let message = error.to_string();
    assert!(message.contains("read, fix, test"), "{message}");
}

#[tokio::test]
async fn naming_a_step_in_a_session_with_no_plan_says_to_write_one() {
    let store = Arc::new(agent_testkit::FakeEventStore::new());

    let error = admitting(store)
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "x", "step": "fix" }),
        ))
        .await
        .expect_err("a step was admitted with no plan at all");

    let message = error.to_string();
    assert!(message.contains("names no step ids"), "{message}");
    assert!(message.contains("plan.update"), "{message}");
}

#[tokio::test]
async fn a_step_that_is_done_or_already_doing_is_not_a_worker_s_step() {
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    plan_of(
        store.as_ref(),
        json!({
            "goal": "ship it",
            "items": [
                { "text": "read", "status": "done", "id": "read" },
                { "text": "fix", "status": "doing", "id": "fix" },
            ],
        }),
    )
    .await;
    let provider = admitting(store);

    for (step, expected) in [("read", "already done"), ("fix", "already doing")] {
        let error = provider
            .invoke(&ToolInvocation::proposed(
                TOOL_NAME,
                json!({ "provider": "fake", "objective": "x", "step": step }),
            ))
            .await
            .expect_err("a step with no work left was given a worker");
        assert!(error.to_string().contains(expected), "{step}: {error}");
    }
}

#[tokio::test]
async fn a_ready_step_passes_admission_and_the_request_carries_it() {
    // The gate opening is what this checks: what happens next is the fork,
    // and this session has no closed turn for one, so the call fails there
    // rather than here — with a reason that says so.
    let store = Arc::new(agent_testkit::FakeEventStore::new());
    plan_of(store.as_ref(), a_graph()).await;
    let provider = admitting(store);

    provider.admit_step("fix").await.expect("a ready step is admitted");

    let error = provider
        .invoke(&ToolInvocation::proposed(
            TOOL_NAME,
            json!({ "provider": "fake", "objective": "fix it", "step": "fix" }),
        ))
        .await
        .expect_err("no closed turn to fork from");
    assert!(!error.to_string().contains("already"), "the gate let it through: {error}");
}

#[test]
fn a_request_that_names_a_step_carries_it_and_one_that_does_not_stays_byte_identical() {
    let named: RunArgs =
        serde_json::from_value(json!({ "provider": "fake", "objective": "x", "step": "fix" }))
            .expect("arguments");
    assert_eq!(named.into_request().step.as_deref(), Some("fix"));

    let plain: RunArgs =
        serde_json::from_value(json!({ "provider": "fake", "objective": "x" })).expect("arguments");
    let request = plain.into_request();
    assert_eq!(request.step, None);
    assert!(
        !serde_json::to_string(&request).expect("serialize").contains("step"),
        "an unnamed step is not written at all"
    );
}

#[test]
fn the_receipt_names_the_step_the_child_carries() {
    let child = SessionId::from_string("child-1");
    assert!(receipt(&child, Some("fix")).contains("It carries step \"fix\""));
    assert!(!receipt(&child, None).contains("carries"), "a plain receipt is unchanged");
}

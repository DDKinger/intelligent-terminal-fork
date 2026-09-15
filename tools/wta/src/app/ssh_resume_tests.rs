use super::*;
use crate::agent_sessions::SessionOrigin;
use crate::app::tests::test_app_with_master_rx;
use crate::shell::wt_channel::WtChannel;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

const PANE: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_PANE: &str = "22222222-2222-4222-8222-222222222222";

struct Reply {
    method: &'static str,
    result: std::result::Result<Value, String>,
}

fn created(pane_id: &str) -> Reply {
    Reply {
        method: "create_tab",
        result: Ok(json!({ "session_id": pane_id })),
    }
}

fn focused() -> Reply {
    Reply {
        method: "focus_pane",
        result: Ok(json!({})),
    }
}

fn failed(method: &'static str, message: &str) -> Reply {
    Reply {
        method,
        result: Err(message.to_string()),
    }
}

struct MockTerminal {
    calls: Mutex<Vec<(String, Value)>>,
    replies: Mutex<VecDeque<Reply>>,
    called: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl WtChannel for MockTerminal {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.calls
            .lock()
            .unwrap()
            .push((method.to_string(), params));
        self.called.notify_one();
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("Unexpected terminal request: {method}"))?;
        assert_eq!(reply.method, method);
        reply.result.map_err(anyhow::Error::msg)
    }

    fn is_available(&self) -> bool {
        true
    }
}

impl MockTerminal {
    async fn wait_calls(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notified = self.called.notified();
                if self.calls.lock().unwrap().len() >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("terminal calls should complete");
    }
}

struct Harness {
    app: App,
    terminal: Arc<MockTerminal>,
    events: mpsc::UnboundedReceiver<AppEvent>,
    master: mpsc::UnboundedReceiver<crate::protocol::acp::client::MasterExtRequest>,
}

impl Harness {
    fn new(replies: Vec<Reply>) -> Self {
        let (mut app, master) = test_app_with_master_rx();
        app.current_agent_id = "copilot".into();
        app.state = ConnectionState::Connected;
        let terminal = Arc::new(MockTerminal {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(replies.into()),
            called: tokio::sync::Notify::new(),
        });
        app.shell_mgr =
            Arc::new(crate::shell::ShellManager::new().with_wt_channel(terminal.clone()));
        let (sender, events) = mpsc::unbounded_channel();
        app.event_tx = Some(sender);
        show_source(
            &mut app,
            &source("remote"),
            vec![history(&source("remote"))],
        );
        Self {
            app,
            terminal,
            events,
            master,
        }
    }

    fn enter(&mut self) {
        self.app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    async fn next_event(&mut self) {
        let event = tokio::time::timeout(Duration::from_secs(2), self.events.recv())
            .await
            .expect("resume event timed out")
            .expect("event channel closed");
        self.app.handle_event(event);
    }

    fn row(&self) -> AgentSession {
        self.app
            .agents_rows_for_tab(DEFAULT_TAB_ID)
            .into_iter()
            .next()
            .unwrap()
    }

    fn close_pane(&mut self, pane_id: &str, state: &str) {
        self.app.handle_event(AppEvent::WtEvent {
            method: "connection_state".into(),
            pane_id: pane_id.to_string(),
            tab_id: Some("resumed-tab".into()),
            params: json!({ "state": state }),
        });
    }
}

fn source(destination: &str) -> ssh_session_view::SshSessionsSource {
    ssh_session_view::SshSessionsSource {
        target: SshTarget::new(destination, None).unwrap(),
        agent_id: "copilot".into(),
    }
}

fn history(source: &ssh_session_view::SshSessionsSource) -> AgentSession {
    let mut info =
        agent_client_protocol::schema::v1::SessionInfo::new("same-id", "/home/me/project");
    info.title = Some("Remote conversation".into());
    crate::session_history::acp_session_to_agent_session(
        &info,
        SessionLocation::Ssh {
            target: source.target.clone(),
        },
        &CliSource::parse(Some(&source.agent_id)),
    )
}

fn show_source(
    app: &mut App,
    source: &ssh_session_view::SshSessionsSource,
    rows: Vec<AgentSession>,
) {
    app.current_agent_id.clone_from(&source.agent_id);
    let tab = app.current_tab_mut();
    tab.current_view = View::Agents;
    tab.agents_view.ssh_profile = super::ssh_profile::SessionsProfile::Ssh(source.target.clone());
    tab.agents_view.ssh_source = Some(source.clone());
    tab.agents_view.snapshot = Some(
        rows.iter()
            .map(crate::session_registry::agent_session_to_session_info)
            .collect(),
    );
    tab.agents_list_state.select(Some(0));
    tab.agents_view.focused_sid =
        Some(agent_client_protocol::schema::v1::SessionId::new("same-id"));
    tab.agents_view.refetch_in_flight = false;
}

fn refresh(app: &mut App, source: &ssh_session_view::SshSessionsSource, rows: Vec<AgentSession>) {
    let tab = app.current_tab_mut();
    tab.agents_view.refetch_in_flight = true;
    tab.agents_view.latest_request_id = Some(42);
    app.handle_event(AppEvent::SshSessionsLoaded {
        tab_id: DEFAULT_TAB_ID.to_string(),
        request_id: 42,
        target: source.target.clone(),
        agent_id: source.agent_id.clone(),
        result: Ok(rows),
    });
}

#[tokio::test]
async fn successful_resume_displays_idle_with_unknown_origin_and_repeat_enter_focuses() {
    let _locale = crate::test_support::lock_locale();
    rust_i18n::set_locale("en-US");
    let mut h = Harness::new(vec![created(PANE), focused(), focused()]);
    h.enter();
    assert!(h.app.ssh_resume_pending(&h.row()));
    assert_eq!(h.row().status, AgentStatus::Historical);
    h.enter();
    h.next_event().await;
    h.terminal.wait_calls(2).await;
    let row = h.row();
    assert_eq!(row.status, AgentStatus::Idle);
    assert_eq!(row.pane_session_id.as_deref(), Some(PANE));
    assert_eq!(row.origin, SessionOrigin::Unknown);
    assert_eq!(h.app.agent_sessions.iter_sorted().len(), 0);
    assert!(h.master.try_recv().is_err());

    let snapshot = h.app.current_tab().agents_view.snapshot.as_ref().unwrap();
    let json = serde_json::to_value(&snapshot[0]).unwrap();
    assert_eq!(json["status"], "Idle");
    assert_eq!(json["origin"], "Unknown");
    assert_eq!(json["pane_session_id"], PANE);

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 12)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut h.app))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("Remote conversation"));
    assert!(text.contains("Idle"));

    h.enter();
    h.terminal.wait_calls(3).await;
    let calls = h.terminal.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(method, _)| method == "create_tab")
            .count(),
        1
    );
    assert_eq!(
        calls.last().unwrap(),
        &("focus_pane".into(), json!({ "session_id": PANE }))
    );
    assert!(calls[0].1["commandline"]
        .as_str()
        .unwrap()
        .contains("ssh.exe"));
    assert!(calls[0].1.get("cwd").is_none());
    assert!(h.master.try_recv().is_err());
}

#[tokio::test]
async fn refresh_and_reopen_preserve_the_live_binding_even_if_history_temporarily_omits_it() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), focused()]);
    h.enter();
    h.next_event().await;
    refresh(
        &mut h.app,
        &source("remote"),
        vec![history(&source("remote"))],
    );
    assert_eq!(h.row().status, AgentStatus::Idle);
    assert_eq!(h.row().pane_session_id.as_deref(), Some(PANE));
    refresh(&mut h.app, &source("remote"), Vec::new());
    assert_eq!(h.row().status, AgentStatus::Idle);

    h.app.close_agents_view_for_tab(DEFAULT_TAB_ID);
    h.app.current_tab_mut().agents_view.refetch_in_flight = true;
    h.app.open_agents_view_for_tab(DEFAULT_TAB_ID.to_string());
    assert_eq!(h.row().status, AgentStatus::Idle);
    assert_eq!(h.app.current_tab().agents_list_state.selected(), Some(0));
    assert!(h.master.try_recv().is_err());
}

#[tokio::test]
async fn create_failure_releases_pending_state_and_allows_retry_without_false_idle() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![
        failed("create_tab", "creation failed"),
        created(PANE),
        focused(),
    ]);
    h.enter();
    h.next_event().await;
    assert!(!h.app.ssh_resume_pending(&h.row()));
    assert_eq!(h.row().status, AgentStatus::Historical);
    assert!(h.row().pane_session_id.is_none());
    assert!(h
        .app
        .current_tab()
        .agents_view
        .ssh_error
        .as_deref()
        .unwrap()
        .contains("creation failed"));
    h.enter();
    h.next_event().await;
    assert_eq!(h.row().status, AgentStatus::Idle);
    assert!(h.app.current_tab().agents_view.ssh_error.is_none());
}

#[tokio::test]
async fn malformed_creation_responses_do_not_publish_idle_or_focus_an_arbitrary_pane() {
    let _locale = crate::test_support::lock_locale();
    for response in [
        json!({}),
        json!({ "session_id": "" }),
        json!({ "session_id": "-t" }),
        json!({ "session_id": 1 }),
        json!({ "session_id": uuid::Uuid::nil().to_string() }),
    ] {
        let mut h = Harness::new(vec![Reply {
            method: "create_tab",
            result: Ok(response),
        }]);
        h.enter();
        h.next_event().await;
        assert_eq!(h.row().status, AgentStatus::Historical);
        assert!(h.row().pane_session_id.is_none());
        assert!(h.app.current_tab().agents_view.ssh_error.is_some());
        assert!(h.app.ssh_resumes.bindings.is_empty());
        assert_eq!(h.terminal.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn native_close_or_failure_ends_only_the_bound_remote_pane_and_allows_resume() {
    let _locale = crate::test_support::lock_locale();
    for state in ["closed", "failed"] {
        let mut h = Harness::new(vec![
            created(PANE),
            focused(),
            created(OTHER_PANE),
            focused(),
        ]);
        h.enter();
        h.next_event().await;
        h.close_pane(OTHER_PANE, state);
        assert_eq!(h.row().status, AgentStatus::Idle);
        h.close_pane(PANE, state);
        assert_eq!(h.row().status, AgentStatus::Ended);
        assert!(h.row().pane_session_id.is_none());
        refresh(
            &mut h.app,
            &source("remote"),
            vec![history(&source("remote"))],
        );
        assert_eq!(h.row().status, AgentStatus::Ended);
        h.enter();
        h.next_event().await;
        assert_eq!(h.row().status, AgentStatus::Idle);
        assert_eq!(h.row().pane_session_id.as_deref(), Some(OTHER_PANE));
    }
}

#[tokio::test]
async fn closure_before_creation_reply_cannot_resurrect_a_dead_pane() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), focused()]);
    h.enter();
    h.close_pane(&format!("{{{PANE}}}"), "closed");
    h.next_event().await;
    assert_eq!(h.row().status, AgentStatus::Ended);
    assert!(h.row().pane_session_id.is_none());
    assert!(h.app.ssh_resumes.closed_while_launching.is_empty());
}

#[tokio::test]
async fn focus_infrastructure_failure_preserves_idle_but_missing_pane_ends_it() {
    let _locale = crate::test_support::lock_locale();
    for (message, status) in [
        ("RPC unavailable", AgentStatus::Idle),
        ("FocusPane failed: 0x80070490", AgentStatus::Ended),
    ] {
        let mut h = Harness::new(vec![
            created(PANE),
            focused(),
            failed("focus_pane", message),
        ]);
        h.enter();
        h.next_event().await;
        h.terminal.wait_calls(2).await;
        h.enter();
        h.next_event().await;
        assert_eq!(h.row().status, status);
        assert_eq!(
            h.row().pane_session_id.is_some(),
            status == AgentStatus::Idle
        );
        assert!(h
            .app
            .current_tab()
            .agents_view
            .ssh_error
            .as_deref()
            .unwrap()
            .contains(message));
        assert!(h.master.try_recv().is_err());
    }
}

#[tokio::test]
async fn automatic_focus_failure_keeps_the_created_pane_bound() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), failed("focus_pane", "focus failed")]);
    h.enter();
    h.next_event().await;
    h.next_event().await;
    assert_eq!(h.row().status, AgentStatus::Idle);
    assert_eq!(h.row().pane_session_id.as_deref(), Some(PANE));
    assert!(h
        .app
        .current_tab()
        .agents_view
        .ssh_error
        .as_deref()
        .unwrap()
        .contains("focus failed"));
}

#[tokio::test]
async fn late_completion_after_source_switch_never_changes_the_foreign_view() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), focused()]);
    h.enter();
    let foreign = source("other-host");
    show_source(&mut h.app, &foreign, vec![history(&foreign)]);
    h.next_event().await;
    assert_eq!(h.row().status, AgentStatus::Historical);
    assert!(h.row().pane_session_id.is_none());
    assert!(h.app.current_tab().agents_view.ssh_error.is_none());
    show_source(
        &mut h.app,
        &source("remote"),
        vec![history(&source("remote"))],
    );
    h.app.refresh_ssh_resume_snapshots();
    assert_eq!(h.row().status, AgentStatus::Idle);
}

#[tokio::test]
async fn binding_identity_includes_host_port_agent_and_session_not_just_session_id() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), focused()]);
    h.enter();
    h.next_event().await;
    for foreign in [
        source("other-host"),
        ssh_session_view::SshSessionsSource {
            target: SshTarget::new("remote", Some(2222)).unwrap(),
            agent_id: "copilot".into(),
        },
        ssh_session_view::SshSessionsSource {
            target: SshTarget::new("remote", None).unwrap(),
            agent_id: "claude".into(),
        },
    ] {
        show_source(&mut h.app, &foreign, vec![history(&foreign)]);
        h.app.refresh_ssh_resume_snapshots();
        assert_eq!(h.row().status, AgentStatus::Historical);
        assert!(h.row().pane_session_id.is_none());
    }
    let own = source("remote");
    let mut another = history(&own);
    another.key = "different-id".to_string();
    show_source(&mut h.app, &own, vec![another]);
    h.app.refresh_ssh_resume_snapshots();
    let rows = h.app.agents_rows_for_tab(DEFAULT_TAB_ID);
    assert_eq!(
        rows.iter()
            .find(|row| row.key == "different-id")
            .unwrap()
            .status,
        AgentStatus::Historical
    );
    assert_eq!(
        rows.iter().find(|row| row.key == "same-id").unwrap().status,
        AgentStatus::Idle
    );
    assert!(h.master.try_recv().is_err());
}

#[tokio::test]
async fn old_operation_results_cannot_overwrite_a_replacement_binding() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![
        created(PANE),
        focused(),
        created(OTHER_PANE),
        focused(),
    ]);
    h.enter();
    h.next_event().await;
    let key = SshSessionKey::from_session(&h.row()).unwrap();
    let old_id = h.app.ssh_resumes.bindings[&key].operation_id;
    h.close_pane(PANE, "closed");
    h.enter();
    h.app.handle_event(AppEvent::SshSessionResumeCompleted {
        key: key.clone(),
        operation_id: old_id,
        outcome: SshResumeOutcome::Created(Err("stale creation".into())),
    });
    assert!(h.app.ssh_resume_pending(&h.row()));
    h.next_event().await;
    h.app.handle_event(AppEvent::SshSessionResumeCompleted {
        key,
        operation_id: old_id,
        outcome: SshResumeOutcome::FocusFailed {
            pane_id: PANE.into(),
            error: "0x80070490".into(),
        },
    });
    assert_eq!(h.row().status, AgentStatus::Idle);
    assert_eq!(h.row().pane_session_id.as_deref(), Some(OTHER_PANE));
}

#[tokio::test]
async fn binding_survives_owner_tab_rename_and_closing_the_view() {
    let _locale = crate::test_support::lock_locale();
    let mut h = Harness::new(vec![created(PANE), focused()]);
    h.enter();
    h.app.owner_tab_id = Some(DEFAULT_TAB_ID.into());
    h.app.tab_id = Some(DEFAULT_TAB_ID.into());
    h.app
        .rename_tab_session(DEFAULT_TAB_ID, "renamed-tab", Some("new-window"));
    h.next_event().await;
    let rows = h.app.agents_rows_for_tab("renamed-tab");
    assert_eq!(rows[0].status, AgentStatus::Idle);
    h.app.close_agents_view_for_tab("renamed-tab");
    h.close_pane(PANE, "closed");
    let binding = h.app.ssh_resumes.bindings.values().next().unwrap();
    assert!(matches!(binding.phase, ResumePhase::Ended));
    assert!(binding.session.pane_session_id.is_none());
}

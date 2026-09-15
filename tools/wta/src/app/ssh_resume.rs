//! Helper-owned pane bindings for explicitly resumed SSH history.
//! Remote session IDs never enter the host/WSL registry.

use super::*;
use crate::agent_sessions::{AgentSession, AgentStatus, CliSource, SessionLocation};
use crate::ssh_sessions::SshTarget;

#[cfg(test)]
#[path = "ssh_resume_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SshSessionKey {
    target: SshTarget,
    cli: CliSource,
    session_id: String,
}

impl SshSessionKey {
    fn from_session(session: &AgentSession) -> Option<Self> {
        let SessionLocation::Ssh { target } = &session.location else {
            return None;
        };
        Some(Self {
            target: target.clone(),
            cli: session.cli_source.clone(),
            session_id: session.key.clone(),
        })
    }

    fn matches_source(&self, source: &ssh_session_view::SshSessionsSource) -> bool {
        self.target == source.target
            && CliSource::from_agent_id(&source.agent_id).as_ref() == Some(&self.cli)
    }
}

#[derive(Debug)]
pub(crate) enum SshResumeOutcome {
    Created(std::result::Result<String, String>),
    FocusFailed { pane_id: String, error: String },
}

#[derive(Debug)]
enum ResumePhase {
    Launching,
    Bound,
    Ended,
}

struct ResumeBinding {
    operation_id: uuid::Uuid,
    phase: ResumePhase,
    session: AgentSession,
}

#[derive(Default)]
pub(crate) struct SshResumes {
    bindings: HashMap<SshSessionKey, ResumeBinding>,
    // A short-lived pane may close before create_tab's response reaches us.
    // Retain only closures observed while creation requests are outstanding.
    closed_while_launching: HashSet<String>,
}

impl SshResumes {
    pub(super) fn merge_rows(
        &self,
        rows: &mut Vec<AgentSession>,
        source: &ssh_session_view::SshSessionsSource,
    ) {
        for (key, binding) in &self.bindings {
            if !key.matches_source(source) {
                continue;
            }
            if let Some(row) = rows
                .iter_mut()
                .find(|row| SshSessionKey::from_session(row).as_ref() == Some(key))
            {
                if !matches!(binding.phase, ResumePhase::Launching) {
                    row.status = binding.session.status.clone();
                    row.pane_session_id = binding.session.pane_session_id.clone();
                    row.last_activity_at =
                        row.last_activity_at.max(binding.session.last_activity_at);
                }
            } else if !matches!(binding.phase, ResumePhase::Ended) {
                rows.push(binding.session.clone());
            }
        }
    }

    fn clear_finished_closures(&mut self) {
        if !self
            .bindings
            .values()
            .any(|binding| matches!(binding.phase, ResumePhase::Launching))
        {
            self.closed_while_launching.clear();
        }
    }
}

fn pane_gone(error: &str) -> bool {
    // This is the existing FocusPane ERROR_NOT_FOUND contract, not a generic
    // process/RPC failure: only a confirmed missing pane ends the binding.
    error.contains("0x80070490")
}

impl App {
    pub(super) fn ssh_resume_pending(&self, session: &AgentSession) -> bool {
        SshSessionKey::from_session(session)
            .and_then(|key| self.ssh_resumes.bindings.get(&key))
            .is_some_and(|binding| matches!(binding.phase, ResumePhase::Launching))
    }

    pub(super) fn start_ssh_session_resume(
        &mut self,
        session: &AgentSession,
        commandline: String,
    ) -> Result<()> {
        let key = SshSessionKey::from_session(session)
            .ok_or_else(|| anyhow::anyhow!("SSH resume requires an SSH session source."))?;
        if let Some(binding) = self.ssh_resumes.bindings.get(&key) {
            match binding.phase {
                ResumePhase::Launching => return Ok(()),
                ResumePhase::Bound => {
                    let bound = binding.session.clone();
                    if let Some(pane_id) = &bound.pane_session_id {
                        self.dispatch_ssh_session_focus(&bound, pane_id);
                    }
                    return Ok(());
                }
                ResumePhase::Ended => {}
            }
        }
        let event_tx = self
            .event_tx
            .clone()
            .ok_or_else(|| anyhow::anyhow!("SSH session resume requires the helper event loop."))?;
        let runtime = tokio::runtime::Handle::try_current()?;
        let operation_id = uuid::Uuid::new_v4();
        self.ssh_resumes.bindings.insert(
            key.clone(),
            ResumeBinding {
                operation_id,
                phase: ResumePhase::Launching,
                session: session.clone(),
            },
        );
        self.set_ssh_resume_error(&key, None);
        let shell = self.shell_mgr.clone();
        let title = session.title.clone();
        #[cfg(test)]
        {
            let mut argv = vec!["new-tab".to_string(), "-c".to_string(), commandline.clone()];
            if !title.is_empty() {
                argv.extend(["--title".to_string(), title.clone()]);
            }
            self.last_dispatched_command = Some(DispatchedCommand {
                kind: DispatchedCommandKind::NewTabResume,
                session_id: None,
                argv,
            });
        }
        runtime.spawn(async move {
            let result = shell
                .wt_create_tab(Some(&commandline), None, Some(&title), None)
                .await
                .and_then(|response| {
                    crate::coordinator::resolve_created_pane_id(&response, "SSH resume")
                })
                .and_then(|id| {
                    let id = uuid::Uuid::parse_str(&id)?;
                    anyhow::ensure!(!id.is_nil(), "SSH resume returned an empty pane GUID.");
                    Ok(id.to_string())
                })
                .map_err(|error: anyhow::Error| format!("{error:#}"));
            let pane_id = result.as_ref().ok().cloned();
            if event_tx
                .send(AppEvent::SshSessionResumeCompleted {
                    key: key.clone(),
                    operation_id,
                    outcome: SshResumeOutcome::Created(result),
                })
                .is_err()
            {
                return;
            }
            if let Some(pane_id) = pane_id {
                if let Err(error) = shell.wt_focus_pane(&pane_id).await {
                    let _ = event_tx.send(AppEvent::SshSessionResumeCompleted {
                        key,
                        operation_id,
                        outcome: SshResumeOutcome::FocusFailed {
                            pane_id,
                            error: format!("{error:#}"),
                        },
                    });
                }
            }
        });
        Ok(())
    }

    pub(super) fn handle_ssh_resume_completed(
        &mut self,
        key: SshSessionKey,
        operation_id: uuid::Uuid,
        outcome: SshResumeOutcome,
    ) {
        let Some(binding) = self.ssh_resumes.bindings.get_mut(&key) else {
            return;
        };
        if binding.operation_id != operation_id {
            return;
        }
        let mut error = None;
        match outcome {
            SshResumeOutcome::Created(result) => {
                if !matches!(binding.phase, ResumePhase::Launching) {
                    return;
                }
                match result {
                    Ok(pane_id) => {
                        if self.ssh_resumes.closed_while_launching.contains(&pane_id) {
                            binding.phase = ResumePhase::Ended;
                            binding.session.status = AgentStatus::Ended;
                            binding.session.pane_session_id = None;
                        } else {
                            binding.phase = ResumePhase::Bound;
                            binding.session.status = AgentStatus::Idle;
                            binding.session.pane_session_id = Some(pane_id);
                            binding.session.last_activity_at = std::time::SystemTime::now();
                        }
                    }
                    Err(message) => {
                        error = Some(message);
                        self.ssh_resumes.bindings.remove(&key);
                    }
                }
            }
            SshResumeOutcome::FocusFailed {
                pane_id,
                error: message,
            } => {
                if binding.session.pane_session_id.as_deref() != Some(&pane_id) {
                    return;
                }
                if pane_gone(&message) {
                    binding.phase = ResumePhase::Ended;
                    binding.session.status = AgentStatus::Ended;
                    binding.session.pane_session_id = None;
                }
                error = Some(message);
            }
        }
        self.ssh_resumes.clear_finished_closures();
        self.set_ssh_resume_error(&key, error);
        self.refresh_ssh_resume_snapshots();
    }

    pub(super) fn ssh_resume_pane_closed(&mut self, pane_id: &str) {
        let Ok(pane_id) = uuid::Uuid::parse_str(pane_id) else {
            return;
        };
        let pane_id = pane_id.to_string();
        if self
            .ssh_resumes
            .bindings
            .values()
            .any(|binding| matches!(binding.phase, ResumePhase::Launching))
        {
            self.ssh_resumes
                .closed_while_launching
                .insert(pane_id.clone());
        }
        let mut changed = false;
        for binding in self.ssh_resumes.bindings.values_mut() {
            if binding.session.pane_session_id.as_deref() == Some(&pane_id) {
                binding.phase = ResumePhase::Ended;
                binding.session.status = AgentStatus::Ended;
                binding.session.pane_session_id = None;
                changed = true;
            }
        }
        if changed {
            self.refresh_ssh_resume_snapshots();
        }
    }

    pub(super) fn dispatch_ssh_session_focus(&mut self, session: &AgentSession, pane_id: &str) {
        let Some(key) = SshSessionKey::from_session(session) else {
            return;
        };
        let result = (|| -> Result<_> {
            anyhow::ensure!(
                self.current_tab()
                    .agents_view
                    .ssh_source
                    .as_ref()
                    .is_some_and(|source| key.matches_source(source)),
                "SSH session does not belong to the selected source."
            );
            let binding = self
                .ssh_resumes
                .bindings
                .get(&key)
                .ok_or_else(|| anyhow::anyhow!("SSH session has no local pane binding."))?;
            anyhow::ensure!(
                matches!(binding.phase, ResumePhase::Bound)
                    && binding.session.pane_session_id.as_deref() == Some(pane_id),
                "SSH session pane binding is no longer current."
            );
            let sender = self.event_tx.clone().ok_or_else(|| {
                anyhow::anyhow!("SSH session focus requires the helper event loop.")
            })?;
            Ok((
                binding.operation_id,
                sender,
                tokio::runtime::Handle::try_current()?,
            ))
        })();
        let (operation_id, sender, runtime) = match result {
            Ok(values) => values,
            Err(error) => {
                self.set_ssh_resume_error(&key, Some(format!("{error:#}")));
                return;
            }
        };
        #[cfg(test)]
        {
            self.last_dispatched_command = Some(DispatchedCommand {
                kind: DispatchedCommandKind::FocusPane,
                session_id: Some(session.key.clone()),
                argv: vec![
                    "focus-pane".to_string(),
                    "-t".to_string(),
                    pane_id.to_string(),
                ],
            });
        }
        self.set_ssh_resume_error(&key, None);
        let shell = self.shell_mgr.clone();
        let pane_id = pane_id.to_string();
        runtime.spawn(async move {
            if let Err(error) = shell.wt_focus_pane(&pane_id).await {
                let _ = sender.send(AppEvent::SshSessionResumeCompleted {
                    key,
                    operation_id,
                    outcome: SshResumeOutcome::FocusFailed {
                        pane_id,
                        error: format!("{error:#}"),
                    },
                });
            }
        });
    }

    fn set_ssh_resume_error(&mut self, key: &SshSessionKey, error: Option<String>) {
        if let Some(error) = &error {
            tracing::warn!(target: "ssh_sessions", %error, "SSH session pane operation failed");
        }
        for tab in self.tab_sessions.values_mut() {
            if tab
                .agents_view
                .ssh_source
                .as_ref()
                .is_some_and(|source| key.matches_source(source))
            {
                tab.agents_view.ssh_error.clone_from(&error);
            }
        }
    }

    pub(super) fn refresh_ssh_resume_snapshots(&mut self) {
        let mut selections = Vec::new();
        for (tab_id, tab) in &mut self.tab_sessions {
            let Some(source) = &tab.agents_view.ssh_source else {
                continue;
            };
            let Some(snapshot) = &mut tab.agents_view.snapshot else {
                continue;
            };
            let mut rows: Vec<_> = snapshot.iter().map(session_info_to_agent_session).collect();
            self.ssh_resumes.merge_rows(&mut rows, source);
            *snapshot = rows
                .iter()
                .map(crate::session_registry::agent_session_to_session_info)
                .collect();
            selections.push((
                tab_id.clone(),
                tab.agents_list_state.selected().unwrap_or(0),
            ));
        }
        for (tab_id, selected) in selections {
            self.restore_agents_selection(&tab_id, selected);
        }
    }
}

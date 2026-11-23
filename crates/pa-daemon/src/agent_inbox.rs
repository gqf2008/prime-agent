//! The daemon's agent-inbox seams: the settings gate (`agentInbox.*`), the
//! engine's per-session store slot the worker's notice sink and reminder
//! read, and the periodic unread-reminder task. The store itself, its
//! journal, and the `rlm.inbox.*` host requests live in pa-core
//! (`session_engine::inbox`).

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::Notify;

use pa_core::session_engine::inbox::SessionInbox;

use crate::agent_engine::AgentSessionEngine;
use crate::journal::WorkerRecoveryJournal;
use crate::worker::SessionCore;

/// The poll period while the reminder is disabled (`reminderIntervalMs`
/// of `0`): a later settings flip still arms it.
const INBOX_REMINDER_DISABLED_POLL_MS: u64 = 60_000;

impl AgentSessionEngine {
    /// The current session's inbox store, set at every session build.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn set_inbox(&self, inbox: Arc<SessionInbox>) {
        *self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(inbox);
    }

    /// Drop the current session's inbox store (the retire seam: a
    /// retired session's store never serves a replacement's rebuild).
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn clear_inbox(&self) {
        *self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Install the worker's post-drain seam for the inbox (the wake-row
    /// refresh): every session build copies it onto the freshly opened
    /// store, and the kernel's read handler runs it after each drain.
    ///
    /// # Panics
    ///
    /// Panics when the read-hook slot lock is poisoned.
    pub(crate) fn set_inbox_read_hook(&self, hook: pa_core::session_engine::inbox::InboxReadHook) {
        *self
            .inbox_read_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }

    /// The installed post-drain seam, when the worker set one.
    ///
    /// # Panics
    ///
    /// Panics when the read-hook slot lock is poisoned.
    pub(crate) fn inbox_read_hook(&self) -> Option<pa_core::session_engine::inbox::InboxReadHook> {
        self.inbox_read_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The current session's inbox store, when a session is built.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn inbox(&self) -> Option<Arc<SessionInbox>> {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the given store is still the session's published inbox: the
    /// reminder cycle (and any read-and-queue admission) verifies its
    /// captured store under the slot's own mutex, so a replacement that
    /// installed the next session's store never admits over the retired
    /// one's count.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn is_live_inbox(&self, inbox: &Arc<SessionInbox>) -> bool {
        self.inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
    }

    /// The identity-checked durable push: the slot identity is re-checked
    /// and the notice appended under the SLOT's own mutex (the same lock the
    /// retire's [`Self::clear_inbox`] takes), so a concurrent retire — or a
    /// `create` replacement that installs the next session's store — either
    /// waits for this append (the replacement's build can only open the
    /// journal after the clear, with the record already on disk) or lands
    /// first and this call answers `None` (the sender retries into the
    /// replacement store). A retired store never takes an append that could
    /// race the next build's reopen and compaction of the same journal, and
    /// a delivery can never succeed against a store the replacement cannot
    /// read.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn push_to_live_inbox(
        &self,
        inbox: &Arc<SessionInbox>,
        notice: pa_core::session_engine::inbox::InboxNotice,
        at_ms: u64,
    ) -> Option<pa_core::session_engine::inbox::InboxMessage> {
        let slot = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
        {
            return None;
        }
        Some(inbox.push(notice, at_ms))
    }

    /// The identity-checked withdrawal of a heartbeat job's waiting
    /// fires, under the slot's mutex like [`Self::push_to_live_inbox`]:
    /// a retired store never journals a withdrawal that could race the
    /// replacement's reopen of the same journal. `None` when the store
    /// is no longer the session's published inbox.
    ///
    /// # Panics
    ///
    /// Panics when the inbox slot lock is poisoned.
    pub(crate) fn withdraw_heartbeat_from_live_inbox(
        &self,
        inbox: &Arc<SessionInbox>,
        job_id: &str,
    ) -> Option<Vec<pa_core::session_engine::inbox::InboxMessage>> {
        let slot = self
            .inbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, inbox))
        {
            return None;
        }
        Some(inbox.withdraw_heartbeat(job_id))
    }
}

/// Route one informational MCP notification frame into the session's live
/// inbox: push only — no steer, no wake. Returns false when the inbox is
/// disabled, the session has none, or the store was retired, so the caller
/// keeps its transcript-row behaviour.
pub(crate) fn push_mcp_notice(
    engine: &crate::agent_engine::AgentSessionEngine,
    agent_dir: &Path,
    message: &pa_types::session::CustomMessage,
) -> bool {
    if !inbox_enabled(&engine.cwd(), agent_dir) {
        return false;
    }
    let Some(inbox) = engine.inbox() else {
        return false;
    };
    let notice = pa_core::session_engine::bash_notices::KernelNotice {
        row: message.clone(),
        preview_label: "mcp",
    };
    let Some(fields) = pa_core::session_engine::inbox::inbox_notice_fields(&notice) else {
        return false;
    };
    engine
        .push_to_live_inbox(&inbox, fields, pa_core::autonomous::now_millis())
        .is_some()
}

/// `agentInbox.enabled` (default on): the kill switch that restores
/// direct notice-row steering.
pub(crate) fn inbox_enabled(cwd: &Path, agent_dir: &Path) -> bool {
    pa_core::settings::SettingsManager::create(cwd, agent_dir).get_agent_inbox_enabled()
}

/// `agentInbox.reminderIntervalMs` (default 10 minutes, `0` disables).
pub(crate) fn reminder_interval_ms(cwd: &Path, agent_dir: &Path) -> u64 {
    pa_core::settings::SettingsManager::create(cwd, agent_dir)
        .get_agent_inbox_reminder_interval_ms()
}

/// The periodic unread reminder: while the session's inbox stays
/// non-empty, one steering row repeats the waiting count every
/// `reminderIntervalMs`; the interval re-reads each cycle — including
/// after the wait, so a flip to `0` never admits — and an empty inbox,
/// a `0` interval, a closed session, or `agentInbox.enabled: false`
/// skips the cycle (the period resets when it drains; a closed session
/// keeps polling so a later session on the worker re-arms the loop).
pub(crate) fn spawn_inbox_reminder(
    engine: Weak<AgentSessionEngine>,
    core: &Arc<Mutex<SessionCore>>,
    work_notify: &Arc<Notify>,
    recovery: &Arc<Mutex<Option<WorkerRecoveryJournal>>>,
    agent_dir: &std::path::Path,
) {
    let core = Arc::clone(core);
    let work_notify = Arc::clone(work_notify);
    let recovery = Arc::clone(recovery);
    let agent_dir = agent_dir.to_path_buf();
    tokio::spawn(async move {
        loop {
            let interval_ms = match engine.upgrade() {
                Some(engine) => reminder_interval_ms(&engine.cwd(), &agent_dir),
                None => return,
            };
            let wait_ms = if interval_ms == 0 {
                INBOX_REMINDER_DISABLED_POLL_MS
            } else {
                interval_ms
            };
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            // `0` disables the reminder: this cycle admits nothing; the
            // poll re-reads the interval next cycle, so a later settings
            // flip re-arms it.
            if interval_ms == 0 {
                continue;
            }
            let Some(engine) = engine.upgrade() else {
                return;
            };
            // The interval re-reads after the wait too: a flip to `0`
            // while this cycle slept never admits (the sample above only
            // chose the wait).
            if reminder_interval_ms(&engine.cwd(), &agent_dir) == 0 {
                continue;
            }
            // A closed session skips the cycle instead of ending the
            // loop: the worker outlives its sessions, so a later session
            // on the same worker must re-arm the reminder.
            if engine.session_is_closed() {
                continue;
            }
            let Some(inbox) = engine.inbox() else {
                continue;
            };
            // The kill switch re-reads each cycle: `agentInbox.enabled:
            // false` never wakes the agent (the notice sink applies the
            // same setting per notice).
            if !inbox_enabled(&engine.cwd(), &agent_dir) {
                continue;
            }
            let closed = {
                let engine = std::sync::Arc::clone(&engine);
                move || engine.session_is_closed()
            };
            crate::worker::admit_inbox_reminder(
                &recovery,
                &core,
                &work_notify,
                &inbox,
                closed,
                Some(engine.as_ref()),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn engine_with_settings(
        dir: &std::path::Path,
        settings: &serde_json::Value,
    ) -> Arc<crate::agent_engine::AgentSessionEngine> {
        let agent_dir = dir.join("agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
        Arc::new(
            crate::agent_engine::AgentSessionEngine::new(crate::agent_engine::AgentEngineConfig {
                cwd: dir.to_path_buf(),
                agent_dir,
                provider: None,
                model: None,
                api_key: None,
                thinking: None,
                session_dir: None,
                session_file: None,
                faux_script: None,
                supervisor_link: None,
                telemetry_disabled: None,
                cron_store: None,
                queued_steering_probe: None,
            })
            .unwrap(),
        )
    }

    fn mcp_row() -> pa_types::session::CustomMessage {
        pa_types::session::CustomMessage {
            custom_type: pa_core::session_engine::mcp_notices::MCP_NOTIFICATION_CUSTOM_TYPE
                .to_string(),
            content: pa_types::ai::UserContent::Text("[notion] logging: warn: slow".to_string()),
            display: true,
            details: Some(json!([{
                "server": "notion", "kind": "logging", "summary": "warn: slow"
            }])),
            timestamp: 0,
            rest: pa_types::JsonMap::default(),
        }
    }

    fn live_inbox(
        engine: &crate::agent_engine::AgentSessionEngine,
    ) -> Arc<pa_core::session_engine::inbox::SessionInbox> {
        let inbox = Arc::new(pa_core::session_engine::inbox::SessionInbox::in_memory());
        engine.set_inbox(Arc::clone(&inbox));
        inbox
    }

    /// An informational MCP notice lands in the live inbox as `mcp-notice`
    /// and the caller is told not to write a transcript row.
    #[test]
    fn an_informational_mcp_notice_lands_in_the_inbox() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = engine_with_settings(dir.path(), &json!({}));
        let inbox = live_inbox(&engine);
        let agent_dir = engine.config.agent_dir.clone();
        assert!(push_mcp_notice(&engine, &agent_dir, &mcp_row()));
        let (count, _, classes) = inbox.count();
        assert_eq!(count, 1);
        assert_eq!(
            classes.get(pa_types::inbox_notice::INBOX_CLASS_MCP_NOTICE),
            Some(&1)
        );
    }

    /// No live inbox (a retired store, or a session that never built one):
    /// the caller keeps its transcript row.
    #[test]
    fn without_a_live_inbox_the_row_is_kept() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = engine_with_settings(dir.path(), &json!({}));
        let agent_dir = engine.config.agent_dir.clone();
        assert!(!push_mcp_notice(&engine, &agent_dir, &mcp_row()));
    }

    /// `agentInbox.enabled: false` is the kill switch back to direct rows.
    #[test]
    fn the_kill_switch_keeps_the_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine =
            engine_with_settings(dir.path(), &json!({ "agentInbox": { "enabled": false } }));
        let inbox = live_inbox(&engine);
        let agent_dir = engine.config.agent_dir.clone();
        assert!(!push_mcp_notice(&engine, &agent_dir, &mcp_row()));
        assert_eq!(inbox.count().0, 0, "nothing enters a disabled inbox");
    }
}

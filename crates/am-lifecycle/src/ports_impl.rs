//! Implementations of lifecycle-owned storage ports for SQLite types.

use crate::events::ports::{HandoffConnRepo, HandoffRepo, MessageTxOps, TurnConnOps, TurnFenceOps};
use crate::{db, handoff, lifecycle};
use anyhow::Result;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

impl TurnConnOps for SqliteConnection {
    async fn set_status_on(&mut self, turn_id: &str, from: &str, to: &str, why: &str) -> Result<crate::lifecycle::turn_controller::Outcome> {
        lifecycle::turn_controller::set_status_on(self, turn_id, from, to, why).await
    }

    async fn complete_with_native_evidence(
        &mut self,
        turn_id: &str,
        admitted: &lifecycle::fence::Admitted,
        ev: lifecycle::turn_controller::NativeEvidence<'_>,
    ) -> Result<lifecycle::turn_controller::Outcome> {
        lifecycle::turn_controller::complete_with_native_evidence(self, turn_id, admitted, ev).await
    }

    async fn fail_with_native_evidence(
        &mut self,
        turn_id: &str,
        admitted: &lifecycle::fence::Admitted,
        ev: lifecycle::turn_controller::NativeEvidence<'_>,
    ) -> Result<lifecycle::turn_controller::Outcome> {
        lifecycle::turn_controller::fail_with_native_evidence(self, turn_id, admitted, ev).await
    }
}

impl MessageTxOps for Transaction<'_, Sqlite> {
    async fn insert_message_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
    ) -> Result<db::Message> {
        lifecycle::insert_message_tx(self, conversation_id, turn_id, role, content, source, incomplete, snapshot).await
    }

    async fn insert_message_relayed_tx(
        &mut self,
        conversation_id: &str,
        turn_id: Option<&str>,
        role: &str,
        content: &str,
        source: &str,
        incomplete: bool,
        snapshot: Option<&str>,
        relay_from: Option<&str>,
    ) -> Result<db::Message> {
        lifecycle::insert_message_relayed_tx(self, conversation_id, turn_id, role, content, source, incomplete, snapshot, relay_from).await
    }
}

impl TurnFenceOps for SqlitePool {
    async fn classify_event_owner(
        &self,
        bot_id: &str,
        run: &db::Run,
        ev: lifecycle::fence::EventIdentity<'_>,
    ) -> lifecycle::fence::Ownership {
        lifecycle::fence::classify(self, bot_id, run, ev).await
    }
}

impl HandoffRepo for SqlitePool {
    async fn bot_handed_off_to(&self, bot_id: &str) -> Result<Option<String>> {
        handoff::bot_handed_off_to(self, bot_id).await
    }

    async fn handoff_footprint(&self, host: &str) -> Result<handoff::Footprint> {
        handoff::footprint(self, host).await
    }
}

impl HandoffConnRepo for SqliteConnection {
    async fn bot_handed_off_to_on(&mut self, bot_id: &str) -> Result<Option<String>> {
        handoff::bot_handed_off_to_on(self, bot_id).await
    }
}

impl crate::lifecycle::send_now::ports::AttachTxPort for Transaction<'_, Sqlite> {
    async fn bind_attachments_tx(&mut self, message_id: &str, items: &[crate::attach::Attachment]) -> Result<()> {
        crate::attach::bind_tx(self, message_id, items).await
    }
}

impl crate::lifecycle::send_now::ports::AttachConnPort for SqliteConnection {
    async fn unbind_attachment_message(&mut self, msg_id: &str, turn_id: &str) -> Result<()> {
        crate::attach::unbind_message(self, msg_id, turn_id).await
    }
}

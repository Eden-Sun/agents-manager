//! Composition adapters for lifecycle's final `App` boundary.

use crate::{app_ports_p4state, lifecycle::s6_ports, state::App};
use crate::capabilities::Db;
use std::future::Future;

impl s6_ports::TurnEventHostServices for App {
    fn child_done_after_completed_turn(&self, turn_id: &str) {
        crate::runners::child_done::on_completed_turn(&self.shared(), turn_id);
    }

    fn publish_lifecycle_turn(&self, bot_id: &str, turn_id: &str, status: &str, delivery: &str) {
        self.publish_turn(crate::state::TurnEvent {
            bot_id: bot_id.to_owned(),
            turn_id: turn_id.to_owned(),
            status: status.to_owned(),
            delivery: delivery.to_owned(),
        });
    }
}

impl s6_ports::InterruptGraceHostServices for App {
    fn codex_interrupted_after<'a>(
        &'a self,
        bot: &'a crate::db::Bot,
        run: &'a crate::db::Run,
        sent: &'a [String],
    ) -> impl Future<Output = bool> + Send + 'a {
        async move { app_ports_p4state::codex_interrupted_after(&self.shared(), bot, run, sent).await }
    }
}

impl s6_ports::RestartHoldHostServices for App {
    fn open_restart_intents(&self) -> impl Future<Output = anyhow::Result<Vec<s6_ports::OpenRestartIntent>>> + Send {
        async move {
            Ok(app_ports_p4state::open_restart_intents(self.db())
                .await?
                .into_iter()
                .map(|intent| s6_ports::OpenRestartIntent {
                    id: intent.id,
                    kind: intent.kind,
                    subject_id: intent.subject_id,
                })
                .collect())
        }
    }
}

//! Principal-scoped view of `/api/state` for Bot callers.

use crate::events::ports::{ApiPort};
use crate::{lifecycle::LcError, state::App};
use serde_json::Value;
use std::{collections::HashSet, sync::Arc};

/// Keep `/api/state` useful to a worker without returning the browser's global snapshot.
/// Resource visibility follows the same self-and-descendants rule as `authorize_bot_path`.
pub async fn view_for_bot(app: &Arc<App>, caller: &str) -> Result<Value, LcError> {
    let visible_bots: HashSet<String> = sqlx::query_scalar(
        "WITH RECURSIVE owned(id) AS (
             SELECT id FROM bots WHERE id = ?
             UNION
             SELECT child.id FROM bots child JOIN owned parent ON child.parent_bot_id = parent.id
         )
         SELECT id FROM owned",
    )
    .bind(caller)
    .fetch_all(&app.db)
    .await
    .map_err(|e| LcError::Upstream(e.to_string()))?
    .into_iter()
    .collect();

    let mut state = app.state_json().await?;
    let Some(object) = state.as_object_mut() else {
        return Err(LcError::Upstream("state snapshot is not an object".into()));
    };
    for key in ["restart_batch", "cli_updates", "herdr_updates", "deploy_wait", "default_connected", "herdr_session", "hosts", "identities"] {
        object.remove(key);
    }

    let Some(projects) = object.get_mut("projects").and_then(Value::as_array_mut) else {
        return Err(LcError::Upstream("state snapshot has no projects".into()));
    };
    let mut scoped_projects = Vec::new();
    for mut project in std::mem::take(projects) {
        let Some(bots) = project.get_mut("bots").and_then(Value::as_array_mut) else {
            continue;
        };
        bots.retain_mut(|bot| {
            let visible = bot.get("id").and_then(Value::as_str).is_some_and(|id| visible_bots.contains(id));
            if visible {
                if let Some(fields) = bot.as_object_mut() {
                    let parent_is_visible = fields
                        .get("parent_bot_id")
                        .and_then(Value::as_str)
                        .is_some_and(|parent| visible_bots.contains(parent));
                    if !parent_is_visible {
                        fields.remove("parent_bot_id");
                    }
                    for key in ["persona", "args", "identity", "env", "herdr_session", "unread", "read_mark"] {
                        fields.remove(key);
                    }
                }
                true
            } else {
                false
            }
        });
        if bots.is_empty() {
            continue;
        }
        if let Some(fields) = project.as_object_mut() {
            fields.remove("group_unread");
            fields.remove("group_read_mark");
        }
        scoped_projects.push(project);
    }
    *projects = scoped_projects;
    Ok(state)
}

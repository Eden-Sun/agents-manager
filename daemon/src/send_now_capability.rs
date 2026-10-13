//! Project the prompt admission gate into both state snapshots and bot_status frames.
use serde_json::Value;

pub(crate) fn annotate(value: &mut Value, kind: &str, run: Option<&crate::db::Run>, codex_on: bool) {
    let (Some(object), Some(run)) = (value.as_object_mut(), run) else { return };
    let codex_running = crate::codex_update::running_version_of(&run.id);
    let supported = crate::lifecycle::send_now::gate(kind, run.status_json.as_deref(), codex_running.as_deref(), codex_on).is_ok();
    object.insert("send_now_supported".into(), Value::Bool(supported));
}

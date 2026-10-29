mod bash;
pub(super) mod editor;
use super::protocol::ToolCall;
pub use bash::Limits;
use futures::future::BoxFuture;
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub struct LocalTools {
    editor: editor::Editor,
    limits: Limits,
}
impl LocalTools {
    pub fn new(worktree: &Path, limits: Limits) -> Result<Self, String> {
        limits.validate()?;
        Ok(Self {
            editor: editor::Editor::new(worktree)?,
            limits,
        })
    }
}
impl super::runner::Tools for LocalTools {
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            match call.name.as_str() {
                "bash" => {
                    if call
                        .input
                        .get("restart")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    {
                        return Err("persistent shell restart is unsupported".into());
                    }
                    let command = call
                        .input
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .ok_or("missing command")?;
                    bash::execute(&self.editor, command, self.limits, cancel).await
                }
                "str_replace_based_edit_tool" => self.editor.execute(&call.input),
                _ => Err("unsupported tool".into()),
            }
        })
    }
}

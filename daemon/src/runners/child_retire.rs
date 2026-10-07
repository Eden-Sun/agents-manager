use std::future::Future;
use std::sync::Arc;
use crate::child_retire::{Mode, Outcome};
use crate::state::App;

#[track_caller]
pub(crate) fn retire<'a>(
    app: &'a Arc<App>,
    bot_id: &'a str,
    why: &'static str,
    mode: Mode,
) -> impl Future<Output = anyhow::Result<Outcome>> + 'a {
    crate::child_retire::retire(app.as_ref(), bot_id, why, mode)
}

use std::future::Future;
use std::sync::Arc;
use std::panic::Location;
use crate::child_retire::{Mode, Outcome};
use crate::state::App;

pub(crate) fn retire_at<'a>(
    app: &'a Arc<App>,
    bot_id: &'a str,
    why: &'static str,
    mode: Mode,
    at: &'static Location<'static>,
) -> impl Future<Output = anyhow::Result<Outcome>> + 'a {
    crate::child_retire::retire_at(app.as_ref(), bot_id, why, mode, at)
}

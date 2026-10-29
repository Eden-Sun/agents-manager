//! Private evidence retained only after the completion owner has truly exited.
use super::{adapter::AdapterIdentity, completion::{self, Probe}, receipt::Payload};
use crate::state::App;
use std::{future::Future, sync::Arc};

pub(super) struct Finalized {
    pub(super) identity: AdapterIdentity,
    pub(super) revision: i64,
    pub(super) payload: Payload,
}

pub(super) fn verify<'a>(app: &'a Arc<App>, proof: &'a Finalized) -> impl Future<Output = bool> + Send + 'a {
    Box::pin(async move {
        matches!(completion::probe_after_join(app, &proof.identity, &proof.payload).await,
            Ok(Probe::Applied(committed)) if committed.session_revision == proof.revision)
    })
}

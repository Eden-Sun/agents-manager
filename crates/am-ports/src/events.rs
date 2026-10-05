use am_core::{EventEnvelope, EventSeq, PortError, TurnEvent};
use std::future::Future;

pub trait EventSink: Send + Sync {
    fn emit<'a>(
        &'a self,
        event: EventEnvelope,
    ) -> impl Future<Output = Result<EventSeq, PortError>> + Send + 'a;
}

pub trait TurnEvents: Send + Sync {
    fn publish<'a>(
        &'a self,
        event: TurnEvent,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;
}

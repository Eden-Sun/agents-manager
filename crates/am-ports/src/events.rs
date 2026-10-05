use am_core::{EventEnvelope, EventSeq, PortError, TurnEvent, TurnId};
use std::future::Future;

pub trait EventSink: Send + Sync {
    fn emit<'a>(
        &'a self,
        event: EventEnvelope,
    ) -> impl Future<Output = Result<EventSeq, PortError>> + Send + 'a;

    fn bot_status_changed<'a>(
        &'a self,
        bot: &'a str,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;
}

pub trait TurnEvents: Send + Sync {
    fn publish<'a>(
        &'a self,
        event: TurnEvent,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;

    fn turn_changed<'a>(
        &'a self,
        turn: &'a TurnId,
    ) -> impl Future<Output = Result<(), PortError>> + Send + 'a;
}

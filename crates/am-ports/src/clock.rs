use am_core::UnixMillis;

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> UnixMillis;
}

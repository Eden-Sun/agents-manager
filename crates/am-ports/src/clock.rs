use am_core::UnixMillis;

pub trait Clock: Send + Sync {
    fn now_unix_ms(&self) -> UnixMillis;
}

pub trait IdSource: Send + Sync {
    fn new_id(&self) -> String;
}

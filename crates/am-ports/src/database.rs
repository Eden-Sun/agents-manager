/// A deliberately transparent resource context. It carries only the pool/handle selected by the
/// composition root; it never carries App or unrelated runtime services.
#[derive(Debug, Clone)]
pub struct DbContext<P> {
    pool: P,
}

impl<P> DbContext<P> {
    pub fn new(pool: P) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &P {
        &self.pool
    }
}

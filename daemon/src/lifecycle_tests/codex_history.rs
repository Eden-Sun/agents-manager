#![allow(unused_imports)]
//! #1862 L2-1：`HistoryHook::default()` 在 daemon 測試建置是空的，這條只能留在 daemon。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::codex_history::*;
    use std::sync::Arc;

    #[test]
    fn the_hook_defaults_off_in_tests_and_swaps() {
        let hook = HistoryHook::default();
        assert!(hook.get().is_none());
        hook.set(Some(Arc::new(StubSource::new())));
        assert!(hook.get().is_some());
        hook.set(None);
        assert!(hook.get().is_none());
    }
}

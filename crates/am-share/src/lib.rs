//! 分享 bot（`share`）：從 agents-managerd 抽出來的獨立 crate，只靠 am-base／am-lifecycle 與窄 trait，
//! `App` 的實作留在 daemon（`app_ports_p10`）。

// Preserve the former crate-root paths for the stable lower-level modules.
pub use am_base::*;
pub use am_core;
pub use am_lifecycle::lifecycle;
pub use am_ports;

pub mod share;

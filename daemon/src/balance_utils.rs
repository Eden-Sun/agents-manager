use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptSpent {
    pub created_at: String,
    pub microusd: i64,
    /// true＝不是確定的花費：結果不明（`unknown`）又沒有估價的 attempt，金額是**預留上限**（最壞情況，實際多半低很多）。
    pub uncertain: bool,
}

pub fn microusd_to_usd(microusd: i64) -> f64 {
    microusd as f64 / 1_000_000.0
}

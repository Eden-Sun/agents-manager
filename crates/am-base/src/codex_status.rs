//! 純 codex 狀態列額度資料與解析。

/// codex status line 上的額度剩餘量；CLI 當下的數字，比輪詢結果更新。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodexStatusQuota {
    pub five_hour_left: Option<f64>,
    pub weekly_left: Option<f64>,
}

impl CodexStatusQuota {
    pub fn is_empty(&self) -> bool {
        self.five_hour_left.is_none() && self.weekly_left.is_none()
    }
}

/// 窄 pane 會把行尾截成 `weekly 48% …`，所以 `left` 不是必要的字。
pub fn parse_status_quota(screen: &str) -> Option<CodexStatusQuota> {
    let mut out = None;
    for raw in screen.lines() {
        let line = raw.trim();
        // fork／resume 起來的 codex 狀態列可能沒有 `Context` 那一段；`% left` 也算。
        if !line.contains('·') || !(line.contains("Context") || line.contains("% left")) {
            continue;
        }
        let q = CodexStatusQuota { five_hour_left: pct_after(line, "5h"), weekly_left: pct_after(line, "weekly") };
        if !q.is_empty() {
            // 最後一個相符的才是現在那行（開頭的 banner 有同樣形狀）。
            out = Some(q);
        }
    }
    out
}

fn pct_after(line: &str, label: &str) -> Option<f64> {
    let mut words = line.split_whitespace().peekable();
    while let Some(w) = words.next() {
        if !w.eq_ignore_ascii_case(label) {
            continue;
        }
        // 只留開頭數字：截斷時省略號直接黏在 `%` 後。
        let raw = *words.peek()?;
        let n: String = raw.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(v) = n.parse::<f64>() {
            if (0.0..=100.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

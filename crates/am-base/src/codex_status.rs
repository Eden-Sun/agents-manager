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

/// 截點可能在任何字元；`%` 之後被截才讀得到，所以 `left` 不是必要的字。
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
        // 截點可能在任何字元；`%` 之後被截才讀得到。
        let raw = *words.peek()?;
        let n: String = raw.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        // 數字後面緊跟 `%` 才是完整讀數：`weekly 4…` 是 48% 被截在數字中間，不是剩 4%。
        if !raw[n.len()..].starts_with('%') {
            continue;
        }
        if let Ok(v) = n.parse::<f64>() {
            if (0.0..=100.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_number_cut_before_its_percent_sign_is_not_a_reading() {
        assert_eq!(
            parse_status_quota("gpt-6-astra high · /tmp · Context 28% used · 5h 90% left · weekly 4…"),
            Some(CodexStatusQuota { five_hour_left: Some(90.0), weekly_left: None })
        );
        assert_eq!(
            parse_status_quota("gpt-6-astra high · /tmp · Context 28% used · 5h 9…"),
            None
        );
        let q = parse_status_quota("gpt-6-astra high · /tmp · Context 28% used · 5h 100% left · weekly 10…").unwrap();
        assert_eq!(q.weekly_left, None);
        assert_eq!(q.five_hour_left, Some(100.0));

        assert_eq!(
            parse_status_quota("m · /tmp · Context 1% used · 5h 7.…"),
            None
        );

        // 不回歸
        let q = parse_status_quota("gpt-6-astra high · /tmp · Context 28% used · 5h 90% left · weekly 48% …").unwrap();
        assert_eq!((q.five_hour_left, q.weekly_left), (Some(90.0), Some(48.0)));

        let q = parse_status_quota("gpt-6-astra high · /tmp · Context 28% used · 5h 36% left · weekly 24%…").unwrap();
        assert_eq!((q.five_hour_left, q.weekly_left), (Some(36.0), Some(24.0)));

        let q = parse_status_quota("m x · /tmp · Context 1% used · 5h 7.5% left, weekly 12%.").unwrap();
        assert_eq!((q.five_hour_left, q.weekly_left), (Some(7.5), Some(12.0)));
    }
}

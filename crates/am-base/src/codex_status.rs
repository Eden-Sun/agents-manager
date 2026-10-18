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

/// codex 畫面底部的模型列（`<model> <effort> · <cwd> · Context N% used · …`），去掉右側的 warning 段。
#[derive(Debug, Clone, PartialEq)]
pub struct CodexFooter {
    pub line: String,
    pub context_used_pct: Option<f64>,
}

/// 只看畫面最後幾列：更上面同形狀的列是啟動 banner 或回覆裡引用的舊列。
const FOOTER_TAIL_LINES: usize = 8;

pub fn parse_footer(screen: &str) -> Option<CodexFooter> {
    let lines: Vec<&str> = screen.lines().collect();
    let mut out = None;
    for raw in &lines[lines.len().saturating_sub(FOOTER_TAIL_LINES)..] {
        let row = without_warning(raw.trim());
        if is_model_row(row) {
            // 最後一個相符的才是現在那列。
            out = Some(CodexFooter { line: row.to_string(), context_used_pct: context_pct(row) });
        }
    }
    out
}

/// 去掉右側靠右的 `⚠ N warning(s) · f2 to view`（跟前面的內容隔兩格以上空白，#1671）。
fn without_warning(row: &str) -> &str {
    let Some(i) = row.rfind('⚠') else { return row };
    let (head, tail) = row.split_at(i);
    let counted = tail.strip_prefix("⚠ ").and_then(|t| t.split_once(' '));
    let is_warning = counted.is_some_and(|(n, rest)| {
        !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && matches!(rest, "warning · f2 to view" | "warnings · f2 to view")
    });
    if is_warning && head.ends_with("  ") {
        head.trim_end()
    } else {
        row
    }
}

/// `<model> [effort] [fast] · <cwd 或 Context／額度段…>`；模型名以英數字開頭（markdown 項目符號不算）。
fn is_model_row(row: &str) -> bool {
    let Some((head, rest)) = row.split_once('·') else { return false };
    let mut words = head.split_whitespace();
    let Some(model) = words.next() else { return false };
    let model_like = model.starts_with(|c: char| c.is_ascii_alphanumeric())
        && model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    let rest = rest.trim();
    model_like && words.count() <= 2 && (rest.starts_with('/') || rest.starts_with('~') || rest.contains("Context") || rest.contains("% left"))
}

fn context_pct(row: &str) -> Option<f64> {
    let after = row.split("Context ").nth(1)?;
    let n: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    after[n.len()..].starts_with('%').then(|| n.parse().ok()).flatten().filter(|v| (0.0..=100.0).contains(v))
}

/// 截點可能在任何字元；`%` 之後被截才讀得到，所以 `left` 不是必要的字。
pub fn parse_status_quota(screen: &str) -> Option<CodexStatusQuota> {
    let footer = parse_footer(screen)?;
    let q = CodexStatusQuota { five_hour_left: pct_after(&footer.line, "5h"), weekly_left: pct_after(&footer.line, "weekly") };
    (!q.is_empty()).then_some(q)
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

        let q = parse_status_quota("m-x x · /tmp · Context 1% used · 5h 7.5% left, weekly 12%.").unwrap();
        assert_eq!((q.five_hour_left, q.weekly_left), (Some(7.5), Some(12.0)));
    }

    #[test]
    fn a_truncated_current_footer_does_not_fall_back_to_an_older_quota_row() {
        let old = "gpt-6-astra high · /old · Context 2% used · weekly 1% left";
        let intervening = "ordinary output\n".repeat(10);
        let current = "gpt-6-astra medium · /project · Context 28% used · weekly 4…";
        let screen = format!("{old}\n{intervening}{current}");
        assert_eq!(parse_status_quota(&screen), None);
    }

    #[test]
    fn a_current_footer_without_quota_does_not_fall_back_to_an_older_quota_row() {
        let old = "gpt-6-astra high · /old · Context 2% used · weekly 1% left";
        let current = "gpt-6-astra medium · /project · Context 28% used";
        assert_eq!(parse_status_quota(&format!("{old}\n{current}")), None);
    }

    #[test]
    fn a_valid_current_footer_replaces_older_quota_and_keeps_only_complete_windows() {
        let old = "gpt-6-astra high · /old · Context 2% used · weekly 1% left";
        let current = "gpt-6-astra medium · /project · Context 28% used · 5h 90% left · weekly 4…";
        assert_eq!(
            parse_status_quota(&format!("{old}\n{current}")),
            Some(CodexStatusQuota { five_hour_left: Some(90.0), weekly_left: None })
        );

        let current = "gpt-6-astra medium · /project · Context 28% used · 5h 36% left · weekly 24% left";
        assert_eq!(
            parse_status_quota(&format!("{old}\n{current}")),
            Some(CodexStatusQuota { five_hour_left: Some(36.0), weekly_left: Some(24.0) })
        );
    }

    #[test]
    fn a_model_token_without_a_hyphen_is_still_a_footer() {
        let row = "m x · /tmp · Context 1% used · 5h 7.5% left, weekly 12%.";
        assert_eq!(
            parse_footer(row),
            Some(CodexFooter { line: row.into(), context_used_pct: Some(1.0) })
        );
        assert_eq!(parse_status_quota(row), Some(CodexStatusQuota { five_hour_left: Some(7.5), weekly_left: Some(12.0) }));
    }
}

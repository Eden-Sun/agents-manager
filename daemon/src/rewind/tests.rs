//! 對話倒回的測試。
//! - 讀畫面：用 2026-09-23 實機截的 claude 2.1.280 畫面（`claude_2.1.280_rewind_*.txt`，pyte 算出來的可見畫面，同 herdr `pane.read visible`）。
//! - 狀態機：[`FakeTui`] 照實機的樣子畫選單／確認頁、照實機的規則處理按鍵，可以注入各種壞掉的情況；不碰真 herdr、真 claude。

use super::*;
use crate::testing as tt;
use std::sync::Mutex as StdMutex;

const IDLE: &str = include_str!("claude_2.1.280_rewind_idle.txt");
const MENU_CURRENT: &str = include_str!("claude_2.1.280_rewind_menu_current.txt");
const MENU_MULTI: &str = include_str!("claude_2.1.280_rewind_menu_selected_multiline.txt");
const MENU_LONG: &str = include_str!("claude_2.1.280_rewind_menu_selected_long_24rows.txt");
const CONFIRM_SHORT: &str = include_str!("claude_2.1.280_rewind_confirm_short_multiline.txt");
const CONFIRM_4_LINES: &str = include_str!("claude_2.1.280_rewind_confirm_truncated_4_lines_24rows.txt");
const CONFIRM_LONG: &str = include_str!("claude_2.1.280_rewind_confirm_truncated_long_24rows.txt");
const REFILLED: &str = include_str!("claude_2.1.280_rewind_restored_composer_refilled.txt");
const CLEARED: &str = include_str!("claude_2.1.280_rewind_restored_composer_cleared.txt");
const CONFIRM_14_ROWS: &str = include_str!("claude_2.1.280_rewind_confirm_options_cut_off_14rows.txt");
const REFILLED_TAIL: &str = include_str!("claude_2.1.280_rewind_restored_composer_tail_14rows.txt");
/// 7178806b 的實機畫面（帶樣式）：輸入列只有 dim 的「建議下一句」`Initialize git`。
const SUGGESTION: &str = include_str!("../lifecycle/fixtures/claude-2.1.280-prompt-suggestion.ansi");

const SECOND: &str = "Second prompt, line one.\nLine two mentions BANANA.\nReply with just OK.";
const THIRD: &str = "Third prompt is deliberately long so that the rewind menu has to truncate it: it talks about cherries, dates, elderberries, figs, grapes, honeydew melons, kiwis, lemons, mangoes, nectarines, oranges, papayas and quinces, and then finally asks you to reply with just OK.";

fn long_words() -> String {
    (0..220).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ") + " reply OK"
}

fn thirty_lines() -> String {
    (0..30).map(|i| format!("multi line {i} of the thirty line prompt")).collect::<Vec<_>>().join("\n") + "\nReply with just OK."
}

// ───────────── 讀實機畫面 ─────────────

#[test]
fn the_real_menu_is_read_with_its_cursor() {
    let m = parse_menu(MENU_CURRENT).expect("選單");
    assert_eq!(m.selected, None, "一開始游標在 (current)");
    let m = parse_menu(MENU_MULTI).expect("選單");
    assert_eq!(m.selected.as_deref(), Some("Second prompt, line one.…"));
    assert!(entry_matches(m.selected.as_deref().unwrap(), SECOND), "多行的只顯示第一行加 …");
    let m = parse_menu(MENU_LONG).expect("選單");
    assert!(entry_matches(m.selected.as_deref().unwrap(), &long_words()), "太長的在欄寬截斷加 …");
    assert!(!entry_matches(m.selected.as_deref().unwrap(), THIRD));
}

/// 歷史裡的 `❯ <prompt>` 不是選單；確認頁也不是選單；閒著的畫面兩者都不是。
#[test]
fn history_prompts_and_the_confirm_page_are_not_the_menu() {
    assert!(parse_menu(IDLE).is_none());
    assert!(parse_menu(CONFIRM_SHORT).is_none());
    assert!(parse_confirm(MENU_MULTI).is_none());
    assert!(!in_rewind_ui(IDLE) && !in_rewind_ui(REFILLED) && !in_rewind_ui(CLEARED));
    assert!(in_rewind_ui(MENU_CURRENT) && in_rewind_ui(CONFIRM_SHORT));
}

#[test]
fn the_real_confirm_page_is_read_and_matched() {
    let c = parse_confirm(CONFIRM_SHORT).expect("確認頁");
    assert_eq!(c.quoted, vec!["Second prompt, line one.", "Line two mentions BANANA.", "Reply with just OK."], "不含 (42s ago)");
    assert!(confirm_matches(&c.quoted, SECOND));
    assert!(!confirm_matches(&c.quoted, THIRD));
    assert!(!confirm_matches(&c.quoted, "Second prompt, line one."), "畫面比目標多：不是同一則");
}

/// 確認頁截斷沒有任何記號：多行的只印前 4 行、長的一行只印約 6 個折行。前綴對上而且確實是截斷的長度才算。
#[test]
fn a_truncated_confirm_page_matches_by_prefix_only_when_it_is_long_enough() {
    let c = parse_confirm(CONFIRM_4_LINES).expect("確認頁");
    assert_eq!(c.quoted.len(), 4);
    assert!(confirm_matches(&c.quoted, &thirty_lines()));
    let c = parse_confirm(CONFIRM_LONG).expect("確認頁");
    assert!(confirm_matches(&c.quoted, &long_words()), "折行不算差異");
    // 只有兩行卻只對到前綴：不是被截斷，是另一則。
    let short = vec!["multi line 0 of the thirty line prompt".to_string(), "multi line 1 of the thirty line prompt".to_string()];
    assert!(!confirm_matches(&short, &thirty_lines()));
}

/// 14 列的 pane：選項整組被擠出畫面，只剩說明兩行。一樣認得出是確認頁、讀得到原文（之後按 `1` 選，不靠游標）。
#[test]
fn a_confirm_page_whose_options_are_cut_off_is_still_read() {
    let c = parse_confirm(CONFIRM_14_ROWS).expect("確認頁");
    assert!(confirm_matches(&c.quoted, SECOND));
    assert!(parse_menu(CONFIRM_14_ROWS).is_none(), "不是選單");
    assert!(in_rewind_ui(CONFIRM_14_ROWS));
}

#[test]
fn the_refilled_composer_is_seen_and_cleared() {
    assert!(lifecycle::composer_text("claude", REFILLED).is_some_and(|t| squash(THIRD).contains(&squash(&t))));
    assert_eq!(lifecycle::composer_text("claude", CLEARED), None);
    // 長的只看得到尾巴幾行：是目標的連續一段。
    assert!(lifecycle::composer_text("claude", REFILLED_TAIL).is_some_and(|t| squash(&thirty_lines()).contains(&squash(&t))));
    assert!(!in_rewind_ui(REFILLED_TAIL));
}

/// dim 的建議句不是字：倒回照樣可以開始；同一個位置換成正常樣式（使用者真的打的）就還是有字。
#[test]
fn a_dim_suggestion_is_an_empty_composer_but_typed_text_is_not() {
    assert!(SUGGESTION.contains("\u{1b}[2mInitialize git"), "fixture 形狀變了");
    assert_eq!(lifecycle::composer_text("claude", &screen_text(SUGGESTION)), None, "建議句＝空的輸入列");
    let agm = SUGGESTION.replace("Initialize git", "重建 release 並重啟 daemon");
    assert_eq!(lifecycle::composer_text("claude", &screen_text(&agm)), None);
    let typed = SUGGESTION.replace("\u{1b}[2mInitialize git", "Initialize git");
    assert!(lifecycle::composer_text("claude", &screen_text(&typed)).is_some(), "非 dim＝打的字");
    assert!(!screen_text(SUGGESTION).contains('\u{1b}'), "判讀用的畫面沒有樣式碼");
}

#[test]
fn squash_ignores_whitespace_and_image_placeholders() {
    assert_eq!(squash("a b\n c"), "abc");
    assert_eq!(squash("look [Image #1] here [Image #12]"), "lookhere");
    assert_eq!(squash("[Image #x] stays"), "[Image#x]stays");
}

// ───────────── 假 TUI ─────────────

#[derive(Debug, Clone, PartialEq)]
enum Ui {
    Idle,
    Menu { cursor: usize },
    Confirm { entry: usize, option: usize },
}

#[derive(Default)]
struct Faults {
    /// `/rewind` 沒反應（選單不出來）。
    menu_disabled: bool,
    /// 確認頁印的字換成這個。
    confirm_text: Option<String>,
    /// 確認頁一開始游標在第幾個選項。
    start_option: usize,
    /// 確認頁的選項被擠出畫面（矮 pane）。
    hide_options: bool,
    /// 按了 Restore 畫面卡在確認頁。
    stuck_after_restore: bool,
    /// 輸入列一開始就有字。
    composer: Option<String>,
    /// ctrl+c 清掉輸入列之後，接下來幾次讀畫面都看得到「Press Ctrl-C again to exit」。
    ctrl_c_hint_reads: usize,
    /// 輸入列只畫得下最後幾行（矮 pane 放回長訊息時，實機只看得到尾巴）。
    composer_tail: Option<usize>,
    /// 畫面帶樣式（herdr `format: ansi` 讀到的樣子）：每一列前後有樣式碼，輸入列空著時畫 dim 的這句建議。
    styled_hint: Option<String>,
}

static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct FakeTui {
    entries: StdMutex<Vec<String>>,
    ui: StdMutex<Ui>,
    composer: StdMutex<String>,
    faults: Faults,
    /// 每一次按鍵／打字。
    log: StdMutex<Vec<String>>,
    restored: StdMutex<Option<usize>>,
    hint_left: StdMutex<usize>,
    /// race point 的 key：每個實例唯一（#573）。
    id: String,
}

impl FakeTui {
    fn new(entries: &[&str], faults: Faults) -> Arc<Self> {
        Arc::new(FakeTui {
            entries: StdMutex::new(entries.iter().map(|s| s.to_string()).collect()),
            ui: StdMutex::new(Ui::Idle),
            composer: StdMutex::new(faults.composer.clone().unwrap_or_default()),
            faults,
            log: StdMutex::new(Vec::new()),
            restored: StdMutex::new(None),
            hint_left: StdMutex::new(0),
            id: format!("fake-tui-{}", NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
        })
    }
    fn ui(&self) -> Ui {
        self.ui.lock().unwrap().clone()
    }
    fn restored(&self) -> Option<usize> {
        *self.restored.lock().unwrap()
    }
    fn composer(&self) -> String {
        self.composer.lock().unwrap().clone()
    }
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn close_confirmation(&self) {
        *self.ui.lock().unwrap() = Ui::Idle;
    }

    fn display(e: &str) -> String {
        let first = e.lines().next().unwrap_or("");
        let multi = e.lines().count() > 1;
        if first.chars().count() > 60 {
            format!("{}…", first.chars().take(60).collect::<String>())
        } else if multi {
            format!("{first}…")
        } else {
            first.to_string()
        }
    }

    fn render(&self) -> String {
        let entries = self.entries.lock().unwrap().clone();
        let mut s = String::new();
        for e in &entries {
            for (i, l) in e.lines().enumerate() {
                s.push_str(if i == 0 { "❯ " } else { "  " });
                s.push_str(l);
                s.push('\n');
            }
            s.push_str("\n⏺ OK\n\n");
        }
        match self.ui() {
            Ui::Idle => {
                let c = self.composer();
                s.push_str(&"─".repeat(40));
                s.push('\n');
                if c.is_empty() {
                    s.push_str("❯\n");
                }
                let all: Vec<&str> = c.lines().collect();
                let from = self.faults.composer_tail.map_or(0, |n| all.len().saturating_sub(n));
                for (i, l) in all[from..].iter().enumerate() {
                    s.push_str(if i == 0 { "❯ " } else { "  " });
                    s.push_str(l);
                    s.push('\n');
                }
                s.push_str(&"─".repeat(40));
                let mut hint = self.hint_left.lock().unwrap();
                if *hint > 0 {
                    *hint -= 1;
                    s.push_str("\n  Press Ctrl-C again to exit\n");
                } else {
                    s.push_str("\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n");
                }
            }
            Ui::Menu { cursor } => {
                s.push_str(&"▔".repeat(40));
                s.push_str("\n   Rewind\n\n   Restore the code and/or conversation to the point before…\n\n");
                // 跟實機一樣只看得到游標與它上面一則。
                let top = cursor.saturating_sub(1);
                if top > 0 {
                    s.push_str(&format!("    ↑ {top} more above\n\n"));
                }
                for (i, e) in entries.iter().map(Some).chain(std::iter::once(None)).enumerate().skip(top).take(cursor + 1 - top) {
                    let label = e.map_or_else(|| "(current)".to_string(), |e| Self::display(e));
                    s.push_str(if i == cursor { "   ❯ " } else { "     " });
                    s.push_str(&label);
                    s.push('\n');
                    if e.is_some() {
                        s.push_str("     ⚠ No code restore\n\n");
                    }
                }
                let below = entries.len().saturating_sub(cursor);
                if below > 0 {
                    s.push_str(&format!("    ↓ {below} more below\n"));
                }
                s.push_str("\n   Enter to continue · Esc to cancel\n");
            }
            Ui::Confirm { entry, option } => {
                let text = self.faults.confirm_text.clone().unwrap_or_else(|| entries[entry].clone());
                s.push_str(&"▔".repeat(40));
                s.push_str("\n   Rewind\n\n   Confirm you want to restore the conversation to the point before you sent this message:\n\n");
                for l in text.lines().take(4) {
                    s.push_str(&format!("   │ {l}\n"));
                }
                s.push_str("   │ (42s ago)\n\n   The conversation will be forked.\n   The code will be unchanged.\n\n");
                let shown = if self.faults.hide_options { 0 } else { 4 };
                for (i, o) in ["1. Restore conversation", "2. Summarize from here", "3. Summarize up to here", "4. Never mind"].iter().enumerate().take(shown) {
                    s.push_str(if i == option { "   ❯ " } else { "     " });
                    s.push_str(o);
                    s.push('\n');
                }
            }
        }
        s
    }

    fn key(&self, k: &str) {
        self.log.lock().unwrap().push(k.to_string());
        let len = self.entries.lock().unwrap().len();
        let next = match (self.ui(), k) {
            (Ui::Idle, "Enter") if self.composer() == "/rewind" && !self.faults.menu_disabled => {
                self.composer.lock().unwrap().clear();
                Ui::Menu { cursor: len }
            }
            (Ui::Idle, "ctrl+c") => {
                self.composer.lock().unwrap().clear();
                *self.hint_left.lock().unwrap() = self.faults.ctrl_c_hint_reads;
                Ui::Idle
            }
            (Ui::Menu { cursor }, "Up") => Ui::Menu { cursor: cursor.saturating_sub(1) },
            (Ui::Menu { cursor }, "Down") => Ui::Menu { cursor: (cursor + 1).min(len) },
            (Ui::Menu { cursor }, "Enter") if cursor < len => Ui::Confirm { entry: cursor, option: self.faults.start_option },
            (Ui::Menu { .. }, "Escape") => Ui::Idle,
            (Ui::Confirm { entry, .. }, "Escape") => Ui::Menu { cursor: entry },
            (Ui::Confirm { entry, option }, "Up") => Ui::Confirm { entry, option: option.saturating_sub(1) },
            (Ui::Confirm { entry, option }, "Down") => Ui::Confirm { entry, option: (option + 1).min(3) },
            (Ui::Confirm { entry, option }, "Enter" | "1") if (option == 0 || k == "1") && !self.faults.stuck_after_restore => {
                let text = self.entries.lock().unwrap()[entry].clone();
                self.entries.lock().unwrap().truncate(entry);
                *self.composer.lock().unwrap() = text;
                *self.restored.lock().unwrap() = Some(entry);
                Ui::Idle
            }
            (other, _) => other,
        };
        *self.ui.lock().unwrap() = next;
    }
}

impl Pane for FakeTui {
    fn race_key(&self) -> String {
        self.id.clone()
    }
    fn read(&self) -> BoxFuture<'_, anyhow::Result<String>> {
        Box::pin(async move {
            let plain = self.render();
            let Some(hint) = self.faults.styled_hint.clone() else { return Ok(plain) };
            // 跟實機一樣：空的輸入列是 `❯` NBSP 再接 dim 的建議句；其他列前後包樣式碼。
            Ok(plain
                .lines()
                .map(|l| {
                    if l == "❯" {
                        format!("❯\u{a0}\u{1b}[0m\u{1b}[2m{hint}\u{1b}[0m")
                    } else {
                        format!("\u{1b}[0m\u{1b}[38;2;136;136;136m{l}\u{1b}[0m")
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        })
    }
    fn send_text<'a>(&'a self, text: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("text:{text}"));
            if self.ui() == Ui::Idle {
                self.composer.lock().unwrap().push_str(text);
            }
            Ok(())
        })
    }
    fn send_keys<'a>(&'a self, keys: &'a [&'a str]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            for k in keys {
                self.key(k);
            }
            Ok(())
        })
    }
}

const A: &str = "Remember the word APPLE. Reply with just OK.";
const C: &str = "Now a third one about CHERRY.";

/// 假 TUI 畫出來的樣子要跟實機一樣讀得懂，不然下面的狀態機測試是在測假的。
#[tokio::test]
async fn the_fake_tui_renders_what_the_real_parsers_read() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    assert!(!in_rewind_ui(&tui.render()) && lifecycle::composer_text("claude", &tui.render()).is_none());
    tui.send_text("/rewind").await.unwrap();
    tui.key("Enter");
    assert_eq!(parse_menu(&tui.render()).unwrap().selected, None);
    tui.key("Up");
    tui.key("Up");
    assert!(entry_matches(parse_menu(&tui.render()).unwrap().selected.as_deref().unwrap(), SECOND));
    tui.key("Enter");
    let c = parse_confirm(&tui.render()).unwrap();
    assert!(confirm_matches(&c.quoted, SECOND));
}

// ───────────── 狀態機 ─────────────

/// 正常路徑：打 `/rewind` → 選單 → 往上兩格到 SECOND → 確認頁字對上 → Restore → 清掉 pane 輸入列。
#[tokio::test]
async fn the_normal_path_restores_the_right_message_and_clears_the_pane() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    let done = drive(tui.as_ref(), SECOND, 0).await.unwrap();
    assert!(done.pane_cleared);
    assert_eq!(tui.restored(), Some(1), "倒回到 SECOND 之前");
    assert_eq!(*tui.entries.lock().unwrap(), vec![A.to_string()]);
    assert_eq!(tui.ui(), Ui::Idle);
    assert_eq!(tui.composer(), "", "pane 的輸入列清空（原文交給網頁）");
    assert_eq!(tui.log(), vec!["text:/rewind", "Enter", "Up", "Up", "Enter", "1", "ctrl+c"]);
}

/// 長的訊息放回輸入列時只看得到尾巴幾行：那也是目標的一段，照樣清掉。
#[tokio::test]
async fn a_long_refill_seen_only_by_its_tail_is_still_cleared() {
    let long = thirty_lines();
    let tui = FakeTui::new(&[A, &long, C], Faults { composer_tail: Some(3), ..Default::default() });
    let done = drive(tui.as_ref(), &long, 0).await.unwrap();
    assert!(done.pane_cleared);
    assert_eq!(tui.composer(), "");
}

/// 清掉輸入列之後 claude 會顯示「Press Ctrl-C again to exit」幾秒：等它消失才回（呼叫端還握著 bot 鎖），
/// 這段時間裡別的路徑再送一個 ctrl+c 會把 claude 關掉。
#[tokio::test]
async fn it_waits_for_the_ctrl_c_hint_to_go_away_before_returning() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults { ctrl_c_hint_reads: 5, ..Default::default() });
    drive(tui.as_ref(), SECOND, 0).await.unwrap();
    assert_eq!(*tui.hint_left.lock().unwrap(), 0, "回來的時候提示已經不在了");
    assert!(!tui.render().contains("Press Ctrl-C again"));
}

/// 帶樣式的畫面、輸入列畫著 dim 的建議句（2026-09-24 AGM：上線前的版本讀純文字，會把它當成有字擋掉）：照樣倒得成，
/// 選單、確認頁、倒回後的輸入列都讀得懂。
#[tokio::test]
async fn a_styled_screen_with_a_dim_suggestion_can_still_rewind() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults { styled_hint: Some("重建 release 並重啟 daemon".into()), ..Default::default() });
    let done = drive(tui.as_ref(), SECOND, 0).await.unwrap();
    assert!(done.pane_cleared);
    assert_eq!(tui.restored(), Some(1));
}

/// 帶樣式的畫面、輸入列裡是真的字（沒有 dim）：還是 `composer_busy`，一個字都不打。
#[tokio::test]
async fn a_styled_screen_with_typed_text_is_still_busy() {
    let tui = FakeTui::new(&[A, SECOND], Faults { styled_hint: Some("unused".into()), composer: Some("half typed".into()), ..Default::default() });
    assert_eq!(drive(tui.as_ref(), SECOND, 0).await.unwrap_err(), Fail::ComposerBusy);
    assert!(tui.log().is_empty());
}

/// 真的 pane 要帶樣式讀（`format: ansi`），不然分不出建議句。
#[tokio::test]
async fn the_real_pane_is_read_with_styles() {
    let e = tt::env().await;
    e.herdr.set_screen("w1:p1", SUGGESTION);
    let pane = HerdrPane { client: e.app.herdr.clone(), pane_id: "w1:p1".into() };
    let raw = pane.read().await.unwrap();
    let read = e.herdr.calls_to("pane.read").pop().expect("pane.read");
    assert_eq!(read["format"], "ansi", "{read}");
    assert_eq!(lifecycle::composer_text("claude", &screen_text(&raw)), None, "讀回來的建議句判成空的輸入列");
}

/// 確認頁的字對不上：Esc 退出，**絕不選 Restore**。
#[tokio::test]
async fn a_confirm_page_with_other_text_backs_out_without_restoring() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults { confirm_text: Some("Something else entirely".into()), ..Default::default() });
    let err = drive(tui.as_ref(), SECOND, 0).await.unwrap_err();
    assert!(matches!(err, Fail::TextMismatch(ref s) if s.contains("Something else")), "{err:?}");
    assert_eq!(tui.restored(), None, "沒有 Restore");
    assert_eq!(tui.ui(), Ui::Idle, "退回閒著的畫面");
    assert_eq!(tui.entries.lock().unwrap().len(), 3, "對話一則都沒少");
    let log = tui.log();
    let confirm_at = log.iter().rposition(|k| k == "Enter").unwrap();
    assert!(log[confirm_at + 1..].iter().all(|k| k == "Escape"), "進確認頁之後只按了 Esc：{log:?}");
    assert!(!log.iter().any(|k| k == "1"), "沒有按 1（Restore）");
}

#[tokio::test]
async fn a_confirmation_that_closes_before_restore_does_not_get_a_stale_one() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    let racing_tui = tui.clone();
    lifecycle::race_point::arm("rewind_before_restore", &tui.race_key(), move || async move {
        racing_tui.close_confirmation();
    });

    assert_eq!(drive(tui.as_ref(), SECOND, 0).await.unwrap_err(), Fail::ConfirmNotShown);
    assert_eq!(tui.restored(), None);
    assert!(!tui.log().contains(&"1".to_string()), "沒有把選擇鍵送進離開後的畫面");
}

/// #573：race point 掛在某一個 pane 上，別的 pane 倒同一句話時不能把它拿走——以前 key 是目標文字，
/// 平行跑的其他測試也倒 SECOND，先走到那一點的就把 hook 吃掉，上面那條間歇紅。
#[tokio::test]
async fn a_race_hook_armed_for_one_pane_is_not_taken_by_another_pane_rewinding_the_same_words() {
    let mine = FakeTui::new(&[A, SECOND, C], Faults::default());
    let other = FakeTui::new(&[A, SECOND, C], Faults::default());
    let racing = mine.clone();
    lifecycle::race_point::arm("rewind_before_restore", &mine.race_key(), move || async move {
        racing.close_confirmation();
    });

    drive(other.as_ref(), SECOND, 0).await.unwrap();
    assert_eq!(other.restored(), Some(1), "別的 pane 照常倒回");
    assert_eq!(drive(mine.as_ref(), SECOND, 0).await.unwrap_err(), Fail::ConfirmNotShown, "hook 還留給掛它的那個 pane");
    assert_eq!(mine.restored(), None);
}

/// 選 Restore 用 `1`，不靠游標：游標停在別的選項、或選項整組被擠出畫面（矮 pane）都一樣選到 Restore。
#[tokio::test]
async fn restore_is_chosen_by_number_not_by_where_the_cursor_is() {
    let tui = FakeTui::new(&[A, SECOND, C], Faults { start_option: 3, ..Default::default() });
    drive(tui.as_ref(), SECOND, 0).await.unwrap();
    assert_eq!(tui.restored(), Some(1));
    let tui = FakeTui::new(&[A, SECOND, C], Faults { hide_options: true, ..Default::default() });
    drive(tui.as_ref(), SECOND, 0).await.unwrap();
    assert_eq!(tui.restored(), Some(1));
}

/// `/rewind` 沒出選單：逾時、清掉打進去的 `/rewind`，什麼都沒倒。
#[tokio::test]
async fn a_menu_that_never_appears_times_out_and_leaves_the_composer_clean() {
    let tui = FakeTui::new(&[A, SECOND], Faults { menu_disabled: true, ..Default::default() });
    assert_eq!(drive(tui.as_ref(), SECOND, 0).await.unwrap_err(), Fail::MenuNotShown);
    assert_eq!(tui.composer(), "", "打進去的 /rewind 被清掉");
    assert_eq!(tui.restored(), None);
}

/// 一路往上到頂都沒有：退出，回錯。
#[tokio::test]
async fn a_message_not_in_the_menu_is_not_found() {
    let tui = FakeTui::new(&[A, SECOND], Faults::default());
    assert_eq!(drive(tui.as_ref(), "never sent", 0).await.unwrap_err(), Fail::NotInMenu);
    assert_eq!(tui.ui(), Ui::Idle);
    assert_eq!(tui.restored(), None);
}

/// 輸入列有字：一個字都不打。
#[tokio::test]
async fn a_busy_composer_is_left_alone() {
    let tui = FakeTui::new(&[A], Faults { composer: Some("half typed".into()), ..Default::default() });
    assert_eq!(drive(tui.as_ref(), A, 0).await.unwrap_err(), Fail::ComposerBusy);
    assert!(tui.log().is_empty());
    assert_eq!(tui.composer(), "half typed");
}

/// 同一句送過兩次：跳過較新的那一則，倒到舊的那一則。
#[tokio::test]
async fn the_same_words_twice_skip_the_newer_one() {
    let tui = FakeTui::new(&[A, "again", SECOND, "again"], Faults::default());
    drive(tui.as_ref(), "again", 1).await.unwrap();
    assert_eq!(tui.restored(), Some(1));
    let tui = FakeTui::new(&[A, "again", SECOND, "again"], Faults::default());
    drive(tui.as_ref(), "again", 0).await.unwrap();
    assert_eq!(tui.restored(), Some(3));
}

/// 按了 Restore 畫面沒離開：不知道倒了沒有，照實回報。
#[tokio::test]
async fn a_restore_that_does_not_leave_the_ui_is_unconfirmed() {
    let tui = FakeTui::new(&[A, SECOND], Faults { stuck_after_restore: true, ..Default::default() });
    assert_eq!(drive(tui.as_ref(), SECOND, 0).await.unwrap_err(), Fail::Unconfirmed);
}

// ───────────── 端點 ─────────────

struct Rig {
    e: tt::Env,
    bot: String,
    run: String,
    ids: Vec<String>,
}

async fn insert_msgs(app: &Arc<App>, bot: &str, msgs: &[(&str, &str)]) -> Vec<String> {
    let conv = db::conversation_id(&app.db, bot).await.unwrap();
    let mut ids = Vec::new();
    for (role, text) in msgs {
        let id = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,?,?,'hook',?)")
            .bind(&id)
            .bind(&conv)
            .bind(role)
            .bind(text)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

/// 一顆跑著、閒著的 claude，網頁上有 A／SECOND／C 三則與回覆。
async fn rig() -> Rig {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "rw").await;
    let run = tt::fake_run(&e.app, &bot.id).await;
    let ids = insert_msgs(&e.app, &bot.id, &[("user", A), ("assistant", "OK"), ("user", SECOND), ("assistant", "OK"), ("user", C), ("assistant", "OK")]).await;
    Rig { bot: bot.id, run, ids, e }
}

async fn call(r: &Rig, id: &str, tui: &Arc<FakeTui>) -> LcResult<Value> {
    rewind(&r.e.app, &r.bot, id, Some(tui.clone() as Arc<dyn Pane>)).await
}

fn reason(e: LcError) -> String {
    match e {
        LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
        other => format!("{other:?}"),
    }
}

async fn rewound(r: &Rig) -> Vec<bool> {
    let mut out = Vec::new();
    for id in &r.ids {
        let at: Option<String> = sqlx::query_scalar("SELECT rewound_at FROM messages WHERE id = ?").bind(id).fetch_one(&r.e.app.db).await.unwrap();
        out.push(at.is_some());
    }
    out
}

/// 整條路：倒回 SECOND，那一則與之後的標成倒回（不刪），原文回給前端，pane 標成打過字，對話多一則說明。
#[tokio::test]
async fn a_rewind_marks_that_message_and_later_and_returns_its_text() {
    let r = rig().await;
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    let out = call(&r, &r.ids[2], &tui).await.unwrap();
    assert_eq!(out["text"], SECOND);
    assert_eq!(out["hidden"], 4);
    assert_eq!(out["pane_cleared"], true);
    assert_eq!(tui.restored(), Some(1));
    assert_eq!(rewound(&r).await, vec![false, false, true, true, true, true]);
    let typed: i64 = sqlx::query_scalar("SELECT pane_typed FROM runs WHERE id = ?").bind(&r.run).fetch_one(&r.e.app.db).await.unwrap();
    assert_eq!(typed, 1, "對 pane 打過字要記下來");
    let conv = db::conversation_id(&r.e.app.db, &r.bot).await.unwrap();
    let note: String = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY rowid DESC LIMIT 1")
        .bind(&conv)
        .fetch_one(&r.e.app.db)
        .await
        .unwrap();
    assert!(note.contains("已倒回"), "{note}");
    assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "already_rewound");
}

/// 確認頁對不上：409、訊息一則都不標。
#[tokio::test]
async fn a_failed_rewind_marks_nothing() {
    let r = rig().await;
    let tui = FakeTui::new(&[A, SECOND, C], Faults { confirm_text: Some("other".into()), ..Default::default() });
    assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "text_mismatch");
    assert_eq!(rewound(&r).await, vec![false; 6]);
}

#[tokio::test]
async fn codex_and_grok_are_unsupported() {
    let r = rig().await;
    for kind in ["codex", "grok"] {
        sqlx::query("UPDATE bots SET kind = ? WHERE id = ?").bind(kind).bind(&r.bot).execute(&r.e.app.db).await.unwrap();
        let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
        assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "unsupported_kind");
        assert!(tui.log().is_empty(), "一個鍵都沒按");
    }
}

/// 使用者自己 default session 的 pane 只觀察、不代打（SPEC §6.5.1）。
#[tokio::test]
async fn a_bot_from_the_users_default_session_is_not_driven() {
    let r = rig().await;
    sqlx::query("UPDATE bots SET herdr_session = 'default' WHERE id = ?").bind(&r.bot).execute(&r.e.app.db).await.unwrap();
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "default_session");
    assert!(tui.log().is_empty());
}

/// 正在忙（跑回合、卡提問、有在飛的回合）：一個鍵都不按。
#[tokio::test]
async fn a_busy_bot_is_refused_before_any_key() {
    let r = rig().await;
    for status in ["working", "blocked"] {
        sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&r.run).execute(&r.e.app.db).await.unwrap();
        let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
        assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "not_idle");
        assert!(tui.log().is_empty());
    }
    sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE id = ?").bind(&r.run).execute(&r.e.app.db).await.unwrap();
    let conv = db::conversation_id(&r.e.app.db, &r.bot).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, created_at) VALUES (?,?,?,'web','in_flight',?)")
        .bind(db::ulid())
        .bind(&conv)
        .bind(&r.run)
        .bind(db::now())
        .execute(&r.e.app.db)
        .await
        .unwrap();
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    assert_eq!(reason(call(&r, &r.ids[2], &tui).await.unwrap_err()), "not_idle");
    assert!(tui.log().is_empty());
    assert_eq!(rewound(&r).await, vec![false; 6]);
}

/// 找不到訊息、不是使用者訊息、別顆 bot 的訊息。
#[tokio::test]
async fn messages_that_cannot_be_rewound_are_refused() {
    let r = rig().await;
    let tui = FakeTui::new(&[A, SECOND, C], Faults::default());
    assert!(matches!(call(&r, "nope", &tui).await.unwrap_err(), LcError::NotFound(_)));
    assert_eq!(reason(call(&r, &r.ids[1], &tui).await.unwrap_err()), "not_a_user_message");
    let other = tt::claude_bot(&r.e.app, &r.e.project_id, "other").await;
    let res = rewind(&r.e.app, &other.id, &r.ids[2], Some(tui.clone() as Arc<dyn Pane>)).await;
    assert!(matches!(res, Err(LcError::NotFound(_))));
    assert!(tui.log().is_empty());
}

/// 同一句在網頁上送過兩次：倒較舊那一則時，較新那則要在選單上跳過。
#[tokio::test]
async fn an_older_duplicate_is_found_past_the_newer_one() {
    let e = tt::env().await;
    let bot = tt::claude_bot(&e.app, &e.project_id, "dup").await;
    tt::fake_run(&e.app, &bot.id).await;
    let ids = insert_msgs(&e.app, &bot.id, &[("user", A), ("user", "again"), ("user", SECOND), ("user", "again")]).await;
    let tui = FakeTui::new(&[A, "again", SECOND, "again"], Faults::default());
    rewind(&e.app, &bot.id, &ids[1], Some(tui.clone() as Arc<dyn Pane>)).await.unwrap();
    assert_eq!(tui.restored(), Some(1), "倒的是第一個 again，不是最後那個");
}

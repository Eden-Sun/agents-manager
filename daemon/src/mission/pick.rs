//! 群組任務要派給哪個身分（`docs/goals/agm-missions.md` D3–D7）。
//!
//! 這是**確定性規則**，放在 daemon 而不是交給 AGM（LLM）判斷：同樣的額度狀態永遠挑出同一個
//! 身分，AGM 只負責照結果開 bot。純函式，吃額度快照、吐決定，所有情境都在底下的測試裡。
//!
//! 規則：
//! - 身分依固定順序（cc2 → cc1 → cc0）看，**用盡才往下一個**（D4）：還能用就一直是它，
//!   `low` 不算用盡。
//! - claude 有三個桶，撞到哪個決定怎麼辦：
//!   - **7d**（`critical` 或 `limit_hit` 判定為週窗）→ 這個身分用盡，換下一個。
//!   - **5h** → 看任務開始時選的 `on_5h_limit`（D5）：`wait` 就原地等它重置，`switch` 就換下一個。
//!   - **Fable 週桶** → 執行者／reviewer 同一身分改用 opus，不換身分；**驗證者**不能用這個身分（D3）。
//! - 驗證者一個能用的身分都沒有 → 停下來問使用者（D6），不降級、不乾等。
//! - reviewer 必須是執行者以外的身分；沒有 → `NoIndependentReviewer`，由呼叫端改走「自審＋驗證者把關」。
//! - 讀不到額度（`None`）視為可以用：未知不等於用盡（與 AGM 自己的模型控制器同一條原則）——
//!   但驗證者例外，D3 要的是「確定有 Fable 額度」，讀不到就不算。

use crate::quota::{LimitHit, Quota, Window};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Executor,
    Reviewer,
    Verifier,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "executor" => Some(Self::Executor),
            "reviewer" => Some(Self::Reviewer),
            "verifier" => Some(Self::Verifier),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum On5hLimit {
    Wait,
    Switch,
}

/// 一個候選身分此刻的狀態。`quota` 是 `claude:<name>` 那一把（cc0 可能落在裸的 `claude`，呼叫端先合好）。
pub struct Candidate<'a> {
    pub name: &'a str,
    pub disabled: bool,
    pub quota: Option<&'a Quota>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Pick {
    /// 用這個身分。`model` 有值時要換成那個模型（Fable 桶用盡的執行者＝`opus`；驗證者一律 `fable`）。
    Use { identity: String, model: Option<String>, reason: String },
    /// 原地等這個身分的 5h 窗重置（`on_5h_limit = wait`），或所有身分都用盡時等最早回來的那一個。
    Wait { identity: String, until: Option<String>, reason: String },
    /// 停下來問使用者（驗證者找不到 Fable 有效額度，D6）。`resets` 列出每個身分 Fable 何時重置。
    AskUser { reason: String, resets: Vec<Reset> },
    /// 找不到跟執行者不同的身分當 reviewer。
    NoIndependentReviewer { reason: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reset {
    pub identity: String,
    pub resets_at: Option<String>,
}

/// 這個身分現在卡在哪一個桶。
#[derive(Debug, Clone, PartialEq)]
enum Block {
    None,
    FiveHour(Option<String>),
    SevenDay(Option<String>),
    /// 只有某個模型的週桶用盡（Fable／Opus／Sonnet）：同一身分換別的模型還能跑。
    ModelOut { model: String, until: Option<String> },
}

/// 那個模型的週桶用盡時，同一身分改用哪個模型。Fable 用盡→opus（7d 共用桶沒見底才會走到這裡）；
/// Opus 用盡→確定還有 Fable 額度就用 Fable，否則 sonnet；Sonnet 用盡→opus。
fn fallback_model(out: &str, q: &Quota) -> Option<&'static str> {
    let fable_ok = q.fable.as_ref().is_some_and(|w| !w.critical());
    match out {
        "fable" => Some("opus"),
        "opus" if fable_ok => Some("fable"),
        "opus" => Some("sonnet"),
        "sonnet" => Some("opus"),
        _ => None,
    }
}

fn hit_active(hit: &LimitHit, now: DateTime<Utc>) -> bool {
    match hit.until.as_deref().and_then(|u| DateTime::parse_from_rfc3339(u).ok()) {
        Some(until) => until.with_timezone(&Utc) > now,
        // 沒寫時間的橫幅要等下一個成功回合才清掉（quota.rs 的黏住規則），這裡照樣當成還擋著。
        None => true,
    }
}

fn exhausted(w: Option<&Window>) -> bool {
    w.is_some_and(|w| w.critical())
}

fn reset_of(w: Option<&Window>) -> Option<String> {
    w.and_then(|w| w.resets_at.clone())
}

/// 兩個重置時間取早的（任一邊沒有就用另一邊）。撞限最晚到那一桶自己重置為止。
fn sooner(a: Option<String>, b: Option<String>) -> Option<String> {
    let t = |s: &str| DateTime::parse_from_rfc3339(s).ok().map(|x| x.with_timezone(&Utc));
    match (a, b) {
        (Some(x), Some(y)) => match (t(&x), t(&y)) {
            (Some(tx), Some(ty)) if ty < tx => Some(y),
            (None, Some(_)) => Some(y),
            _ => Some(x),
        },
        (x, None) => x,
        (None, y) => y,
    }
}

/// 這個身分卡在哪一桶。
///
/// 橫幅說了是哪一桶（`hit.bucket`）就**照它決定種類**，不再拿當下的讀數猜：那一桶沒讀數時（剛重啟、
/// Fable 只有 `/usage` 讀得到）猜出來的一律是週窗，Fable 撞限變成整個身分換掉、session 撞限的 +5h
/// 保底被丟掉（review 2026-09-16 M7）。唯一的例外是讀數本身說更大的桶也見底了（5h／7d `critical`）。
/// 時間取 `hit.until` 與那一桶自己的 `resets_at` 較早者。
///
/// 沒有桶名（codex、舊資料、開機回填）才用讀數推：5h 見底就是 5h，否則 Fable 見底就是 Fable，
/// 都看不出來（例如 credits 用完、桶子卻是滿的）就當週窗——等不到，只能換身分。
fn block_of(q: &Quota, now: DateTime<Utc>) -> Block {
    if let Some(hit) = q.limit_hit.as_ref().filter(|h| hit_active(h, now)) {
        let (five, seven, fable) = (q.five_hour.as_ref(), q.seven_day.as_ref(), q.fable.as_ref());
        let model_bucket = matches!(hit.bucket.as_deref(), Some("fable") | Some("opus") | Some("sonnet"));
        match hit.bucket.as_deref() {
            Some("seven_day") => return Block::SevenDay(sooner(hit.until.clone(), reset_of(seven))),
            Some("five_hour") if exhausted(seven) => return Block::SevenDay(reset_of(seven)),
            Some("five_hour") => return Block::FiveHour(sooner(hit.until.clone(), reset_of(five))),
            Some(_) if model_bucket && exhausted(seven) => return Block::SevenDay(reset_of(seven)),
            // 5h 也見底：先等 5h（時間是 5h 自己的，不是模型週桶的下週）；5h 回來之後撞限還在，再換模型。
            Some(_) if model_bucket && exhausted(five) => return Block::FiveHour(reset_of(five)),
            // Opus／Sonnet 的週桶沒有自己的量表，時間借 7d 那格（同一個重置週期）。
            Some(m) if model_bucket => {
                let own = if m == "fable" { reset_of(fable) } else { reset_of(seven) };
                return Block::ModelOut { model: m.to_string(), until: sooner(hit.until.clone(), own) };
            }
            _ => {}
        }
        if exhausted(five) && !exhausted(seven) {
            return Block::FiveHour(hit.until.clone().or(reset_of(five)));
        }
        if exhausted(fable) && !exhausted(seven) && !exhausted(five) {
            return Block::ModelOut { model: "fable".into(), until: hit.until.clone().or(reset_of(fable)) };
        }
        return Block::SevenDay(hit.until.clone().or(reset_of(seven)));
    }
    if exhausted(q.seven_day.as_ref()) {
        return Block::SevenDay(reset_of(q.seven_day.as_ref()));
    }
    if exhausted(q.five_hour.as_ref()) {
        return Block::FiveHour(reset_of(q.five_hour.as_ref()));
    }
    if exhausted(q.fable.as_ref()) {
        return Block::ModelOut { model: "fable".into(), until: reset_of(q.fable.as_ref()) };
    }
    Block::None
}

/// 最早回來的那個重置時間（RFC3339 字串可以直接比大小：同一種格式、UTC）。
fn earliest(resets: impl Iterator<Item = (String, Option<String>)>) -> Option<(String, Option<String>)> {
    resets.min_by(|a, b| match (&a.1, &b.1) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    })
}

pub fn pick(role: Role, candidates: &[Candidate], on_5h: On5hLimit, exclude: Option<&str>, now: DateTime<Utc>) -> Pick {
    match role {
        Role::Executor | Role::Reviewer => pick_worker(role, candidates, on_5h, exclude, now),
        Role::Verifier => pick_verifier(candidates, on_5h, now),
    }
}

fn pick_worker(role: Role, candidates: &[Candidate], on_5h: On5hLimit, exclude: Option<&str>, now: DateTime<Utc>) -> Pick {
    let mut exhausted_resets: Vec<(String, Option<String>)> = Vec::new();
    let mut any_other = false;
    for c in candidates {
        if c.disabled || Some(c.name) == exclude {
            continue;
        }
        any_other = true;
        let block = c.quota.map(|q| block_of(q, now)).unwrap_or(Block::None);
        match block {
            Block::None => {
                return Pick::Use { identity: c.name.into(), model: None, reason: "額度可用".into() };
            }
            Block::ModelOut { model, until } => match c.quota.and_then(|q| fallback_model(&model, q)) {
                Some(alt) => {
                    return Pick::Use {
                        identity: c.name.into(),
                        model: Some(alt.into()),
                        reason: format!("{model} 週桶已用盡，同一身分改用 {alt}"),
                    };
                }
                // 換不到別的模型：這個身分這一輪用不了，跟週窗用盡一樣往下一個身分找。
                None => exhausted_resets.push((c.name.into(), until)),
            },
            Block::FiveHour(until) => match on_5h {
                On5hLimit::Wait => {
                    return Pick::Wait { identity: c.name.into(), until, reason: "5 小時窗撞限，依任務設定原地等重置".into() };
                }
                On5hLimit::Switch => exhausted_resets.push((c.name.into(), until)),
            },
            Block::SevenDay(until) => exhausted_resets.push((c.name.into(), until)),
        }
    }
    if role == Role::Reviewer && !any_other {
        return Pick::NoIndependentReviewer { reason: "沒有跟執行者不同、且未停用的身分".into() };
    }
    match earliest(exhausted_resets.into_iter()) {
        Some((identity, until)) => Pick::Wait { identity, until, reason: "所有身分都已用盡，等最早重置的那一個".into() },
        None if role == Role::Reviewer => Pick::NoIndependentReviewer { reason: "沒有可用的身分".into() },
        None => Pick::AskUser { reason: "沒有任何未停用的身分".into(), resets: Vec::new() },
    }
}

fn pick_verifier(candidates: &[Candidate], on_5h: On5hLimit, now: DateTime<Utc>) -> Pick {
    let mut resets = Vec::new();
    // Fable 還有額度、只是 5h 窗撞限的身分（任務選 `switch` 時會先看下一個身分）。全部看完都沒有能馬上用的，
    // 就等這裡面最早重置的那一個——這是「1～2 小時後就回來」，不是 D6 的「沒有 Fable 額度」（review3 c1 L12）。
    let mut five_hour_waits: Vec<(String, Option<String>)> = Vec::new();
    for c in candidates {
        if c.disabled {
            continue;
        }
        let Some(q) = c.quota else {
            resets.push(Reset { identity: c.name.into(), resets_at: None });
            continue;
        };
        // D3：確定有 Fable 額度才算——讀不到 Fable 桶跟用盡一樣不能用。
        let fable_ok = q.fable.as_ref().is_some_and(|w| !w.critical());
        match block_of(q, now) {
            Block::None if fable_ok => {
                return Pick::Use { identity: c.name.into(), model: Some("fable".into()), reason: "Fable 週桶有額度".into() };
            }
            // 別的模型的週桶用盡跟驗證者無關：它只跑 Fable。
            Block::ModelOut { ref model, .. } if model != "fable" && fable_ok => {
                return Pick::Use { identity: c.name.into(), model: Some("fable".into()), reason: "Fable 週桶有額度".into() };
            }
            Block::FiveHour(until) if fable_ok && on_5h == On5hLimit::Wait => {
                return Pick::Wait { identity: c.name.into(), until, reason: "驗證者的 5 小時窗撞限，依任務設定原地等重置".into() };
            }
            Block::FiveHour(until) if fable_ok => {
                five_hour_waits.push((c.name.into(), until));
                resets.push(Reset { identity: c.name.into(), resets_at: reset_of(q.fable.as_ref()) });
            }
            _ => resets.push(Reset { identity: c.name.into(), resets_at: reset_of(q.fable.as_ref()) }),
        }
    }
    // 有 Fable 額度、只卡 5h：等最早重置的那個，不要停下來問人——以前這種情況回 `AskUser`，說的是「沒有 Fable 額度」，
    // 給的重置時間還是 Fable 的（好幾天後），使用者可能因此同意降級成 opus 驗證（review3 c1 L12）。
    if let Some((identity, until)) = earliest(five_hour_waits.into_iter()) {
        return Pick::Wait { identity, until, reason: "驗證者的 Fable 還有額度，只是 5 小時窗撞限，等最早重置的那一個".into() };
    }
    Pick::AskUser { reason: "沒有任何身分的 Fable 週桶有有效額度，驗證者必須用 Fable".into(), resets }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-13T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    fn w(used: f64, resets: &str) -> Option<Window> {
        Some(Window { used_pct: used, resets_at: Some(resets.into()) })
    }

    fn q(five: f64, seven: f64, fable: Option<f64>) -> Quota {
        Quota {
            five_hour: w(five, "2026-09-13T15:00:00Z"),
            seven_day: w(seven, "2026-09-18T06:00:00Z"),
            fable: fable.and_then(|f| w(f, "2026-09-18T06:00:00Z")),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: "2026-09-13T11:59:00Z".into(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        }
    }

    fn hit(mut quota: Quota, until: Option<&str>) -> Quota {
        quota.limit_hit = Some(LimitHit { message: "You've reached your limit".into(), until: until.map(String::from), at: "2026-09-13T11:50:00Z".into(), bucket: None });
        quota
    }

    fn cands<'a>(qs: &'a [(&'a str, Option<Quota>)]) -> Vec<Candidate<'a>> {
        qs.iter().map(|(n, q)| Candidate { name: n, disabled: false, quota: q.as_ref() }).collect()
    }

    fn used(p: &Pick) -> (&str, Option<&str>) {
        match p {
            Pick::Use { identity, model, .. } => (identity, model.as_deref()),
            other => panic!("expected Use, got {other:?}"),
        }
    }

    /// 橫幅說是哪一桶就照它的。以前用「當下哪個桶見底」倒推：撞 Fable 上限而 5h 剛好也快滿時，
    /// 會把 Fable 的下週重置時間當成「5 小時窗什麼時候回來」，整個身分被鎖到下週（review 2026-09-16）。
    #[test]
    fn the_banner_says_which_bucket_and_that_beats_guessing() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-16T10:00:00Z").unwrap().with_timezone(&Utc);
        let w = |used: f64| Some(crate::quota::Window { used_pct: used, resets_at: Some("2026-09-23T00:00:00Z".into()) });
        let mut q = crate::quota::Quota {
            five_hour: w(96.0), // 剛好也快滿
            seven_day: w(40.0),
            fable: w(100.0),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: "2026-09-16T09:59:00Z".into(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        };
        let hit = |bucket: Option<&str>| crate::quota::LimitHit {
            message: "You've hit your Fable limit".into(),
            until: Some("2026-09-23T00:00:00Z".into()),
            at: "2026-09-16T09:59:00Z".into(),
            bucket: bucket.map(str::to_string),
        };
        // 5h 也見底了，所以封鎖確實是 5h——但**時間不能拿 Fable 的**。
        q.limit_hit = Some(hit(Some("fable")));
        assert_eq!(
            block_of(&q, now),
            Block::FiveHour(Some("2026-09-23T00:00:00Z".into())),
            "桶名對不上時要用 5h 自己的 resets_at"
        );
        let five_reset = |t: &str| {
            let mut q2 = q.clone();
            q2.five_hour = Some(crate::quota::Window { used_pct: 96.0, resets_at: Some(t.into()) });
            q2
        };
        let mut q3 = five_reset("2026-09-16T14:00:00Z");
        q3.limit_hit = Some(hit(Some("fable")));
        assert_eq!(block_of(&q3, now), Block::FiveHour(Some("2026-09-16T14:00:00Z".into())), "5h 的時間才對");
        // 橫幅就是這一桶：種類照它，時間取橫幅與這一桶自己重置較早的那個——撞限不會比它那一桶活得久（M2）。
        let mut q4 = five_reset("2026-09-16T14:00:00Z");
        q4.limit_hit = Some(crate::quota::LimitHit { bucket: Some("five_hour".into()), ..hit(None) });
        assert_eq!(block_of(&q4, now), Block::FiveHour(Some("2026-09-16T14:00:00Z".into())), "橫幅說的就是 5h");
        // 舊資料沒有 bucket：維持舊行為。
        let mut q5 = five_reset("2026-09-16T14:00:00Z");
        q5.limit_hit = Some(hit(None));
        assert_eq!(block_of(&q5, now), Block::FiveHour(Some("2026-09-23T00:00:00Z".into())), "沒有 bucket 時照舊");
    }

    /// M7（review 2026-09-16）：橫幅說了是哪一桶，那一桶卻還沒有讀數（剛重啟、Fable 只有 `/usage` 讀得到）。
    /// 以前照讀數猜，一律猜成週窗：Fable 撞限把整個身分換掉，session 撞限的 +5h 保底被週窗的重置時間蓋掉。
    #[test]
    fn a_banner_bucket_without_a_reading_still_decides_the_kind_of_block() {
        let now = now();
        let hit = |bucket: &str, until: &str| LimitHit {
            message: "You've hit your limit".into(),
            until: Some(until.into()),
            at: "2026-09-13T11:50:00Z".into(),
            bucket: Some(bucket.into()),
        };
        let mut fable_hit = q(10.0, 20.0, None);
        fable_hit.limit_hit = Some(hit("fable", "2026-09-20T11:50:00Z"));
        assert_eq!(block_of(&fable_hit, now), Block::ModelOut { model: "fable".into(), until: Some("2026-09-20T11:50:00Z".into()) });
        let qs = [("cc2", Some(fable_hit.clone())), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Wait, None, now)), ("cc2", Some("opus")), "Fable 用完是同身分換 opus，不是換身分");

        let mut session = q(10.0, 20.0, Some(10.0));
        session.five_hour = None;
        session.limit_hit = Some(hit("five_hour", "2026-09-13T16:50:00Z"));
        match pick(Role::Executor, &cands(&[("cc2", Some(session))]), On5hLimit::Wait, None, now) {
            Pick::Wait { identity, until, .. } => {
                assert_eq!(identity, "cc2");
                assert_eq!(until.as_deref(), Some("2026-09-13T16:50:00Z"), "保底 +5h，不是週窗的重置時間");
            }
            other => panic!("expected Wait, got {other:?}"),
        }

        // 讀數本身說週窗也見底：那就是週窗，桶名擋不住。
        let mut week_gone = q(10.0, 100.0, None);
        week_gone.limit_hit = Some(hit("fable", "2026-09-20T11:50:00Z"));
        assert_eq!(block_of(&week_gone, now), Block::SevenDay(Some("2026-09-18T06:00:00Z".into())));
    }

    /// review3 c4 M1：`Opus limit`／`Sonnet limit` 是模型自己的週桶，不是整個身分的 7d。
    /// 以前被記成 `seven_day`：同身分所有 claude bot 停派、群組任務把整個身分換掉，一路到週重置。
    #[test]
    fn a_model_week_bucket_switches_models_instead_of_dropping_the_identity() {
        let now = now();
        let hit = |bucket: &str| LimitHit {
            message: format!("You've hit your {bucket} limit · resets Sep 18"),
            until: Some("2026-09-18T06:00:00Z".into()),
            at: "2026-09-13T11:50:00Z".into(),
            bucket: Some(bucket.into()),
        };
        // Opus 週桶用盡、Fable 還有：同一身分改用 fable，不換身分。
        let mut opus_out = q(10.0, 20.0, Some(10.0));
        opus_out.limit_hit = Some(hit("opus"));
        assert_eq!(
            block_of(&opus_out, now),
            Block::ModelOut { model: "opus".into(), until: Some("2026-09-18T06:00:00Z".into()) },
            "時間借 7d 那格（同一個重置週期）"
        );
        let qs = [("cc2", Some(opus_out.clone())), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Wait, None, now)), ("cc2", Some("fable")));

        // Fable 那格也見底（或根本沒讀數）：退而求其次用 sonnet，仍然是同一個身分。
        let mut both = q(10.0, 20.0, Some(100.0));
        both.limit_hit = Some(hit("opus"));
        assert_eq!(used(&pick(Role::Executor, &cands(&[("cc2", Some(both))]), On5hLimit::Wait, None, now)), ("cc2", Some("sonnet")));

        // Sonnet 週桶用盡：換 opus。
        let mut sonnet_out = q(10.0, 20.0, Some(10.0));
        sonnet_out.limit_hit = Some(hit("sonnet"));
        assert_eq!(used(&pick(Role::Executor, &cands(&[("cc2", Some(sonnet_out))]), On5hLimit::Wait, None, now)), ("cc2", Some("opus")));

        // 驗證者只跑 Fable：別的模型的週桶用盡跟它無關。
        let mut opus_out_v = q(10.0, 20.0, Some(10.0));
        opus_out_v.limit_hit = Some(hit("opus"));
        assert_eq!(used(&pick(Role::Verifier, &cands(&[("cc2", Some(opus_out_v))]), On5hLimit::Wait, None, now)), ("cc2", Some("fable")));

        // 讀數本身說 7d 也見底：那就是整個身分用盡，桶名擋不住。
        let mut week_gone = q(10.0, 100.0, Some(10.0));
        week_gone.limit_hit = Some(hit("opus"));
        assert_eq!(block_of(&week_gone, now), Block::SevenDay(Some("2026-09-18T06:00:00Z".into())));
    }

    /// review3 c1 L11：撞限換手挑 reviewer 時要排除執行者的身分，不然換完 reviewer 跟執行者同一個帳號。
    #[test]
    fn a_reviewer_never_lands_on_the_executors_identity() {
        let now = now();
        let qs = [("cc2", Some(q(10.0, 20.0, Some(10.0)))), ("cc1", Some(q(10.0, 20.0, Some(10.0))))];
        // 沒有排除：挑到順序上的第一個，可能正是執行者。
        assert_eq!(used(&pick(Role::Reviewer, &cands(&qs), On5hLimit::Wait, None, now)), ("cc2", None));
        // 排除執行者：挑另一個身分。
        assert_eq!(used(&pick(Role::Reviewer, &cands(&qs), On5hLimit::Wait, Some("cc2"), now)), ("cc1", None));
        // 只剩執行者自己：寧可回「沒有獨立 reviewer」，也不要偷偷用同一個帳號。
        let only = [("cc2", Some(q(10.0, 20.0, Some(10.0))))];
        assert!(matches!(
            pick(Role::Reviewer, &cands(&only), On5hLimit::Wait, Some("cc2"), now),
            Pick::NoIndependentReviewer { .. }
        ));
    }

    /// review3 c1 L12：三個身分的 Fable 都還有額度、只是 5h 全撞限（任務選 `switch`）。以前回 `AskUser`
    /// 「沒有 Fable 額度」、時間給的是 Fable 的下週，使用者可能因此同意降級成 opus 驗證——其實 1～2 小時就回來。
    #[test]
    fn a_verifier_blocked_only_by_the_five_hour_window_waits_instead_of_asking() {
        let now = now();
        let five_hit = |resets: &str| {
            let mut x = q(100.0, 20.0, Some(10.0));
            x.five_hour = w(100.0, resets);
            x.limit_hit = Some(LimitHit {
                message: "You've hit your session limit".into(),
                until: Some(resets.into()),
                at: "2026-09-13T11:50:00Z".into(),
                bucket: Some("five_hour".into()),
            });
            x
        };
        let qs = [("cc2", Some(five_hit("2026-09-13T15:00:00Z"))), ("cc1", Some(five_hit("2026-09-13T13:30:00Z")))];
        match pick(Role::Verifier, &cands(&qs), On5hLimit::Switch, None, now) {
            Pick::Wait { identity, until, .. } => {
                assert_eq!(identity, "cc1", "等最早重置的那一個");
                assert_eq!(until.as_deref(), Some("2026-09-13T13:30:00Z"), "給的是 5h 的重置時間，不是 Fable 的下週");
            }
            other => panic!("expected Wait, got {other:?}"),
        }
        // 真的沒有 Fable 額度時照舊停下來問人。
        let no_fable = [("cc2", Some(q(10.0, 20.0, Some(100.0))))];
        assert!(matches!(
            pick(Role::Verifier, &cands(&no_fable), On5hLimit::Switch, None, now),
            Pick::AskUser { .. }
        ));
    }

    #[test]
    fn keeps_using_the_first_identity_until_it_is_exhausted_not_merely_low() {
        let qs = [("cc2", Some(q(80.0, 90.0, Some(90.0)))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        // 剩 10–20% 只是 low，不是用盡：照樣用 cc2。
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Switch, None, now())), ("cc2", None));
    }

    #[test]
    fn a_seven_day_exhaustion_moves_to_the_next_identity() {
        let qs = [("cc2", Some(q(10.0, 99.0, Some(10.0)))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Wait, None, now())), ("cc1", None));
    }

    #[test]
    fn a_five_hour_hit_waits_or_switches_as_the_mission_chose() {
        let qs = [("cc2", Some(hit(q(100.0, 40.0, Some(40.0)), Some("2026-09-13T15:00:00Z")))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        match pick(Role::Executor, &cands(&qs), On5hLimit::Wait, None, now()) {
            Pick::Wait { identity, until, .. } => {
                assert_eq!(identity, "cc2");
                assert_eq!(until.as_deref(), Some("2026-09-13T15:00:00Z"));
            }
            other => panic!("expected Wait, got {other:?}"),
        }
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Switch, None, now())), ("cc1", None));
    }

    #[test]
    fn an_expired_limit_hit_no_longer_blocks() {
        let qs = [("cc2", Some(hit(q(10.0, 10.0, Some(10.0)), Some("2026-09-13T11:00:00Z"))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Switch, None, now())), ("cc2", None));
    }

    #[test]
    fn a_hit_with_full_buckets_counts_as_the_week_and_switches() {
        // credits 用完但桶子是滿的：等不到，只能換。
        let qs = [("cc2", Some(hit(q(0.0, 0.0, Some(0.0)), None))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Wait, None, now())), ("cc1", None));
    }

    #[test]
    fn an_exhausted_fable_bucket_keeps_the_identity_on_opus() {
        let qs = [("cc2", Some(q(10.0, 10.0, Some(99.0)))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Executor, &cands(&qs), On5hLimit::Switch, None, now())), ("cc2", Some("opus")));
    }

    #[test]
    fn disabled_identities_are_skipped_and_unknown_quota_is_usable_for_workers() {
        let qs = [("cc2", Some(q(0.0, 0.0, Some(0.0)))), ("cc1", None)];
        let mut cs = cands(&qs);
        cs[0].disabled = true;
        assert_eq!(used(&pick(Role::Executor, &cs, On5hLimit::Switch, None, now())), ("cc1", None));
    }

    #[test]
    fn all_identities_exhausted_waits_for_the_earliest_reset() {
        let mut a = q(10.0, 99.0, Some(10.0));
        a.seven_day = w(99.0, "2026-09-18T06:00:00Z");
        let mut b = q(10.0, 99.0, Some(10.0));
        b.seven_day = w(99.0, "2026-09-14T04:00:00Z");
        let qs = [("cc2", Some(a)), ("cc1", Some(b))];
        match pick(Role::Executor, &cands(&qs), On5hLimit::Switch, None, now()) {
            Pick::Wait { identity, until, .. } => {
                assert_eq!(identity, "cc1");
                assert_eq!(until.as_deref(), Some("2026-09-14T04:00:00Z"));
            }
            other => panic!("expected Wait, got {other:?}"),
        }
    }

    #[test]
    fn the_reviewer_must_be_a_different_identity() {
        let qs = [("cc2", Some(q(0.0, 0.0, Some(0.0)))), ("cc1", Some(q(0.0, 0.0, Some(0.0))))];
        assert_eq!(used(&pick(Role::Reviewer, &cands(&qs), On5hLimit::Switch, Some("cc2"), now())), ("cc1", None));
        let only = [("cc2", Some(q(0.0, 0.0, Some(0.0))))];
        assert!(matches!(
            pick(Role::Reviewer, &cands(&only), On5hLimit::Switch, Some("cc2"), now()),
            Pick::NoIndependentReviewer { .. }
        ));
    }

    #[test]
    fn the_verifier_needs_fable_quota_and_asks_the_user_otherwise() {
        let qs = [("cc2", Some(q(0.0, 0.0, Some(100.0)))), ("cc1", Some(q(0.0, 0.0, Some(40.0))))];
        assert_eq!(used(&pick(Role::Verifier, &cands(&qs), On5hLimit::Switch, None, now())), ("cc1", Some("fable")));

        let none = [("cc2", Some(q(0.0, 0.0, Some(100.0)))), ("cc1", Some(q(0.0, 0.0, None))), ("cc0", None)];
        match pick(Role::Verifier, &cands(&none), On5hLimit::Switch, None, now()) {
            Pick::AskUser { resets, .. } => {
                let names: Vec<_> = resets.iter().map(|r| r.identity.as_str()).collect();
                assert_eq!(names, ["cc2", "cc1", "cc0"]);
                assert_eq!(resets[0].resets_at.as_deref(), Some("2026-09-18T06:00:00Z"));
            }
            other => panic!("expected AskUser, got {other:?}"),
        }
    }

    #[test]
    fn a_verifier_on_a_five_hour_hit_waits_only_when_the_mission_chose_to() {
        let qs = [("cc2", Some(hit(q(100.0, 10.0, Some(10.0)), Some("2026-09-13T15:00:00Z"))))];
        assert!(matches!(pick(Role::Verifier, &cands(&qs), On5hLimit::Wait, None, now()), Pick::Wait { .. }));
        // `switch` 時先看別的身分；只有這一個而且它的 Fable 還有額度，就等 5h 重置——
        // 不是 D6 的「沒有 Fable 額度」（review3 c1 L12）。
        match pick(Role::Verifier, &cands(&qs), On5hLimit::Switch, None, now()) {
            Pick::Wait { identity, until, .. } => {
                assert_eq!(identity, "cc2");
                assert_eq!(until.as_deref(), Some("2026-09-13T15:00:00Z"), "5h 的重置時間");
            }
            other => panic!("expected Wait, got {other:?}"),
        }
    }
}

/// 任務裡的一件交辦撞到額度時，controller 要怎麼做（`supervisor/controller.rs` 的 `park_quota`）。
#[derive(Debug, Clone, PartialEq)]
pub enum QuotaPolicy {
    /// 照 supervisor 原本的規則停在 `quota_blocked`，額度回來自己重送。
    Wait,
    /// 換手：交給 AGM 用這個身分（或模型）開 followup 接手。
    Switch { identity: String, model: Option<String>, reason: String },
    /// 停下來問使用者（驗證者找不到 Fable 有效額度）。
    AskUser { reason: String },
}

/// 把 [`pick`] 的結果對上「這顆 bot 現在是哪個身分、哪個模型」。
///
/// 同一個身分、同一個模型還被挑中 → 額度讀數可能比 CLI 慢一步，照原本的規則等；
/// 挑到別的身分，或同一身分但要換模型（Fable 用盡改 opus）→ 換手。
pub fn quota_policy(current_identity: &str, current_model: Option<&str>, decision: &Pick) -> QuotaPolicy {
    match decision {
        Pick::Use { identity, model, reason } => {
            let other_identity = identity != current_identity;
            let other_model = model.as_deref().is_some_and(|m| Some(m) != current_model);
            if other_identity || other_model {
                QuotaPolicy::Switch { identity: identity.clone(), model: model.clone(), reason: reason.clone() }
            } else {
                QuotaPolicy::Wait
            }
        }
        Pick::AskUser { reason, .. } => QuotaPolicy::AskUser { reason: reason.clone() },
        Pick::Wait { .. } | Pick::NoIndependentReviewer { .. } => QuotaPolicy::Wait,
    }
}

#[cfg(test)]
mod quota_policy_tests {
    use super::*;

    fn use_(identity: &str, model: Option<&str>) -> Pick {
        Pick::Use { identity: identity.into(), model: model.map(String::from), reason: "r".into() }
    }

    #[test]
    fn a_different_identity_or_model_is_a_switch_and_the_same_one_waits() {
        assert!(matches!(quota_policy("cc2", Some("fable"), &use_("cc1", None)), QuotaPolicy::Switch { .. }));
        assert!(matches!(quota_policy("cc2", Some("fable"), &use_("cc2", Some("opus"))), QuotaPolicy::Switch { .. }));
        assert_eq!(quota_policy("cc2", Some("opus"), &use_("cc2", Some("opus"))), QuotaPolicy::Wait);
        assert_eq!(quota_policy("cc2", None, &use_("cc2", None)), QuotaPolicy::Wait);
        let wait = Pick::Wait { identity: "cc2".into(), until: None, reason: "5h".into() };
        assert_eq!(quota_policy("cc2", None, &wait), QuotaPolicy::Wait);
        let ask = Pick::AskUser { reason: "no fable".into(), resets: vec![] };
        assert!(matches!(quota_policy("cc1", Some("fable"), &ask), QuotaPolicy::AskUser { .. }));
    }
}

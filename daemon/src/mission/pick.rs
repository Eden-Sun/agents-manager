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
    /// 只有 Fable 週桶用盡：opus 還能跑。
    FableOnly(Option<String>),
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

/// `limit_hit` 本身不說是哪個桶，用當下的桶子讀數推：5h 見底就是 5h，否則週窗見底就是週窗；
/// 都看不出來（例如 credits 用完、桶子卻是滿的）就當週窗——等不到，只能換身分。
fn block_of(q: &Quota, now: DateTime<Utc>) -> Block {
    if let Some(hit) = q.limit_hit.as_ref().filter(|h| hit_active(h, now)) {
        // `hit.until` 是**橫幅那一桶**的重置時間，只有在算出來的封鎖是同一桶時才能拿來用。
        // 以前一律拿它：撞 Fable 上限而 5h 剛好也快滿時，「5 小時窗什麼時候回來」會變成下週一，
        // 整個身分被鎖到下週（review 2026-09-16）。桶名對不上就用那個視窗自己的 resets_at。
        let until_for = |bucket: &str, own: Option<String>| match hit.bucket.as_deref() {
            Some(b) if b == bucket => hit.until.clone().or(own),
            // 舊資料沒有 bucket：維持舊行為（拿 hit.until），否則升級後反而少了時間。
            None => hit.until.clone().or(own),
            Some(_) => own,
        };
        if exhausted(q.five_hour.as_ref()) && !exhausted(q.seven_day.as_ref()) {
            return Block::FiveHour(until_for("five_hour", reset_of(q.five_hour.as_ref())));
        }
        if exhausted(q.fable.as_ref()) && !exhausted(q.seven_day.as_ref()) && !exhausted(q.five_hour.as_ref()) {
            return Block::FableOnly(until_for("fable", reset_of(q.fable.as_ref())));
        }
        return Block::SevenDay(until_for("seven_day", reset_of(q.seven_day.as_ref())));
    }
    if exhausted(q.seven_day.as_ref()) {
        return Block::SevenDay(reset_of(q.seven_day.as_ref()));
    }
    if exhausted(q.five_hour.as_ref()) {
        return Block::FiveHour(reset_of(q.five_hour.as_ref()));
    }
    if exhausted(q.fable.as_ref()) {
        return Block::FableOnly(reset_of(q.fable.as_ref()));
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
            Block::FableOnly(_) => {
                return Pick::Use {
                    identity: c.name.into(),
                    model: Some("opus".into()),
                    reason: "Fable 週桶已用盡，同一身分改用 opus".into(),
                };
            }
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
            Block::FiveHour(until) if fable_ok && on_5h == On5hLimit::Wait => {
                return Pick::Wait { identity: c.name.into(), until, reason: "驗證者的 5 小時窗撞限，依任務設定原地等重置".into() };
            }
            _ => resets.push(Reset { identity: c.name.into(), resets_at: reset_of(q.fable.as_ref()) }),
        }
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
        // 橫幅就是這一桶：照它的。
        let mut q4 = five_reset("2026-09-16T14:00:00Z");
        q4.limit_hit = Some(crate::quota::LimitHit { bucket: Some("five_hour".into()), ..hit(None) });
        assert_eq!(block_of(&q4, now), Block::FiveHour(Some("2026-09-23T00:00:00Z".into())), "橫幅說的就是 5h");
        // 舊資料沒有 bucket：維持舊行為。
        let mut q5 = five_reset("2026-09-16T14:00:00Z");
        q5.limit_hit = Some(hit(None));
        assert_eq!(block_of(&q5, now), Block::FiveHour(Some("2026-09-23T00:00:00Z".into())), "沒有 bucket 時照舊");
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
        assert!(matches!(pick(Role::Verifier, &cands(&qs), On5hLimit::Switch, None, now()), Pick::AskUser { .. }));
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

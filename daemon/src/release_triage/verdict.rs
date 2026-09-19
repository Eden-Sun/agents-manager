//! 第二層的輸出被框死（issue #204 §2、§4）：模型對每一條 kept／unmatched entry 交回一個 verdict，
//! 要開 issue 的另外交提案。這裡只做**驗證**——不合格整份退回，不半收。
//!
//! 模型交回來的文字（`title`／`goal`／`suggestion`／`acceptance`）只進 issue 的對應段落；
//! `## 來源` 的引用一律由 daemon 從帳本原文貼（`issue::render`），模型的字不會進引用。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Bucket, Entry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// 不處理會壞／行為會變。
    #[serde(rename = "guard")]
    Guard,
    /// 處理了會更好。
    #[serde(rename = "adopt")]
    Adopt,
    /// 只是「值得早點升級」的理由，不用改程式；永不開 issue。
    #[serde(rename = "upgrade-arg")]
    UpgradeArg,
    #[serde(rename = "none")]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryVerdict {
    pub entry_id: String,
    pub verdict: Verdict,
    /// 一句理由。
    #[serde(default)]
    pub reason: String,
    /// 對到我們哪個模組／哪個檔；找不到對應就 `none` 並照實寫理由。
    #[serde(default)]
    pub module: String,
}

/// 模型要開的一張 issue（可合併同一原因的多條 entry）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub entry_ids: Vec<String>,
    /// 選填；有給就必須跟 entry verdict 推得出的一致（含任何 guard ＝ guard，否則 adopt）。
    #[serde(default)]
    pub verdict: Option<Verdict>,
    /// 一句話（daemon 補上 `<kind> <version>: ` 前綴與（提防｜採用）後綴）。
    pub title: String,
    pub goal: String,
    pub suggestion: String,
    pub acceptance: String,
    /// 跨版重複：只在那張 issue 留言，不另開。
    #[serde(default)]
    pub duplicate_of: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Submission {
    pub kind: String,
    pub version: String,
    pub verdicts: Vec<EntryVerdict>,
    #[serde(default)]
    pub issues: Vec<Proposal>,
}

/// 通過驗證、帶著推得出的 issue 類別，存進 `verdicts_json` 的形狀。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredProposal {
    pub entry_ids: Vec<String>,
    /// `guard`｜`adopt`
    pub triage: String,
    pub title: String,
    pub goal: String,
    pub suggestion: String,
    pub acceptance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<i64>,
}

const MAX_TITLE: usize = 160;
const MAX_TEXT: usize = 4000;
const MARKER_PREFIX: &str = "release-triage:";

fn check_text(field: &str, s: &str, max: usize, single_line: bool, errs: &mut Vec<String>) {
    let t = s.trim();
    if t.is_empty() {
        errs.push(format!("issue 提案的 {field} 不能是空的"));
    } else if t.chars().count() > max {
        errs.push(format!("issue 提案的 {field} 超過 {max} 字"));
    }
    if single_line && t.contains('\n') {
        errs.push(format!("issue 提案的 {field} 只能一行"));
    }
    // 隱藏標記是去重的鍵，只有 daemon 能寫。
    if t.contains("<!--") || t.contains(MARKER_PREFIX) {
        errs.push(format!("issue 提案的 {field} 不能含 `<!--` 或 `{MARKER_PREFIX}`（去重標記由 daemon 產生）"));
    }
}

/// 驗證整份提交。回傳逐條 verdict（照 entry 順序）與轉好的提案；不合格回所有問題（一次列完，省得來回退）。
pub fn validate(sub: &Submission, entries: &[Entry]) -> Result<(Vec<EntryVerdict>, Vec<StoredProposal>), Vec<String>> {
    let mut errs: Vec<String> = Vec::new();
    let judged: BTreeSet<&str> = entries.iter().filter(|e| e.bucket != Bucket::Dropped).map(|e| e.id.as_str()).collect();
    let mut by_id: BTreeMap<&str, &EntryVerdict> = BTreeMap::new();
    for v in &sub.verdicts {
        if !judged.contains(v.entry_id.as_str()) {
            errs.push(format!("verdict 的 entry_id `{}` 不是這一版的 kept／unmatched entry", v.entry_id));
        } else if by_id.insert(v.entry_id.as_str(), v).is_some() {
            errs.push(format!("entry `{}` 有多個 verdict", v.entry_id));
        }
    }
    let missing: Vec<&str> = judged.iter().copied().filter(|id| !by_id.contains_key(id)).collect();
    if !missing.is_empty() {
        errs.push(format!("這些 kept／unmatched entry 沒有 verdict：{}", missing.join("、")));
    }

    let mut used: BTreeSet<&str> = BTreeSet::new();
    let mut out: Vec<StoredProposal> = Vec::new();
    for (i, p) in sub.issues.iter().enumerate() {
        let n = i + 1;
        if p.entry_ids.is_empty() {
            errs.push(format!("issue 提案 {n} 沒有 entry_ids"));
            continue;
        }
        let mut any_guard = false;
        let mut ok = true;
        for id in &p.entry_ids {
            match by_id.get(id.as_str()) {
                Some(v) if matches!(v.verdict, Verdict::Guard | Verdict::Adopt) => any_guard |= v.verdict == Verdict::Guard,
                Some(_) => {
                    errs.push(format!("issue 提案 {n} 引用的 entry `{id}` verdict 不是 guard／adopt（只有這兩種會變成 issue）"));
                    ok = false;
                }
                None => {
                    errs.push(format!("issue 提案 {n} 引用了不存在的 entry `{id}`"));
                    ok = false;
                }
            }
            if !used.insert(id.as_str()) {
                errs.push(format!("entry `{id}` 出現在多個 issue 提案（同一原因請合併成一張）"));
                ok = false;
            }
        }
        let triage = if any_guard { Verdict::Guard } else { Verdict::Adopt };
        if let Some(v) = p.verdict {
            if v != triage {
                errs.push(format!("issue 提案 {n} 的 verdict 與 entry verdict 不一致（應為 {}）", triage_str(triage)));
                ok = false;
            }
        }
        if let Some(d) = p.duplicate_of {
            if d <= 0 {
                errs.push(format!("issue 提案 {n} 的 duplicate_of 必須是正整數"));
                ok = false;
            }
        }
        check_text("title", &p.title, MAX_TITLE, true, &mut errs);
        check_text("goal", &p.goal, MAX_TEXT, false, &mut errs);
        check_text("suggestion", &p.suggestion, MAX_TEXT, false, &mut errs);
        check_text("acceptance", &p.acceptance, MAX_TEXT, false, &mut errs);
        if ok {
            out.push(StoredProposal {
                entry_ids: p.entry_ids.clone(),
                triage: triage_str(triage).to_string(),
                title: p.title.trim().to_string(),
                goal: p.goal.trim().to_string(),
                suggestion: p.suggestion.trim().to_string(),
                acceptance: p.acceptance.trim().to_string(),
                duplicate_of: p.duplicate_of,
            });
        }
    }
    if !errs.is_empty() {
        return Err(errs);
    }
    let ordered = entries.iter().filter_map(|e| by_id.get(e.id.as_str()).map(|v| (*v).clone())).collect();
    Ok((ordered, out))
}

fn triage_str(v: Verdict) -> &'static str {
    if v == Verdict::Guard {
        "guard"
    } else {
        "adopt"
    }
}

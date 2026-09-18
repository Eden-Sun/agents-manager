//! herdr 有新版時，整理出對我們有沒有用、會不會壞，交給 AGM 排程處理（issue #66）。
//!
//! 這裡**只做偵測與整理**，不碰網路、不執行任何指令，也不會真的觸發升級：真的升級／重啟
//! herdr server 一律留給 AGM 核准後手動做。2026-09-17 herdr 0.8.2 → 0.9.0 那次升級失敗自動
//! 回滾，過程中所有 pane 被結束；頂層 bot 接得回，但五顆子 agent 沒有 `native_session_id`／
//! 走的是父 agent 開 pane 的路，維護窗口一過就被軟刪，事後靠人工一顆一顆 restore +
//! `claude --resume` 救回來——這正是「升級會影響什麼」要先講清楚、而不是只偵測「有新版」的原因。
//!
//! 版本比對複用 [`crate::changelog::parse_version`]（`457dd14`：字串比較會把 `0.9.0` 判成比
//! `0.10.0` 新）；CHANGELOG 段落擷取複用 [`crate::changelog::parse_changelog`] / `pick_sections`。
//! 抓 GitHub release／Homebrew、真的把交辦寫進 AGM inbox 的腳本（`scripts/ops/herdr-update-kick.sh`）
//! 不在這次範圍：外層腳本只管拿到「本機版本」「最新穩定版」「CHANGELOG 全文」三個字串，版本比較
//! 與段落擷取一律交給這裡，不要在 bash 裡重做一次（同樣會踩 `9` vs `10` 的坑）。

use crate::changelog::{self, Section};
use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct HerdrUpdateReport {
    pub installed_version: String,
    pub latest_version: String,
    /// `latest_version` 真的比 `installed_version` 新——不是同版，也不是探到比較舊的（例如
    /// Homebrew 還沒同步、或帶了預發布 tag）。
    pub has_update: bool,
    /// `installed_version`（不含）到 `latest_version`（含）之間的段落，新的在前；沒有新版或抓不到
    /// 段落時是空的。
    pub sections: Vec<Section>,
}

/// 純函式：`herdr --version` 讀到的本機版本、GitHub release／Homebrew 查到的最新穩定版、herdr
/// 的 CHANGELOG 全文，整理成一份報告。三個輸入都是呼叫端已經拿到手的字串，這裡不碰網路、不重試。
/// 任一版本號解析不出來就回 `None`——版本探測本身失敗跟「沒有更新」是兩件事，呼叫端要分開處理，
/// 不能把「看不懂版本」當成「已是最新」悄悄吞掉。
pub fn build_report(installed: &str, latest: &str, changelog_md: &str) -> Option<HerdrUpdateReport> {
    let installed_version = changelog::version_string(installed)?;
    let latest_version = changelog::version_string(latest)?;
    let has_update = changelog::parse_version(&latest_version) > changelog::parse_version(&installed_version);
    let sections = if has_update {
        let all = changelog::parse_changelog(changelog_md);
        changelog::pick_sections(&all, Some(&installed_version), &latest_version)
    } else {
        Vec::new()
    };
    Some(HerdrUpdateReport { installed_version, latest_version, has_update, sections })
}

/// 同一版不重複派工：`last_notified` 是上次真的派過工的那個版本（腳本記在
/// `supervisor/AGM/herdr-update.last` 的那個字串）。沒有更新、或這一版已經派過，都不用再派。
pub fn should_notify(report: &HerdrUpdateReport, last_notified: Option<&str>) -> bool {
    report.has_update && last_notified != Some(report.latest_version.as_str())
}

/// 交給 AGM 的交辦內文：版本差異＋原始 CHANGELOG 段落。「哪些條目對我們有用／可能弄壞什麼」
/// 刻意不在這裡判斷——那要讀懂 changelog 的敘述內容，是 AGM 建置 child 的活；這裡只保證版本
/// 沒比錯、段落沒抓漏。呼叫端只在 `report.has_update` 時才需要送這份交辦。
pub fn render_agm_brief(report: &HerdrUpdateReport) -> String {
    let mut out = format!(
        "herdr 有新版：{} → {}（本機／最新穩定版）。\n\n\
         請判斷並回報：\n\
         - 哪些條目對 agents-manager 有用（對到 daemon 的哪個模組、目前繞路的哪一段可以拿掉）\n\
         - 哪些可能弄壞現有整合（API 變更、拿掉的功能、需不需要停 server）\n\
         - 哪些繞路修不到、仍要保留\n\n\
         驗證通過才向我申請升級窗口；升級與重啟 herdr server 一律要核准，不自動執行。\n\n",
        report.installed_version, report.latest_version
    );
    if report.sections.is_empty() {
        out.push_str("（沒有抓到對應版本範圍的 CHANGELOG 段落，附件請自己核對原始 CHANGELOG）\n");
    } else {
        for s in &report.sections {
            out.push_str(&format!("## {}\n\n{}\n\n", s.version, s.body));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MD: &str = "# Changelog\n\n\
        ## 0.9.0\n\n- agent prompt 確實送出才回成功\n- recent pane read 不再回空\n\n\
        ## 0.8.3\n\n- 小修\n\n\
        ## 0.8.2\n\n- 基準版\n";

    /// herdr 實際的 CHANGELOG 格式（`gh api repos/herdrdev/herdr/contents/CHANGELOG.md` 2026-09-18
    /// 驗過）：Keep a Changelog 的 `## [x.y.z] - date`，帶 `## Unreleased`。跟上面那份 Claude Code
    /// 格式的 `MD` 不一樣，兩種都要能被 `build_report` 吃下去。
    const HERDR_MD: &str = "# Changelog\n\n## Unreleased\n\n\
        ## [0.9.1] - 2026-09-16\n\n### Added\n- machine 遠端指令轉發\n\n\
        ## [0.9.0] - 2026-09-07\n\n### Changed\n- endpoint generation 1，升級要停一次 server\n\n\
        ## [0.8.2] - 2026-08-01\n\n- 基準版\n";

    #[test]
    fn build_report_parses_herdrs_real_bracket_and_date_changelog_format() {
        let r = build_report("0.8.2", "0.9.1", HERDR_MD).unwrap();
        assert!(r.has_update);
        assert_eq!(r.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.1", "0.9.0"]);
        assert!(r.sections[1].body.contains("endpoint generation 1"));
    }

    #[test]
    fn a_newer_stable_release_has_an_update_with_the_range_between() {
        let r = build_report("0.8.2", "0.9.0", MD).unwrap();
        assert!(r.has_update);
        assert_eq!(r.installed_version, "0.8.2");
        assert_eq!(r.latest_version, "0.9.0");
        assert_eq!(r.sections.iter().map(|s| s.version.as_str()).collect::<Vec<_>>(), ["0.9.0", "0.8.3"]);
    }

    #[test]
    fn the_same_version_is_not_an_update() {
        let r = build_report("0.9.0", "0.9.0", MD).unwrap();
        assert!(!r.has_update);
        assert!(r.sections.is_empty());
    }

    /// 探到的「最新版」其實比本機舊（Homebrew 落後、或探錯了）：不算有更新，不要叫人去升級。
    #[test]
    fn a_stale_or_downgraded_latest_is_not_an_update() {
        let r = build_report("0.9.0", "0.8.2", MD).unwrap();
        assert!(!r.has_update);
        assert!(r.sections.is_empty());
    }

    /// 位數不同也要比得對：`0.9.9` 不能被字串比較誤判成比 `0.9.10`新（457dd14 那個坑）。
    #[test]
    fn versions_compare_numerically_not_as_strings() {
        let r = build_report("0.9.9", "0.9.10", "## 0.9.10\n\nfix\n\n## 0.9.9\n\nold\n").unwrap();
        assert!(r.has_update, "0.9.10 > 0.9.9 用數值比較才看得出來");
        let r2 = build_report("0.9.10", "0.9.9", "## 0.9.10\n\nfix\n\n## 0.9.9\n\nold\n").unwrap();
        assert!(!r2.has_update, "0.9.9 < 0.9.10，不是升級");
    }

    #[test]
    fn an_unparsable_version_is_none_not_a_silent_no_update() {
        assert!(build_report("", "0.9.0", MD).is_none());
        assert!(build_report("0.8.2", "not-a-version", MD).is_none());
    }

    #[test]
    fn should_notify_is_gated_by_update_and_dedup() {
        let no_update = build_report("0.9.0", "0.9.0", MD).unwrap();
        assert!(!should_notify(&no_update, None), "沒有更新不用派工");

        let update = build_report("0.8.2", "0.9.0", MD).unwrap();
        assert!(should_notify(&update, None), "有更新、還沒派過");
        assert!(!should_notify(&update, Some("0.9.0")), "這一版已經派過，不重派");
        assert!(should_notify(&update, Some("0.8.9")), "上次派的是別的（更早的）版本，這版還是要派");
    }

    #[test]
    fn the_agm_brief_carries_both_versions_and_the_raw_sections() {
        let r = build_report("0.8.2", "0.9.0", MD).unwrap();
        let brief = render_agm_brief(&r);
        assert!(brief.contains("0.8.2") && brief.contains("0.9.0"), "{brief}");
        assert!(brief.contains("agent prompt 確實送出才回成功"), "原始段落要帶進去，不能只給版本號：{brief}");
        assert!(brief.contains("核准") && brief.contains("不自動執行"), "不能漏掉『升級要核准』這句：{brief}");

        let no_sections = HerdrUpdateReport { installed_version: "0.8.2".into(), latest_version: "0.9.0".into(), has_update: true, sections: vec![] };
        assert!(render_agm_brief(&no_sections).contains("沒有抓到"), "抓不到段落要講清楚，不能留白裝作沒事");
    }
}

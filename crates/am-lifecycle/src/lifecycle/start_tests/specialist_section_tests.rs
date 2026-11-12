use super::{advisor_role_section, manager_role_section, specialist_role_section};

#[test]
fn only_a_specialist_with_a_track_gets_the_scope_section() {
    let section = specialist_role_section(Some("specialist"), Some("infra")).expect("specialist 有範圍段");
    assert!(section.contains("你是本專案 infra 的專家，範圍限於 infra；超出範圍的工作回報管理者，不要自己做。"), "{section}");
    assert!(section.contains("「專家可直接派工給執行者」開關開著時") && section.contains("開關關著就經管理者派工") && !section.contains("agm grant"), "{section}");
    assert!(specialist_role_section(Some("executor"), Some("infra")).is_none());
    assert!(specialist_role_section(None, Some("infra")).is_none());
    assert!(specialist_role_section(Some("specialist"), Some("  ")).is_none());
}

#[test]
fn deputy_section_only_for_manager_and_the_deputy_when_one_is_set() {
    use super::deputy_role_section;
    assert!(deputy_role_section("m", Some("manager"), None).is_none(), "沒設副管理者就不加");
    assert!(deputy_role_section("d", Some("advisor"), None).is_none());
    let m = deputy_role_section("m", Some("manager"), Some("d")).expect("管理者知道有副管理者");
    assert!(m.contains("收到他處理過哪些的摘要"), "{m}");
    let d = deputy_role_section("d", Some("advisor"), Some("d")).expect("副管理者本人");
    assert!(d.contains("不能增聘") && d.contains("留給管理者"), "{d}");
    assert!(deputy_role_section("x", Some("executor"), Some("d")).is_none(), "其他成員不變");
}

#[test]
fn manager_role_section_only_present_for_manager() {
    let manager_sec = manager_role_section(Some("manager")).expect("manager 有專屬段");
    assert!(manager_sec.contains("你是管理者：不親自實作或做長任務，你的回合要短——收到工作就用 `agm peers --for <track>` 挑人、`--expect-result` 派出去後立刻結束回合，結果會以通知送回再整合；你在回合中時，外部請求與成員求助都只能排隊等你。"), "{manager_sec}");
    assert!(manager_sec.contains("不確定給誰時用 `bot:auto:cheap|standard|strong`"), "{manager_sec}");
    assert!(manager_sec.contains("「專家可直接派工給執行者」開關") && !manager_sec.contains("agm grant"), "{manager_sec}");
    assert!(manager_sec.contains("claude-api 顧問（`agm peers` 標 billing=api）用 API 額度"), "{manager_sec}");
    assert!(manager_sec.contains("其他 claude-api 成員不能派工"), "{manager_sec}");

    assert!(manager_role_section(Some("executor")).is_none());
    assert!(manager_role_section(Some("advisor")).is_none());
    assert!(manager_role_section(Some("specialist")).is_none());
    assert!(manager_role_section(None).is_none());
    assert!(manager_role_section(Some("other")).is_none());
}

#[test]
fn no_bg_end_rule_mentions_wait_log() {
    assert!(super::NO_BG_END_RULE.contains("scripts/wait-log.py LOG --exit-prefix CHECK_RC= --timeout 540"));
    assert!(super::NO_BG_END_RULE.contains("回合結束就等於回報"));
}

#[test]
fn advisor_role_section_only_present_for_advisor() {
    let advisor_sec = advisor_role_section(Some("advisor")).expect("advisor 有啟動指示");
    assert!(advisor_sec.contains("你是本專案的顧問：不實作、不改檔、不 commit；接受任何成員的諮詢，負責回答問題、審查方案或 diff、比較設計取捨並給建議；需要動手的工作回報管理者派給執行者或專家。"), "{advisor_sec}");
    assert!(!advisor_sec.contains("bot ID") && !advisor_sec.contains("<id>"), "顧問指示不帶 bot 專屬名稱或 id：{advisor_sec}");

    assert!(advisor_role_section(Some("manager")).is_none());
    assert!(advisor_role_section(Some("executor")).is_none());
    assert!(advisor_role_section(Some("specialist")).is_none());
    assert!(advisor_role_section(None).is_none());
    assert!(advisor_role_section(Some("other")).is_none());
}

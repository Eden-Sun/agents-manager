
    use super::*;

    /// claude 2.1.289 實機：ALPHA、BRAVO、CHARLIE 三則，`/rewind` 倒回 BRAVO 之前，什麼都沒送就 `/exit`。
    const REAL: &str = include_str!("claude_2.1.289_rewind_then_exit.jsonl");
    const SID: &str = "66c4bdfb-5480-4180-a8ff-cb7c591a4e4e";
    /// BRAVO 的 parentUuid（ALPHA 那一回合最後的 `turn_duration`）。
    const BEFORE_BRAVO: &str = "22bbc7ad-b205-4719-b121-42f6a4def0dd";

    #[test]
    fn the_real_rewind_leaves_nothing_in_the_transcript() {
        // 結束時補的是舊分支的尾巴，沒有錨點：這就是 bug 的樣子。
        let last = REAL.lines().rev().find(|l| l.contains("\"last-prompt\"")).unwrap();
        assert!(last.contains("88a06305") && !last.contains("explicit"), "{last}");
    }

    #[test]
    fn the_rewound_prompt_is_found_and_its_parent_is_the_rewind_point() {
        assert_eq!(find(REAL, "Say only: BRAVO", 0), Some(Some(BEFORE_BRAVO.into())));
        assert_eq!(find(REAL, "  Say  only:  BRAVO\n", 0), Some(Some(BEFORE_BRAVO.into())), "空白不算差異");
        assert_eq!(find(REAL, "Say only: ALPHA", 0), Some(None), "第一則：倒回空對話");
        assert_eq!(find(REAL, "Say only: DELTA", 0), None, "不在 transcript 裡");
        assert_eq!(find(REAL, "Say only: BRAVO", 1), None, "沒有更舊的同一句");
    }

    #[test]
    fn a_prompt_whose_first_line_matches_but_the_rest_does_not_is_not_guessed() {
        assert_eq!(find(REAL, "Say only: BRAVO\nand more", 0), None);
    }

    #[test]
    fn the_same_words_twice_skip_the_newer_one_like_the_menu() {
        let j = r#"{"type":"user","uuid":"u1","parentUuid":null,"message":{"role":"user","content":"again"}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"OK"}]}}
{"type":"user","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":"again"}}
{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"role":"assistant","content":[{"type":"text","text":"OK"}]}}
"#;
        assert_eq!(find(j, "again", 0), Some(Some("a1".into())));
        assert_eq!(find(j, "again", 1), Some(None));
    }

    /// 走的是目前那條鏈：被倒掉的舊分支（錨點之後）上的訊息找不到；錨點之後長出的新分支找得到。
    #[test]
    fn only_the_live_branch_is_walked() {
        let j = format!(
            "{REAL}{}\n{}\n",
            line(SID, &Some(BEFORE_BRAVO.into())),
            r#"{"type":"user","uuid":"d1","parentUuid":"22bbc7ad-b205-4719-b121-42f6a4def0dd","message":{"role":"user","content":"Say only: DELTA"}}"#
        );
        assert_eq!(find(&j, "Say only: CHARLIE", 0), None, "舊分支");
        assert_eq!(find(&j, "Say only: DELTA", 0), Some(Some(BEFORE_BRAVO.into())));
        let only_anchor = format!("{REAL}{}\n", line(SID, &Some(BEFORE_BRAVO.into())));
        assert_eq!(find(&only_anchor, "Say only: BRAVO", 0), None, "已經倒掉的不在鏈上");
        assert_eq!(find(&only_anchor, "Say only: ALPHA", 0), Some(None));
    }

    #[test]
    fn pasted_and_tool_rows_are_read_like_the_cli_wrote_them() {
        let j = r#"{"type":"user","uuid":"u1","parentUuid":"p0","message":{"role":"user","content":"\n\n<pasted_content id=\"c4ab\">\nhello there friend\n</pasted_content id=\"c4ab\">\n"}}
{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use"}]}}
{"type":"user","uuid":"t1","parentUuid":"a1","message":{"role":"user","content":[{"type":"tool_result","content":"hello there friend"}]}}
{"type":"user","uuid":"m1","parentUuid":"t1","isMeta":true,"message":{"role":"user","content":"hello there friend"}}
"#;
        assert_eq!(find(j, "hello there friend", 0), Some(Some("p0".into())), "tool_result／meta 不是使用者訊息");
    }

    #[test]
    fn the_anchor_is_needed_until_it_is_written_or_a_new_turn_lands() {
        let leaf: Leaf = Some(BEFORE_BRAVO.into());
        // 倒回之後的東西：CLI 結束時寫的那幾行（實機）。
        let exit = r#"{"type":"file-history-snapshot","messageId":"x"}
{"type":"cost-state"}
{"type":"last-prompt","leafUuid":"88a06305-846c-43c9-b185-00db02270335"}
"#;
        assert_eq!(need(exit, &leaf), Need::Append);
        let written = format!("{exit}{}\n", line(SID, &leaf));
        assert_eq!(need(&written, &leaf), Need::AlreadyThere);
        assert_eq!(need(&format!("{written}{{\"type\":\"cost-state\"}}\n"), &leaf), Need::AlreadyThere, "resume 後什麼都沒送就結束");
        // CLI 還開著時補的會被它結束時那行舊 leaf 蓋掉（實機）：要再補。
        let overridden = format!("{written}{{\"type\":\"last-prompt\",\"leafUuid\":\"88a06305-846c-43c9-b185-00db02270335\"}}\n");
        assert_eq!(need(&overridden, &leaf), Need::Append);
        let new_turn = format!("{written}{{\"type\":\"user\",\"uuid\":\"d1\",\"parentUuid\":\"{BEFORE_BRAVO}\",\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}\n");
        assert_eq!(need(&new_turn, &leaf), Need::Obsolete);
        let sidechain = r#"{"type":"user","uuid":"s1","isSidechain":true,"message":{"role":"user","content":"x"}}"#;
        assert_eq!(need(sidechain, &leaf), Need::Append, "子代理的列不算新回合");
        // 倒回到空對話：`leafUuid:null` 的錨點。
        let empty = format!("{}\n", line(SID, &None));
        assert!(empty.contains("\"leafUuid\":null"), "{empty}");
        assert_eq!(need(&empty, &None), Need::AlreadyThere);
        assert_eq!(need(&empty, &leaf), Need::Append);
    }

    #[test]
    fn apply_appends_once_and_respects_a_replaced_file() {
        let dir = crate::testing::scratch_dir("rewind-anchor");
        let p = dir.join(format!("{SID}.jsonl"));
        std::fs::write(&p, REAL).unwrap();
        let leaf: Leaf = Some(BEFORE_BRAVO.into());
        assert_eq!(apply(&p, SID, &leaf, REAL.len() as u64).unwrap(), Need::Append);
        assert_eq!(apply(&p, SID, &leaf, REAL.len() as u64).unwrap(), Need::AlreadyThere);
        let after = std::fs::read_to_string(&p).unwrap();
        assert_eq!(after.matches("\"rewound\":true").count(), 1);
        assert!(after.ends_with(&format!("{}\n", line(SID, &leaf))));
        assert_eq!(apply(&p, SID, &leaf, after.len() as u64 + 1).unwrap(), Need::Obsolete, "檔案變短＝被換過");
        // 沒有換行結尾的檔也不會黏在上一行。
        let q = dir.join("no-newline.jsonl");
        std::fs::write(&q, "{\"type\":\"cost-state\"}").unwrap();
        apply(&q, SID, &leaf, 0).unwrap();
        assert_eq!(std::fs::read_to_string(&q).unwrap().lines().count(), 2);
    }

    fn open_rw(p: &std::path::Path) -> std::fs::File {
        std::fs::OpenOptions::new().read(true).append(true).open(p).unwrap()
    }

    /// issue #851：決定「要補」之後、寫之前，來源 CLI 剛好寫進一個新回合：重驗後作廢，錨點不能補在新回合後面。
    #[test]
    fn a_turn_landing_between_scan_and_append_makes_the_anchor_obsolete() {
        use std::io::Write;
        let dir = crate::testing::scratch_dir("rewind-anchor-race");
        let p = dir.join(format!("{SID}.jsonl"));
        std::fs::write(&p, REAL).unwrap();
        let leaf: Leaf = Some(BEFORE_BRAVO.into());
        let f = open_rw(&p);
        let writer = p.clone();
        let out = decide_and_append(&f, &p, SID, &leaf, REAL.len() as u64, || {
            let mut w = std::fs::OpenOptions::new().append(true).open(&writer).unwrap();
            writeln!(w, r#"{{"type":"user","uuid":"n1","parentUuid":"{BEFORE_BRAVO}","message":{{"role":"user","content":"new turn"}}}}"#).unwrap();
            writeln!(w, r#"{{"type":"assistant","uuid":"n2","parentUuid":"n1","message":{{"role":"assistant","content":[{{"type":"text","text":"OK"}}]}}}}"#).unwrap();
        })
        .unwrap();
        assert_eq!(out, Need::Obsolete);
        let after = std::fs::read_to_string(&p).unwrap();
        let last = after.lines().rev().find(|l| l.contains("\"last-prompt\"")).unwrap();
        assert!(!last.contains("\"rewound\":true"), "最後一個 last-prompt 不能是錨點：{last}");
        assert!(!after.contains("\"rewound\":true"), "錨點一行都沒寫");
    }

    /// issue #851：路徑在判斷之後被換成另一個檔：錨點不寫進換進來的檔。
    #[test]
    fn a_replaced_transcript_does_not_receive_the_anchor() {
        let dir = crate::testing::scratch_dir("rewind-anchor-replaced");
        let p = dir.join(format!("{SID}.jsonl"));
        std::fs::write(&p, REAL).unwrap();
        let other = dir.join("other.jsonl");
        let replacement = format!("{REAL}{{\"type\":\"cost-state\"}}\n");
        std::fs::write(&other, &replacement).unwrap();
        let leaf: Leaf = Some(BEFORE_BRAVO.into());
        let f = open_rw(&p);
        let out = decide_and_append(&f, &p, SID, &leaf, REAL.len() as u64, || std::fs::rename(&other, &p).unwrap()).unwrap();
        assert_eq!(out, Need::Obsolete);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), replacement, "替換進來的檔內容不變");
        assert_eq!(std::fs::read_to_string(dir.join("other.jsonl")).ok(), None);
    }

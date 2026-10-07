
    use super::*;

    #[test]
    fn expired_issue_lists_are_dropped_when_a_new_one_is_stored() {
        let mut cache: HashMap<String, (Instant, Value)> = HashMap::new();
        cache.insert("p|o/r|open|30|old query".into(), (Instant::now() - ISSUES_TTL - Duration::from_secs(1), json!({"issues": []})));
        remember_issues(&mut cache, "p|o/r|open|30|new query".into(), json!({"issues": [1]}));
        assert!(!cache.contains_key("p|o/r|open|30|old query"), "過期的那份被清掉");
        assert!(cache.contains_key("p|o/r|open|30|new query"));
    }

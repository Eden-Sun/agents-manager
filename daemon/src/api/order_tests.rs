use super::reorder_by;

fn ids(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn puts_the_named_ones_in_the_given_order() {
    let mut items = ids(&["a", "b", "c"]);
    reorder_by(&mut items, &ids(&["c", "a", "b"]), |x| Some(x.clone()));
    assert_eq!(items, ids(&["c", "a", "b"]));
}

#[test]
fn unnamed_items_keep_their_relative_order_at_the_end() {
    let mut items = ids(&["a", "new1", "b", "new2"]);
    reorder_by(&mut items, &ids(&["b", "a"]), |x| Some(x.clone()));
    assert_eq!(items, ids(&["b", "a", "new1", "new2"]));
}

#[test]
fn items_without_an_id_survive() {
    let mut items = vec![Some("a".to_string()), None, Some("b".to_string())];
    reorder_by(&mut items, &ids(&["b"]), |x| x.clone());
    assert_eq!(items, vec![Some("b".to_string()), Some("a".to_string()), None]);
}

use super::*;

async fn project_with_bot(env: &crate::testing::Env, kind: &str, model: &str) -> String {
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
    let res = create_bot(State(env.app.clone()), Path(env.project_id.clone()), Json(serde_json::from_value(json!({"name": "rp-bot", "kind": kind, "model": model})).unwrap())).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn bot_override_is_user_only_validated_and_projected() {
    let env = crate::testing::env().await;
    let bot = project_with_bot(&env, "claude", "opus").await;
    let model_before = db::bot(&env.app.db, &bot).await.unwrap().unwrap().model;
    let patch = |caller: RequestPrincipal, v: Value| patch_bot(State(env.app.clone()), Path(bot.clone()), Extension(caller), Json(serde_json::from_value::<PatchBot>(v).unwrap()));

    let err = patch(RequestPrincipal::Bot("someone".into()), json!({"runtime_override": {"keep_warm": true}})).await.err().unwrap();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "user_only"), "{err:?}");

    // 超出模型視窗（claude 一律 1M）的覆寫要被擋，連同一起送的模型也不寫。
    let err = patch(RequestPrincipal::User, json!({"model": "claude-x", "runtime_override": {"context_tokens": 1000001}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "context_out_of_range"), "{err:?}");
    assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().model, model_before, "驗證失敗什麼都不寫");

    patch(RequestPrincipal::User, json!({"runtime_override": {"context_tokens": 512000, "keep_warm": false}})).await.map_err(|e| format!("{e:?}")).unwrap();
    let b = db::bot(&env.app.db, &bot).await.unwrap().unwrap();
    let stored: Value = serde_json::from_str(b.runtime_override_json.as_deref().unwrap()).unwrap();
    assert_eq!((stored["context_tokens"].clone(), stored["keep_warm"].clone()), (json!(512000), json!(false)));
    let in_cfg = env.app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|x| x.id.as_deref() == Some(bot.as_str())).and_then(|x| x.runtime_override.clone());
    assert_eq!(in_cfg.as_ref().and_then(|v| v.get("context_tokens")), Some(&json!(512000)), "真相在 config.toml");

    // 清掉＝跟角色。
    patch(RequestPrincipal::User, json!({"runtime_override": null})).await.map_err(|e| format!("{e:?}")).unwrap();
    assert!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().runtime_override_json.is_none());
}

#[tokio::test]
async fn role_policy_values_are_validated_before_anything_is_written() {
    let env = crate::testing::env().await;
    let _bot = project_with_bot(&env, "claude", "opus").await;
    let patch = |v: Value| patch_project(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::User), Json(serde_json::from_value::<PatchProject>(v).unwrap()));

    let err = patch(json!({"role_policy": {"manager": {"keep_warm_after_min": 60}}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_runtime_policy" && v["field"] == "keep_warm_after_min" && v["role"] == "manager"), "{err:?}");
    let err = patch(json!({"role_policy": {"executor": {"context_tokens": 1000}}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["field"] == "context_tokens"), "{err:?}");
    let err = patch(json!({"role_policy": {"manager": {"keep_warm": true}, "advisor": {"bogus": 1}}})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "invalid_runtime_policy"), "{err:?}");
    assert!(db::project(&env.app.db, &env.project_id).await.unwrap().unwrap().role_policy().is_empty(), "整份不寫入");

    patch(json!({"role_policy": {"manager": {"keep_warm": true, "keep_warm_after_min": 30}}})).await.map_err(|e| format!("{e:?}")).unwrap();
    let p = db::project(&env.app.db, &env.project_id).await.unwrap().unwrap();
    assert_eq!(p.role_policy()["manager"]["keep_warm_after_min"], 30);
    assert!(p.role_policy()["manager"].get("context_tokens").is_none(), "存正規化後的形狀（沒設的欄位不寫，TOML 也沒有 null）");
}

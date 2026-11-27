use crate::capabilities::ExePath;
use am_base::ledger::{deliverable_shas, LedgerItem};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

static ANCESTOR: LazyLock<Mutex<HashMap<(String, String), bool>>> = LazyLock::new(Default::default);
static VERIFIED: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Default::default);

pub(crate) fn agm_repo(app: &impl ExePath) -> PathBuf {
    crate::deploy_now::repo_dir(app)
}

pub(crate) fn is_agm_project(app: &impl ExePath, project_path: &str) -> bool {
    same_repo(project_path, &agm_repo(app))
}

pub(crate) fn same_repo(project_path: &str, repo: &Path) -> bool {
    Path::new(project_path.trim_end_matches('/')) == repo
}

async fn verify_commit(repo: &Path, token: &str) -> Option<String> {
    if let Some(hit) = VERIFIED.lock().unwrap_or_else(|e| e.into_inner()).get(token).cloned() {
        return Some(hit);
    }
    let spec = format!("{token}^{{commit}}");
    let out = crate::deploy_now::git(repo, &["rev-parse", "--verify", "--quiet", &spec]).await.ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if full.is_empty() {
        return None;
    }
    VERIFIED.lock().unwrap_or_else(|e| e.into_inner()).insert(token.to_string(), full.clone());
    Some(full)
}

async fn is_ancestor(repo: &Path, sha: &str, live: &str) -> Option<bool> {
    let key = (sha.to_string(), live.to_string());
    if let Some(hit) = ANCESTOR.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied() {
        return Some(hit);
    }
    let out = crate::deploy_now::git(repo, &["merge-base", "--is-ancestor", sha, live]).await.ok()?;
    let v = match out.status.code() {
        Some(0) => true,
        Some(1) => false,
        _ => return None,
    };
    ANCESTOR.lock().unwrap_or_else(|e| e.into_inner()).insert(key, v);
    Some(v)
}

pub(crate) async fn deploy_state(repo: &Path, live: &str, item: &LedgerItem) -> Option<&'static str> {
    if item.status != "answered" {
        return None;
    }
    let shas: Vec<String> = match item.deploy_sha.as_deref() {
        Some(s) if !s.is_empty() => vec![s.to_string()],
        _ => {
            let mut found = Vec::new();
            for tok in deliverable_shas(&item.deliverable) {
                if let Some(full) = verify_commit(repo, &tok).await {
                    found.push(full);
                }
            }
            found
        }
    };
    if shas.is_empty() {
        return None;
    }
    let mut all_live = true;
    for sha in &shas {
        match is_ancestor(repo, sha, live).await? {
            true => {}
            false => all_live = false,
        }
    }
    Some(if all_live { "live" } else { "pending" })
}

pub(crate) async fn state_for_app(app: &impl ExePath, project_path: &str, item: &LedgerItem) -> Option<&'static str> {
    if item.status != "answered" || !is_agm_project(app, project_path) {
        return None;
    }
    let repo = agm_repo(app);
    deploy_state(&repo, crate::build_info::BUILD_SHA_FULL, item).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use am_base::ledger::LedgerItem;

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn item(status: &str, deliverable: &str, deploy_sha: Option<&str>) -> LedgerItem {
        LedgerItem {
            id: "li".into(),
            project_id: "p".into(),
            code_no: 1,
            source_message_id: None,
            source_turn_id: None,
            ordinal: 0,
            title: "t".into(),
            status: status.into(),
            accountable_role: "manager".into(),
            owner_bot_id: None,
            promised_by_bot_id: None,
            relay_request_id: None,
            assignment_id: None,
            deliverable: deliverable.into(),
            next_step: String::new(),
            wait_for: None,
            due_at: None,
            merged_into_id: None,
            close_reason: None,
            deploy_sha: deploy_sha.map(str::to_string),
            version: 1,
            last_reminded_version: None,
            last_reminded_stage: None,
            last_reminded_at: None,
            last_reminded_count: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[tokio::test]
    async fn deploy_state_is_pending_until_the_commit_is_live_then_live() {
        let dir = crate::testing::scratch_dir("am-ledger-deploy");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "one"]);
        let first = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "two"]);
        let live = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &["checkout", "-q", "-b", "side", &first]);
        git(&dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "unmerged"]);
        let unmerged = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &["checkout", "-q", "main"]);

        let on_live = item("answered", &format!("已推 {}", &first[..7]), None);
        assert_eq!(deploy_state(&dir, &live, &on_live).await, Some("live"));
        let by_field = item("answered", "", Some(&unmerged));
        assert_eq!(deploy_state(&dir, &live, &by_field).await, Some("pending"));
        let mixed = item("answered", &format!("{} {}", &first[..7], &unmerged[..7]), None);
        assert_eq!(deploy_state(&dir, &live, &mixed).await, Some("pending"), "one unmerged sha keeps it pending");
        assert_eq!(deploy_state(&dir, &live, &item("following", &first[..7], None)).await, None, "only answered items get a state");
        assert_eq!(deploy_state(&dir, &live, &item("answered", "改好了，沒有 sha", None)).await, None);
        assert!(same_repo("/tmp/x/", Path::new("/tmp/x")) && !same_repo("/tmp/y", Path::new("/tmp/x")));
    }
}

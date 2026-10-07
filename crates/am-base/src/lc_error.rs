//! LcError, LcResult and RunExit error/outcome definitions and Axum response conversion.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

#[derive(Debug)]
pub enum LcError {
    NotFound(String),
    Conflict(Value),
    Upstream(String),
    Bad(String),
    /// A 404 whose body is machine-readable (`{"error":"not_found","reason":…}`) instead of a bare `what`.
    NotFoundValue(Value),
    /// A 400 whose body is machine-readable rather than a message, e.g.
    /// `{"error":"remote_not_supported","host":"m4p"}`.
    BadValue(Value),
    /// 422: the request is well-formed and allowed, but this one can never be carried out as asked
    /// (e.g. a prompt too long to prove delivered). Machine-readable body; callers treat it as final.
    Unprocessable(Value),
    /// 403：請求本身沒問題，但**你不是可以做這件事的人**（目前只有租約的憑證比對）。
    /// 跟 409 分開：409 是「狀態不對，等一下再來」，403 重試一百次也一樣。
    Forbidden(Value),
    /// 503：我們自己需要的一份狀態暫時讀不到（目前只有維護窗口的租約，issue #127），所以**不敢**往下做——
    /// 不是「herdr／DB 出錯」的統稱 502。body 是機器可讀的，帶 `retryable:true`、`sent:false`（一個字都沒送）。
    Unavailable(Value),
    /// 503：跟 `Unavailable` 相反——外面的副作用**已經做了**（agent 起來了、pane 關了），run 的狀態卻寫不進 DB
    /// （#145／#146）。不是「沒做」也不是「做好了」：重試已經排了，run 會照 herdr 的證據收斂。body 見 [`LcError::uncommitted`]。
    Uncommitted(Value),
}


impl From<crate::github::GithubError> for LcError {
    fn from(error: crate::github::GithubError) -> Self {
        match error {
            crate::github::GithubError::NotFound(what) => LcError::NotFound(what),
            crate::github::GithubError::Bad(message) => LcError::Bad(message),
            crate::github::GithubError::Upstream(message) => LcError::Upstream(message),
        }
    }
}

impl LcError {
    pub fn conflict(reason: &str, extra: Value) -> Self {
        let mut o = json!({ "error": "conflict", "reason": reason });
        if let (Some(a), Some(b)) = (o.as_object_mut(), extra.as_object()) {
            for (k, v) in b {
                a.insert(k.clone(), v.clone());
            }
        }
        LcError::Conflict(o)
    }

    /// `{"error": what, "run_id", "retryable": true, "message", "detail"}`；`what` 是
    /// `start_state_uncommitted`／`stop_state_uncommitted`。
    pub fn uncommitted(what: &str, run_id: &str, message: &str, detail: impl std::fmt::Display) -> Self {
        LcError::Uncommitted(json!({
            "error": what, "run_id": run_id, "retryable": true, "message": message, "detail": detail.to_string(),
        }))
    }

    #[allow(dead_code)]
    pub fn is_retryable(&self) -> bool {
        match self {
            LcError::Conflict(v) | LcError::Unavailable(v) | LcError::Uncommitted(v) | LcError::Unprocessable(v) => {
                v.get("retryable").and_then(Value::as_bool).unwrap_or(false)
            }
            LcError::Upstream(_) => true,
            _ => false,
        }
    }
}

impl IntoResponse for LcError {
    fn into_response(self) -> Response {
        match self {
            LcError::NotFound(what) => (StatusCode::NOT_FOUND, Json(json!({"error": "not_found", "what": what}))).into_response(),
            LcError::NotFoundValue(v) => (StatusCode::NOT_FOUND, Json(v)).into_response(),
            LcError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
            LcError::Bad(m) => (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "message": m}))).into_response(),
            LcError::BadValue(v) => (StatusCode::BAD_REQUEST, Json(v)).into_response(),
            LcError::Unprocessable(v) => (StatusCode::UNPROCESSABLE_ENTITY, Json(v)).into_response(),
            LcError::Forbidden(v) => (StatusCode::FORBIDDEN, Json(v)).into_response(),
            LcError::Unavailable(v) => {
                let retry = v.get("retry_after_secs").and_then(Value::as_i64).unwrap_or(10).max(1);
                (StatusCode::SERVICE_UNAVAILABLE, [(axum::http::header::RETRY_AFTER, retry.to_string())], Json(v)).into_response()
            }
            LcError::Uncommitted(v) => (StatusCode::SERVICE_UNAVAILABLE, Json(v)).into_response(),
            LcError::Upstream(m) => {
                (StatusCode::BAD_GATEWAY, Json(json!({"error": "upstream", "message": m}))).into_response()
            }
        }
    }
}

pub type LcResult<T> = std::result::Result<T, LcError>;

/// [`mark_run_exited`] 做成了什麼。呼叫端多半不看，但「寫不進去」與「別的路徑先收了」要分得開（#135）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    /// 記成 `exited`，收尾（in-flight、孤兒佇列、watcher）做完了。
    Recorded,
    /// 這顆 run 已經不是 active（讀到時就不是，或 CAS 輸給先收掉它的路徑）：收尾歸那條路，這裡一樣都不做。
    AlreadyEnded,
    /// DB 讀寫失敗：什麼都沒動，run 照舊是 active；寫入失敗的排了對帳重試。
    NotRecorded,
    /// 記成 `exited`、孤兒佇列與 watcher 收了，但 in-flight 那一筆寫不進 failed（#156）：記成欠著的收尾，之後補上
    /// （`interruption` 的帳：定時重試、這顆 bot 的下一則 hook／prompt；daemon 重啟則由 `rearm_progress` 補收）。
    TurnOwed,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PromptOut {
    pub turn_id: String,
    pub message_id: String,
    pub delivery: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send_now: Option<&'static str>,
}

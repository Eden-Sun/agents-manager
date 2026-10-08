//! 呼叫端給的冪等鍵（`client_request_id`／`request_id`）的統一驗證（#923）。
//!
//! 這些字串會直接進主鍵／唯一索引，再隨 GET、WebSocket、inbox payload 回出去：不限長度、不擋控制字元，
//! 就是任意大的資料進 DB、換行與控制字元進日誌與前端。一律 trim 之後 1..=[`MAX_LEN`] 字、只收 `[A-Za-z0-9._:-]`。

use crate::lc_error::LcError;

/// 一個冪等鍵最長幾字元（`/prompt` 是 128、assignment 是 200；這裡取較寬的那個，新增的入口統一用它）。
pub const MAX_LEN: usize = 200;

/// 驗證並回 trim 過的值；不合格回 400 `bad_request`，`label` 是欄位名（寫在訊息裡）。
pub fn validate(label: &str, s: &str) -> Result<String, LcError> {
    let t = s.trim();
    let ok = !t.is_empty() && t.len() <= MAX_LEN && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    if ok {
        Ok(t.to_string())
    } else {
        Err(LcError::Bad(format!("{label} must be 1..={MAX_LEN} chars of [A-Za-z0-9._:-]")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ids_pass_and_are_trimmed() {
        for ok in ["a", "req-1", "01J9ZZ", "a.b_c:d-e", &"x".repeat(MAX_LEN)] {
            assert_eq!(validate("client_request_id", ok).unwrap(), ok);
        }
        assert_eq!(validate("client_request_id", "  req-2 \n").unwrap(), "req-2");
    }

    #[test]
    fn empty_oversized_and_unsafe_ids_are_refused() {
        for bad in ["", "   ", "a b", "a/b", "a\nb", "a\u{0}b", "a\u{1b}[31m", "中文", "a;b", "a\"b", &"x".repeat(MAX_LEN + 1)] {
            let e = validate("client_request_id", bad).unwrap_err();
            assert!(matches!(&e, LcError::Bad(m) if m.starts_with("client_request_id must be 1..=200")), "{bad:?}: {e:?}");
        }
    }
}

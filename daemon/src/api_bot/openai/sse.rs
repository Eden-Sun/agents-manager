//! Server-Sent Events 的位元組層解析（只切事件，不懂 Responses 的語意）。
//!
//! 可以任意切塊餵（UTF-8 字元、`\r\n`、事件本身都可能被切開）；行以 `\n`、`\r\n` 或單獨 `\r` 結尾；
//! `data:` 多行以 `\n` 接起來；`:` 開頭是註解；空行送出一個事件；沒有空行收尾的最後一段在 EOF 時**丟棄**
//! （不完整的事件不能當成事件，終態判斷由 `codec::ResponseAssembler` 負責）。單一事件與單行有大小上限，超過是錯誤，不是無限累積。

/// 單一事件（所有 `data:` 行合併後）的上限。
pub(crate) const MAX_EVENT_BYTES: usize = 8 * 1024 * 1024;
/// 單行上限（`encrypted_content` 可能很大，但不會單行超過事件上限）。
pub(crate) const MAX_LINE_BYTES: usize = MAX_EVENT_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    /// `event:` 欄位（沒有就是 `None`；Responses 的型別也寫在 data 的 `type` 裡）。
    pub event: Option<String>,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SseError {
    /// 單行或單一事件超過上限。
    TooLarge,
    /// 不是合法 UTF-8。
    InvalidUtf8,
}

#[derive(Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    /// 上一個位元組是 `\r`：緊接的 `\n` 屬於同一個換行。
    after_cr: bool,
    event: Option<String>,
    data: Vec<String>,
    data_bytes: usize,
    saw_field: bool,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 餵一塊位元組，回這一塊完成的事件。
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseError> {
        let mut out = Vec::new();
        for &b in chunk {
            if self.after_cr {
                self.after_cr = false;
                if b == b'\n' {
                    continue;
                }
            }
            match b {
                b'\n' => self.end_line(&mut out)?,
                b'\r' => {
                    self.after_cr = true;
                    self.end_line(&mut out)?;
                }
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(SseError::TooLarge);
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    /// 串流結束：有沒有收尾的半截事件（呼叫端要把「沒有終態事件」當成斷流，不是成功）。
    pub(crate) fn has_partial(&self) -> bool {
        !self.line.is_empty() || self.saw_field
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseError> {
        let raw = std::mem::take(&mut self.line);
        let line = String::from_utf8(raw).map_err(|_| SseError::InvalidUtf8)?;
        if line.is_empty() {
            if self.saw_field {
                let data = self.data.join("\n");
                out.push(SseEvent { event: self.event.take(), data });
            }
            self.event = None;
            self.data.clear();
            self.data_bytes = 0;
            self.saw_field = false;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_str(), ""),
        };
        match name {
            "data" => {
                self.data_bytes += value.len() + 1;
                if self.data_bytes > MAX_EVENT_BYTES {
                    return Err(SseError::TooLarge);
                }
                self.data.push(value.to_string());
                self.saw_field = true;
            }
            "event" => {
                self.event = Some(value.to_string());
                self.saw_field = true;
            }
            // id／retry／未知欄位：Responses 不用，忽略（不算事件內容）。
            _ => {}
        }
        Ok(())
    }
}

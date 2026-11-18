//! OpenAI Responses API 的 codec 與價表（openai-api bot kind 的切片 B）。
//!
//! **尚無 production caller**：只做「請求怎麼編、SSE 怎麼組、終態與錯誤怎麼分、usage 怎麼算錢」，不碰網路、不讀 key、
//! 不寫 DB。對外用自己的中立型別（`codec::ModelOutput` 等）；把它接成切片 A 的 provider trait 是後續切片
//! （A 的 `Codec`／`Provider` 目前綁著 Anthropic 形狀的 `Response`／`State`，接上去要連 checkpoint 版本一起改）。
//! 加密 reasoning（`encrypted_content`）只供下一次呼叫續傳：不進對話、log、WS（`ReplayItem` 的 Debug 會遮掉）。
#![allow(dead_code)]

pub(crate) mod codec;
pub(crate) mod pricing;
pub(crate) mod sse;

#[cfg(test)]
mod tests;

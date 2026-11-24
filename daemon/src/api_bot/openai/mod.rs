//! OpenAI Responses API 的 codec 與價表（openai-api bot kind 的切片 B）。
//!
//! **尚無 production caller**。`codec`／`sse`／`pricing` 只做「請求怎麼編、SSE 怎麼組、終態與錯誤怎麼分、usage 怎麼算錢」，
//! 不碰網路、不讀 key、不寫 DB；`provider` 把它們接上 `api_bot::provider` 的 `Provider`／`Codec`（切片 D1），
//! 送出用的 key 與 endpoint 由呼叫端傳入。還沒有 bot kind、設定、帳本欄位與 native 生命週期（切片 C、D2）。
//! 加密 reasoning（`encrypted_content`）只供下一次呼叫續傳：不進對話、log、WS（`ReplayItem` 的 Debug 會遮掉）。
#![allow(dead_code)]

pub(crate) mod cap;
pub(crate) mod codec;
pub(crate) mod pricing;
pub(crate) mod provider;
pub(crate) mod sse;

#[cfg(test)]
mod provider_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod key_tests;
#[cfg(test)]
mod leak_tests;
#[cfg(test)]
mod progress_tests;

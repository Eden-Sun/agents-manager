//! UI 要看的 herdr 版本（每台主機一份）：server 版本＋protocol 來自 `ping`，CLI 版本來自 tools 探測的
//! `herdr --version`。兩者可能不一致（CLI 升了但 server 還在跑舊版，#242 審查）——那時 bot 的 herdr 指令
//! 全回 protocol_mismatch，所以要能看出來。讀不到就是 `null`，不猜。
use serde_json::{json, Value};

/// `server`＝最近一次 ping 成功的 `(version, protocol)`（主機沒連上就傳 None）；`cli`＝`herdr --version` 那行原文。
pub fn summary(server: Option<(&str, u32)>, cli: Option<&str>) -> Value {
    let server_version = server.and_then(|(v, _)| crate::changelog::cli_version_string(v).or_else(|| Some(v.trim().to_string()).filter(|s| !s.is_empty())));
    let cli_version = cli.and_then(crate::changelog::cli_version_string);
    let protocol = server.map(|(_, p)| p);
    let mismatch = matches!((&server_version, &cli_version), (Some(s), Some(c)) if s != c);
    json!({
        "server_version": server_version,
        "protocol": protocol,
        // protocol 不在實測過的清單（herdr.rs `SUPPORTED_PROTOCOLS`）：daemon 的 RPC 形狀沒驗過。
        "protocol_supported": protocol.map(crate::herdr::protocol_supported),
        "cli_version": cli_version,
        "mismatch": mismatch,
    })
}

/// `hosts[].herdr` 與 `host_changed.herdr` 共用：server 只在主機連著時報（斷線後的舊 pong 不算現況）。
pub fn for_host(conn: &crate::hosts::HostConn, connected: bool, detected: Option<&crate::tools::HostTools>) -> Value {
    let pong = connected.then(|| conn.client.last_pong()).flatten();
    summary(pong.as_ref().map(|p| (p.version.as_str(), p.protocol)), detected.and_then(|d| d.herdr_cli.as_deref()))
}

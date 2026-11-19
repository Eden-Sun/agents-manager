import { msg, tr } from './i18n'

/**
 * claude-api bot 啟動被 daemon 在「確定沒有副作用」時拒絕的原因（API.md `POST /api/bots/{id}/start`）。
 * 沒見過的 reason 回 null，呼叫端退回 daemon 原文。
 */
const TEXT: Record<string, string> = {
  api_bot_disabled: msg('Claude API 沒有開：config 要同時打開 [claude_api] enabled 與 [claude_api.api_bot] enabled，再重新載入設定。'),
  api_bot_turn_cost_cap_missing: msg('沒有設定單次成本上限：在 [claude_api.api_bot] 填 max_turn_cost_microusd（必須大於 0）。'),
  api_bot_turn_cost_cap_too_low: msg('單次成本上限太低，連一次請求的預留都容不下（至少約 0.33 美元，快速加倍）：把 max_turn_cost_microusd 調高（至少 327680，建議 1000000 以上）。'),
  api_bot_config_invalid: msg('[claude_api.api_bot] 的設定值超出允許範圍，請檢查各項上限。'),
  api_bot_sandbox_unavailable: msg('這台機器沒有可用的沙箱：需要安裝 bwrap（/usr/bin/bwrap），且要能建立 user namespace。'),
  api_bot_workspace_unavailable: msg('Bot 的工作目錄無法使用，請確認路徑存在且可存取。'),
  api_bot_key_unavailable: msg('讀不到 API key：請確認 [claude_api] key_file 指向存在、權限正確且非空的檔案。'),
  api_native_prompt_unconfirmed: msg('daemon 無法確認這一則有沒有交給 Claude API（可能上一回合還沒結算，或 bot 正忙）；輸入已保留，請稍後再送，或先停止再啟動。'),
  api_native_start_unconfirmed: msg('daemon 無法確認 claude-api bot 的啟動狀態；請稍後再試。'),
  'api_bot_stop_unconfirmed:busy': msg('bot 還有一個回合沒有結算，暫時無法停止；請稍後再試。'),
  api_bot_host_offline: msg('這顆 claude-api bot 的專案在遠端主機，但那台主機目前沒有連線：等主機連上線再啟動。'),
  api_model_changed: msg('Bot 目前的模型與上一段對話產生的模型不相容：可換回原模型接續，或清除對話以新模型開始。'),
  api_context_unsettled: msg('上一回合對話狀態尚未完全結算（有未完成的請求或工具呼叫），無法安全接續；請稍後再試。'),
  api_session_expired: msg('上一段對話 session 已過期，無法接續；請清除對話後重新開始。'),
  api_checkpoint_too_large: msg('對話記錄已超出上限大小，無法載入；請清除對話後重新開始。'),
  api_receipt_unsettled: msg('對話結算憑證尚未就緒，無法確認狀態；請稍後再試。'),
  cache_ttl_unsupported_for_kind: msg('這個 bot 種類不支援 prompt cache 保留時間（cache_ttl）：請不要帶 cache_ttl，改用預設。'),
  cache_ttl_api_bot_only: msg('cache_ttl 只給 claude-api bot 用。'),
  api_bot_effort_unsupported_for_model: msg('這個模型不收這個強度：gpt-6.2 Astra／Sol 不收 none，請改 low 以上或換 gpt-6-luna。'),
  openai_not_configured: msg('先到 環境設定 → OpenAI API 啟用並設定金鑰（啟用開關與金鑰檔都要就緒）。'),
  attachments_unsupported_for_kind: msg('這個 bot 種類不支援附件：請移除附件後再送。'),
  compact_unsupported: msg('這個 bot 種類不支援 /compact。'),
}

/**
 * openai-api bot 的同名原因要講 OpenAI 的設定位置（使用者 2026-11-19：缺單回合上限時寫成 [claude_api.api_bot] 是錯的）。
 * daemon 的 reason 名稱跟 Claude 共用，所以依 bot 的 kind 選句；沒有對應的 OpenAI 句就退回通用句。
 */
const OPENAI_TEXT: Record<string, string> = {
  api_bot_turn_cost_cap_missing: msg('沒有設定單回合成本上限：到 設定 → OpenAI API 填「單回合成本上限」（或 config 的 [openai_api.api_bot] max_turn_cost_microusd，必須大於 0）。'),
  api_bot_config_invalid: msg('[openai_api.api_bot] 的設定值超出允許範圍，請檢查各項上限。'),
  api_bot_key_unavailable: msg('讀不到 OpenAI API key：請確認 [openai_api] key_file 指向存在、權限正確且非空的檔案。'),
}

export function apiBotStartReasonText(reason: unknown, kind?: unknown): string | null {
  if (typeof reason !== 'string') return null
  if (kind === 'openai-api' && Object.hasOwn(OPENAI_TEXT, reason)) return tr(OPENAI_TEXT[reason])
  return Object.hasOwn(TEXT, reason) ? tr(TEXT[reason]) : null
}

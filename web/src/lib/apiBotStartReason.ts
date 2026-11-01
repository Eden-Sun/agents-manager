import { msg, tr } from './i18n'

/**
 * claude-api bot 啟動被 daemon 在「確定沒有副作用」時拒絕的原因（API.md `POST /api/bots/{id}/start`）。
 * 沒見過的 reason 回 null，呼叫端退回 daemon 原文。
 */
const TEXT: Record<string, string> = {
  api_bot_disabled: msg('Claude API 沒有開：config 要同時打開 [claude_api] enabled 與 [claude_api.api_bot] enabled，再重新載入設定。'),
  api_bot_turn_cost_cap_missing: msg('沒有設定單次成本上限：在 [claude_api.api_bot] 填 max_turn_cost_microusd（必須大於 0）。'),
  api_bot_turn_cost_cap_too_low: msg('單次成本上限低於一次請求的預留額（標準速度約 4.17 美元；快速加倍）：把 max_turn_cost_microusd 調高（至少 4163840）。'),
  api_bot_config_invalid: msg('[claude_api.api_bot] 的設定值超出允許範圍，請檢查各項上限。'),
  api_bot_sandbox_unavailable: msg('這台機器沒有可用的沙箱：需要安裝 bwrap（/usr/bin/bwrap），且要能建立 user namespace。'),
  api_bot_workspace_unavailable: msg('Bot 的工作目錄無法使用，請確認路徑存在且可存取。'),
  api_bot_key_unavailable: msg('讀不到 API key：請確認 [claude_api] key_file 指向存在、權限正確且非空的檔案。'),
  api_bot_local_project_only: msg('claude-api bot 只能放在本機專案。'),
}

export function apiBotStartReasonText(reason: unknown): string | null {
  return typeof reason === 'string' && Object.hasOwn(TEXT, reason) ? tr(TEXT[reason]) : null
}

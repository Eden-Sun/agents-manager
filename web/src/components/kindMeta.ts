import type { BotKind } from '../api/types'
import { msg } from '../lib/i18n'

export const KIND_LABEL: Record<BotKind, string> = { claude: 'claude', codex: 'codex', grok: 'grok', agy: 'agy' }

/**
 * 預設 tooltip 寫「這是什麼」而非重複 kind 名（2026-09-12 使用者：「header 裡 codex 的 kind 文字說明可以優化」）。
 * `aria-label` 維持只有 kind 名：驗收腳本與朗讀靠它。
 */
export const KIND_DESC: Record<BotKind, string> = {
  claude: msg('claude · Anthropic 的 CLI（Claude 模型）'),
  codex: msg('codex · OpenAI 的 CLI（GPT 模型，標誌是 OpenAI 的）'),
  grok: msg('grok · xAI 的 CLI（Grok 模型）'),
  agy: msg('agy · Google Antigravity CLI（Gemini 模型）'),
}

import type { BotKind } from '../api/types'
import { msg } from '../lib/i18n'

export const KIND_LABEL: Record<BotKind, string> = { claude: 'claude', codex: 'codex', grok: 'grok', agy: 'agy', 'claude-api': 'claude-api', 'openai-api': 'OpenAI' }

export function isApiKind(kind: BotKind | string | null | undefined): boolean {
  return kind === 'claude-api' || kind === 'openai-api'
}

export function apiProvider(kind: BotKind | string | null | undefined): 'anthropic' | 'openai' | null {
  if (kind === 'claude-api') return 'anthropic'
  if (kind === 'openai-api') return 'openai'
  return null
}

export const KIND_DESC: Record<BotKind, string> = {
  claude: msg('claude · Anthropic 的 CLI（Claude 模型）'),
  codex: msg('codex · OpenAI 的 CLI（GPT 模型，標誌是 OpenAI 的）'),
  grok: msg('grok · xAI 的 CLI（Grok 模型）'),
  agy: msg('agy · Google Antigravity CLI（Gemini 模型）'),
  'claude-api': msg('claude-api · 直接打 Claude API 的 bot（daemon 內的 harness，沒有 CLI／pane）'),
  'openai-api': msg('openai-api · 直接打 OpenAI API 的 bot（daemon 內的 harness，沒有 CLI／pane）'),
}

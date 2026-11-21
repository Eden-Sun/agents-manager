import type { BotKind } from '../api/types'
import { identityStatusOfHost, projectHostName, useStore } from '../store/store'
import { msg } from './i18n'

type StoreState = ReturnType<typeof useStore.getState>

export function canLoginInSession(kind: BotKind): boolean {
  return kind === 'claude' || kind === 'grok'
}

export function shouldUseIdentityLogin(identity: string | null): identity is string {
  return identity !== null
}

export function findLoginTargetId(
  s: StoreState,
  host: string,
  kind: BotKind,
  identity: string | null,
): string | null {
  const bot = s.bots.find((b) => {
    if (b.kind !== kind) return false
    if ((b.identity ?? null) !== identity) return false
    if (projectHostName(s, b.project_id) !== host) return false
    const r = s.runs[b.id]
    return r ? r.state !== 'stopped' && r.state !== 'exited' : false
  })
  return bot?.id ?? null
}

const CLI_LOGIN: Record<BotKind, string> = {
  claude: 'claude auth login',
  codex: 'codex login --device-auth',
  grok: 'grok login',
  agy: 'AGY_CLI_DISABLE_AUTO_UPDATE=true agy',
  'claude-api': '',
  'openai-api': '',
}

export function cliLoginCommand(kind: BotKind, env: Record<string, string>): string {
  const entries = Object.entries(env)
  if (entries.length === 0) return CLI_LOGIN[kind]
  return `env ${entries.map(([k, v]) => `${k}=${shellQuote(v)}`).join(' ')} ${CLI_LOGIN[kind]}`
}

export function identityEnv(s: StoreState, host: string, kind: BotKind, identity: string | null): Record<string, string> {
  if (!identity) return {}
  const cfg = s.identities.find((i) => i.kind === kind && i.name === identity)
  if (cfg) return cfg.env
  const dir = identityStatusOfHost(s, host)[identity]?.config_dir
  return dir && kind === 'claude' ? { CLAUDE_CONFIG_DIR: dir } : {}
}

function shellQuote(v: string): string {
  return `'${v.replaceAll("'", `'\\''`)}'`
}

export const CODEX_LOGIN_GUIDE = msg('終端會印出登入網址和一次性碼：用手機或任何裝置打開網址、輸入那組碼就好，不用貼回終端。')
export const AGY_LOGIN_GUIDE = msg('agy 的 TUI 會引導登入：SSH 底下它會印出一個授權網址，在瀏覽器完成授權後，把授權碼貼回那個終端。登好後輸入 /quit 離開。')

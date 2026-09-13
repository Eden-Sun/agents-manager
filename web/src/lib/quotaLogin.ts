import type { BotKind } from '../api/types'
import { identityStatusOfHost, projectHostName, useStore } from '../store/store'

/** store 沒 export state 型別，就地取——不動共用的 `store.ts`。 */
type StoreState = ReturnType<typeof useStore.getState>

/**
 * TUI 內有沒有 `/login`：claude 2.1.263、grok 1.0.13 有；codex 0.153.4 沒有（只能外面跑
 * `codex login`）。須與 daemon `lifecycle::login_slash_command` 一致，否則回 400 `login_unsupported`。
 */
export function canLoginInSession(kind: BotKind): boolean {
  return kind === 'claude' || kind === 'grok'
}

/**
 * 「未登入」的 `/login` 要送進哪個活著的同 host／kind／身份 bot；沒有回 null（按鈕 disabled）。
 * 回 id 不回物件：當 selector 用，新物件會讓 useSyncExternalStore 每輪都判定變了。
 */
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

/** TUI 外登入指令（claude 2.1.263 `auth login`、grok 1.0.13 `login`）。 */
const CLI_LOGIN: Record<BotKind, string> = {
  claude: 'claude auth login',
  codex: 'codex login',
  grok: 'grok login',
}

/** 打進主機 shell 的一行。身份本體是 env，不帶就登到預設帳號，所以有 env 就加 `env K=V …` 前綴。 */
export function cliLoginCommand(kind: BotKind, env: Record<string, string>): string {
  const entries = Object.entries(env)
  if (entries.length === 0) return CLI_LOGIN[kind]
  return `env ${entries.map(([k, v]) => `${k}=${shellQuote(v)}`).join(' ')} ${CLI_LOGIN[kind]}`
}

/** config `[[identities]]` 優先，否則用主機回報的 `config_dir`（`ccN` alias），都沒有是預設帳號。 */
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

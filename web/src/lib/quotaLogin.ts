import type { BotKind } from '../api/types'
import { identityStatusOfHost, projectHostName, useStore } from '../store/store'

/** store 沒 export state 型別，這裡就地取——不動共用的 `store.ts`。 */
type StoreState = ReturnType<typeof useStore.getState>

/**
 * 這個 kind 的 TUI 有沒有「不離開 session 就能登入 / 換帳號」的 slash 指令。
 *
 * claude 2.1.263 與 grok 1.0.13 都有 `/login`；**codex 0.153.4 沒有**——它的 slash 選單
 * 只有 `/logout`，登入得在 TUI 外面跑 `codex login`。daemon 那邊同一份判斷在
 * `lifecycle::login_slash_command`，對不上時後端會回 400 `login_unsupported`。
 */
export function canLoginInSession(kind: BotKind): boolean {
  return kind === 'claude' || kind === 'grok'
}

/**
 * 額度 popover 那句「未登入」要把 `/login` 送給誰。
 *
 * 送 `/login` 是送進某個**正在跑**的 pane，所以只有同 host、同 kind、同身份而且還活著的
 * bot 能當目標；一個都沒有時回 null（按鈕就 disabled，叫使用者先起一個）。多個就取第一個
 * ——它們登的是同一個帳號，登哪一個結果一樣。
 *
 * 回的是 bot id 而不是物件：這支會被 `useStore` 當 selector 用，每次回新物件會讓
 * useSyncExternalStore 每輪 state 更新都判定成變了。
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

/** 各 kind 在 TUI 外面登入的 CLI 指令（claude 2.1.263 有 `auth login`、grok 1.0.13 有 `login`）。 */
const CLI_LOGIN: Record<BotKind, string> = {
  claude: 'claude auth login',
  codex: 'codex login',
  grok: 'grok login',
}

/**
 * 要打進主機 shell 的那一行。
 *
 * 身份的本體是 env（config 身份的 `Identity.env`；shell 的 `ccN` alias 只認得 `CLAUDE_CONFIG_DIR`，
 * 由 `IdentityStatus.config_dir` 帶回來）：不帶著它就會登到預設帳號，使用者按了半天還是那個
 * 沒登入的身份沒動。所以有 env 就用 `env K=V … <cli> login` 前綴；沒有（預設帳號 cc0，或這裡
 * 看不到 env）就送裸的指令。
 */
export function cliLoginCommand(kind: BotKind, env: Record<string, string>): string {
  const entries = Object.entries(env)
  if (entries.length === 0) return CLI_LOGIN[kind]
  return `env ${entries.map(([k, v]) => `${k}=${shellQuote(v)}`).join(' ')} ${CLI_LOGIN[kind]}`
}

/**
 * 從 store 湊出某身份在某主機上的 env：config 的 `[[identities]]` 優先，否則用該主機
 * `identity_status` 回報的 `config_dir`（`ccN` alias），兩邊都沒有就是預設帳號、空物件。
 */
export function identityEnv(s: StoreState, host: string, kind: BotKind, identity: string | null): Record<string, string> {
  if (!identity) return {}
  const cfg = s.identities.find((i) => i.kind === kind && i.name === identity)
  if (cfg) return cfg.env
  const dir = identityStatusOfHost(s, host)[identity]?.config_dir
  return dir && kind === 'claude' ? { CLAUDE_CONFIG_DIR: dir } : {}
}

/** 只求安全：一律單引號包起來，內部的單引號用 `'\''` 收尾再接。 */
function shellQuote(v: string): string {
  return `'${v.replaceAll("'", `'\\''`)}'`
}

import { useState } from 'react'
import { useT } from '../lib/i18n'
import { BOT_KINDS, type BotKind } from '../api/types'
import { useStore } from '../store/store'
import { identityHost, identityRowKey } from '../store/identityRows'
import { checkEnvText } from './identityEnv'
import { useUnsavedGuard } from '../hooks/useUnsavedGuard'
import { DefaultAccountRow, IdentityRow } from './IdentityRow'
import { ShellIdentityRows } from './IdentityShellRows'
import { IdentityKindCard } from './IdentityKindCard'
import { IdentityHostTabs, IDENTITY_HOST_PANEL_ID, type HostTabItem } from './IdentityHostTabs'
import './identitiesPanel.css'

export { IdentityBadge } from './IdentityRow'

/** 身份預設（例如 `cc1` = 另一個 `CLAUDE_CONFIG_DIR`）：daemon 啟動 bot 時注入 `env`；`args` 契約仍在但 UI 不提供輸入。 */
const SWITCHABLE_KINDS: readonly BotKind[] = ['claude', 'codex', 'grok', 'agy']
const LAST_TAB_KEY = 'agm.identities.host-tab'

function readLastTab(): string | null {
  try {
    return globalThis.localStorage?.getItem(LAST_TAB_KEY) ?? null
  } catch {
    return null // 讀不到（隱私模式、被擋）就當沒記住
  }
}

function writeLastTab(name: string) {
  try {
    globalThis.localStorage?.setItem(LAST_TAB_KEY, name)
  } catch {
    // 寫不進去不影響功能
  }
}

/** 各 kind 的設定目錄變數（SPEC §16／§12）：claude 用 CLAUDE_CONFIG_DIR、codex 用 CODEX_HOME、grok 用 GROK_HOME；agy 沒有，換 `HOME`。 */
const ENV_PREFILL: Record<BotKind, string> = {
  claude: 'CLAUDE_CONFIG_DIR=$HOME/.claude-',
  codex: 'CODEX_HOME=$HOME/.codex-',
  grok: 'GROK_HOME=$HOME/.grok-',
  // agy 的狀態在 `$HOME/.gemini`，沒有專用變數、XDG_* 也無效（2026-11-14 實測）：身份＝換一個 HOME（daemon 會建影子家目錄）。
  agy: 'HOME=$HOME/.agy-',
  'claude-api': '',
  'openai-api': '',
}

function NewIdentityForm() {
  const t = useT()
  const addIdentity = useStore((s) => s.addIdentity)
  const [name, setName] = useState('')
  const [kind, setKind] = useState<BotKind>('claude')
  const [envText, setEnvText] = useState(ENV_PREFILL.claude)
  const [cleanPreset, setCleanPreset] = useState({ kind: 'claude' as BotKind, envText: ENV_PREFILL.claude })
  const [busy, setBusy] = useState(false)
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)
  const envCheck = checkEnvText(envText)
  useUnsavedGuard(name !== '' || kind !== cleanPreset.kind || envText !== cleanPreset.envText)

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!nameOk || busy || envCheck.errors.length > 0) return
        const submitted = { name, kind, envText }
        setBusy(true)
        void addIdentity({ name: submitted.name, kind: submitted.kind, env: envCheck.env }).then((ok) => {
          setBusy(false)
          if (ok) {
            setName((current) => (current === submitted.name ? '' : current))
            setCleanPreset({ kind: submitted.kind, envText: submitted.envText })
          }
        })
      }}
    >
      <label className="field">
        <span>{t('名稱（例如 cc1）')}</span>
        <input
          type="text"
          value={name}
          placeholder="cc1"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? (
          <span className="hint">{t('小寫英文字母開頭，之後可接小寫字母、數字、- 或 _，最多 32 個字。')}</span>
        ) : null}
      </label>
      <fieldset className="field kind-radios">
        <legend>kind</legend>
        <div className="kind-radio-choices" role="radiogroup" aria-label="kind">
          {(['claude', 'codex', 'grok', 'agy'] as const).map((k) => (
            <label key={k}>
              <input
                type="radio"
                name="identity-kind"
                value={k}
                checked={kind === k}
                onChange={() => {
                  // 使用者沒改過那一行才跟著換 kind 的設定目錄變數，免得蓋掉打好的。
                  setEnvText((cur) => (cur === ENV_PREFILL[kind] ? ENV_PREFILL[k] : cur))
                  setKind(k)
                }}
              />
              {k}
            </label>
          ))}
        </div>
        {kind === 'agy' ? (
          <span className="hint" data-k="agy-home-hint">
            {t.x('agy 沒有設定目錄變數，所以這個身份是換一個 {home}：daemon 會建那個目錄，{gemini} 與 agy 的憑證各自獨立（建好是未登入，登入請按身份列上的「登入」），其餘（git／ssh／PATH 等）用連結指回真家目錄。副作用：用這個身份的 agy 看到的 {homeVar} 是那個目錄。', { home: 'HOME', gemini: '.gemini', homeVar: '$HOME' })}
          </span>
        ) : null}
      </fieldset>
      <label className="field">
        <span>{t.x('env（每行 KEY=VALUE；{home} 與開頭 {tilde} 會以該主機的家目錄展開；claude 用 CLAUDE_CONFIG_DIR、grok 用 GROK_HOME）', { home: '$HOME', tilde: '~' })}</span>
        <textarea rows={3} value={envText} spellCheck={false} onChange={(e) => setEnvText(e.target.value)} />
        {envCheck.errors.length > 0 ? (
          <ul className="hint env-errors" role="alert">
            {envCheck.errors.map((e) => (
              <li key={e}>{e}</li>
            ))}
          </ul>
        ) : null}
      </label>
      <div className="form-actions">
        <button type="submit" className="btn primary" disabled={!nameOk || busy || envCheck.errors.length > 0}>
          {t('新增身份')}
        </button>
      </div>
    </form>
  )
}

export function IdentitiesPanel() {
  const t = useT()
  const hosts = useStore((s) => s.hosts)
  const identities = useStore((s) => s.identities)
  const localConnected = useStore((s) => s.connected)
  const localTools = useStore((s) => s.localTools)
  const localStatuses = useStore((s) => s.localIdentityStatus)
  const hostNames = ['local', ...hosts.map((h) => h.name)]
  const [picked, setPicked] = useState<string | null>(() => readLastTab())
  const active = picked && hostNames.includes(picked) ? picked : 'local'

  const summarize = (name: string, label: string, connected: boolean, tools: typeof localTools | undefined, statuses: typeof localStatuses): HostTabItem => {
    const defaults = BOT_KINDS.filter((k) => tools?.[k]?.installed && tools[k]?.logged_in === true).length
    const values = Object.values(statuses)
    return {
      name,
      label,
      connected,
      loggedIn: defaults + values.filter((st) => st.logged_in === true).length,
      anomaly: !connected || values.some((st) => Boolean(st.login_needed)),
    }
  }
  const tabs: HostTabItem[] = [
    summarize('local', t('本機'), localConnected, localTools, localStatuses),
    ...hosts.map((h) => summarize(h.name, h.name, h.connected, h.tools, h.identity_status)),
  ]
  const activeConnected = active === 'local' ? localConnected : hosts.find((h) => h.name === active)?.connected !== false
  // 只有一台主機時不顯示分頁；其他情況有幾台就幾個 tab。
  const kinds: BotKind[] = [
    ...SWITCHABLE_KINDS,
    ...BOT_KINDS.filter((k) => !SWITCHABLE_KINDS.includes(k) && identities.some((i) => identityHost(i) === active && i.kind === k)),
  ]
  const choose = (name: string) => {
    setPicked(name)
    writeLastTab(name)
  }

  return (
    <div className="hosts-panel identities-panel">
      {hostNames.length > 1 ? <IdentityHostTabs tabs={tabs} value={active} onChange={choose} /> : null}
      <div role={hostNames.length > 1 ? 'tabpanel' : undefined} id={IDENTITY_HOST_PANEL_ID} aria-labelledby={hostNames.length > 1 ? `identity-tab-${active}` : undefined} className="identity-host-panel">
        {!activeConnected ? (
          <p className="identity-offline-banner" role="status">
            {t('主機離線：以下是最後已知的狀態，這台暫時不能登入、登出或變更身份。')}
          </p>
        ) : null}
        {kinds.map((kind) => (
          <IdentityKindCard key={kind} host={active} kind={kind}>
            <DefaultAccountRow host={active} kind={kind} />
            {identities
              .filter((i) => identityHost(i) === active && i.kind === kind)
              .map((i) => (
                <IdentityRow key={identityRowKey(i)} host={active} name={i.name} />
              ))}
            <ShellIdentityRows host={active} kind={kind} />
          </IdentityKindCard>
        ))}
      </div>
      <div className="identity-section">
        <h3 className="identity-section-title">{t('新增身份')}</h3>
        <NewIdentityForm />
      </div>
    </div>
  )
}

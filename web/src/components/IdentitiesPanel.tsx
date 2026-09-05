import { useState } from 'react'
import type { BotKind } from '../api/types'
import { useStore } from '../store/store'

/**
 * 身份預設（identities）：例如 `cc1` = 用另一個 `CLAUDE_CONFIG_DIR` 跑不同帳號。
 * daemon 啟動 bot 時把 `env` 注入 pane。身份的本體就是 env，所以表單只收 env；
 * `args` 契約仍在（舊設定讀得到），但 UI 不再提供輸入。
 */

/** `KEY=VALUE` 每行 → map；忽略空行與 `#` 註解。 */
export function parseEnvText(text: string): Record<string, string> {
  const out: Record<string, string> = {}
  for (const raw of text.split('\n')) {
    const line = raw.trim()
    if (!line || line.startsWith('#')) continue
    const i = line.indexOf('=')
    if (i <= 0) continue
    out[line.slice(0, i).trim()] = line.slice(i + 1).trim()
  }
  return out
}

export function envToText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join('\n')
}

export function IdentityBadge({ name }: { name: string | null }) {
  if (!name) return null
  return (
    <span className="identity-badge" title={`身份：${name}`}>
      {name}
    </span>
  )
}

function IdentityRow({ name }: { name: string }) {
  const ident = useStore((s) => s.identities.find((i) => i.name === name))
  const used = useStore((s) => s.bots.filter((b) => b.identity === name).length)
  const removeIdentity = useStore((s) => s.removeIdentity)
  if (!ident) return null
  const envText = envToText(ident.env)
  return (
    <div className="identity-row">
      <span className="identity-main">
        <span className="identity-name">
          <IdentityBadge name={ident.name} />
          <span className={`kind-tag ${ident.kind}`}>{ident.kind}</span>
          <span className="host-count">{used > 0 ? `${used} 個 Bot` : '未使用'}</span>
        </span>
        <span className="identity-detail" title={envText}>
          {envText ? envText.replace(/\n/g, ' ・ ') : '（無 env）'}
        </span>
      </span>
      <button
        type="button"
        className="icon-btn"
        title={used > 0 ? '仍有 Bot 使用這個身份' : '刪除身份'}
        onClick={() => {
          if (confirm(`刪除身份「${name}」？`)) void removeIdentity(name)
        }}
      >
        ✕
      </button>
    </div>
  )
}

function NewIdentityForm() {
  const addIdentity = useStore((s) => s.addIdentity)
  const [name, setName] = useState('')
  const [kind, setKind] = useState<BotKind>('claude')
  const [envText, setEnvText] = useState('CLAUDE_CONFIG_DIR=$HOME/.claude-')
  const [busy, setBusy] = useState(false)
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)

  return (
    <form
      className="form"
      onSubmit={(e) => {
        e.preventDefault()
        if (!nameOk || busy) return
        setBusy(true)
        void addIdentity({ name, kind, env: parseEnvText(envText) }).then((ok) => {
          setBusy(false)
          if (ok) setName('')
        })
      }}
    >
      <label className="field">
        <span>名稱（例如 cc1）</span>
        <input
          type="text"
          value={name}
          placeholder="cc1"
          spellCheck={false}
          onChange={(e) => setName(e.target.value.toLowerCase())}
        />
        {name && !nameOk ? <span className="hint">必須符合 [a-z][a-z0-9_-]&#123;0,31&#125;</span> : null}
      </label>
      <label className="field">
        <span>kind</span>
        <select value={kind} onChange={(e) => setKind(e.target.value as BotKind)}>
          <option value="claude">claude</option>
          <option value="codex">codex</option>
        </select>
      </label>
      <label className="field">
        <span>env（每行 KEY=VALUE；`$HOME` 與開頭 `~` 會以該主機的家目錄展開）</span>
        <textarea rows={3} value={envText} spellCheck={false} onChange={(e) => setEnvText(e.target.value)} />
      </label>
      <div className="form-actions">
        <button type="submit" className="btn primary" disabled={!nameOk || busy}>
          新增身份
        </button>
      </div>
    </form>
  )
}

export function IdentitiesPanel() {
  const identities = useStore((s) => s.identities)
  return (
    <div className="hosts-panel identities-panel">
      {identities.length === 0 ? <p className="hint">尚未定義任何身份。</p> : null}
      {identities.map((i) => (
        <IdentityRow key={i.name} name={i.name} />
      ))}
      <NewIdentityForm />
    </div>
  )
}

import { useEffect, useLayoutEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { EFFORT_OPTIONS, MODEL_CUSTOM, MODEL_DEFAULT, MODEL_OPTIONS } from '../api/types'
import type { BotKind, PatchBotInput } from '../api/types'
import { projectHostName, useStore } from '../store/store'
import { ConfirmDialog } from './ConfirmDialog'
import { KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'

/**
 * 「Bot 設定」面板（API.md v3.3）：改名 / 模型 / 身份 / autostart / auto_approve，
 * 以及刪除 Bot。儲存走 `PATCH /api/bots/:id`，只送有變更的欄位；後端回
 * `needs_restart: true` 時面板頂部提示要重啟才生效（`POST /api/bots/:id/restart`）。
 *
 * 使用者決定 UI 不提供 `args` / `env` / `inject_hooks`：契約與型別保留，但這裡既不顯示
 * 也不會出現在 PATCH body 裡（維持 config.toml 既有的值）。
 */

/** 各 kind 的 auto_approve 旗標（daemon `injected_args`）。 */
export function AutoApproveFlags() {
  return (
    <>
      claude <code>--dangerously-skip-permissions</code> / codex <code>--yolo</code> / grok{' '}
      <code>--always-approve</code>
    </>
  )
}

/** 模型欄位下方的提示文字。 */
export function modelHint(kind: BotKind): string {
  switch (kind) {
    case 'claude':
      return 'claude 預設可能是 haiku，建議選 opus 或 sonnet'
    case 'codex':
      return '選「使用 CLI 預設」則不帶 -m，由 codex 自行決定'
    case 'grok':
      return '選「使用 CLI 預設」則不帶 -m，由 grok 自行決定（`grok models`：grok-4.6 為預設）'
  }
}

/**
 * 模型下拉：常用別名 + 「（預設）」+「自訂…」（任意字串）。
 * `null` = 不帶 `--model`，由 agent CLI 自己決定。
 */
export function ModelField({
  kind,
  value,
  onChange,
  hint,
}: {
  kind: BotKind
  value: string | null
  onChange: (v: string | null) => void
  hint?: ReactNode
}) {
  const opts = MODEL_OPTIONS[kind]
  const known = value === null || opts.includes(value)
  const [customMode, setCustomMode] = useState(false)
  const custom = customMode || !known

  return (
    <label className="field">
      <span>模型</span>
      <select
        value={custom ? MODEL_CUSTOM : (value ?? MODEL_DEFAULT)}
        onChange={(e) => {
          const v = e.target.value
          if (v === MODEL_CUSTOM) {
            setCustomMode(true)
            return
          }
          setCustomMode(false)
          onChange(v === MODEL_DEFAULT ? null : v)
        }}
      >
        <option value={MODEL_DEFAULT}>使用 CLI 預設</option>
        {opts.map((m) => (
          <option key={m} value={m}>
            {m}
          </option>
        ))}
        <option value={MODEL_CUSTOM}>自訂…</option>
      </select>
      {custom ? (
        <input
          type="text"
          value={value ?? ''}
          placeholder={opts[0]}
          spellCheck={false}
          aria-label="自訂模型名稱"
          onChange={(e) => onChange(e.target.value ? e.target.value : null)}
        />
      ) : null}
      {hint ? <span className="hint">{hint}</span> : null}
    </label>
  )
}

/** grok only: reasoning effort as a row of options (daemon → `--reasoning-effort`). */
export function EffortField({ value, onChange }: { value: string | null; onChange: (v: string | null) => void }) {
  return (
    <div className="field">
      <span>強度</span>
      <div className="opt-group" role="radiogroup" aria-label="reasoning effort">
        <button type="button" className={`opt${value === null ? ' on' : ''}`} onClick={() => onChange(null)}>
          預設
        </button>
        {EFFORT_OPTIONS.map((e) => (
          <button key={e} type="button" className={`opt${value === e ? ' on' : ''}`} onClick={() => onChange(e)}>
            {e}
          </button>
        ))}
      </div>
    </div>
  )
}

/** v4.0 人設：auto-growing textarea; empty = null. */
export function PersonaField({ value, onChange, collapsible }: { value: string; onChange: (v: string) => void; collapsible?: boolean }) {
  const [open, setOpen] = useState(!collapsible || Boolean(value))
  const ref = useRef<HTMLTextAreaElement>(null)
  useEffect(() => {
    const el = ref.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${Math.min(240, Math.max(56, el.scrollHeight))}px`
  }, [value, open])
  if (collapsible && !open) {
    return (
      <button type="button" className="disclosure sub persona-toggle" aria-expanded={false} onClick={() => setOpen(true)}>
        <span className="chev">▶</span> 人設（選填）
      </button>
    )
  }
  return (
    <label className="field persona-field">
      <span>
        人設{collapsible ? '（選填）' : ''}
        <span className="field-note">啟動時作為 system prompt 前置文字</span>
      </span>
      <textarea
        ref={ref}
        value={value}
        rows={2}
        placeholder="你是這個專案的 PM，回覆用繁體中文、先給結論"
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  )
}

/** Small marker shown next to a bot that has a persona; hover = the first 80 chars. */
export function PersonaMark({ persona }: { persona: string | null }) {
  if (!persona) return null
  const short = persona.length > 80 ? `${persona.slice(0, 80)}…` : persona
  return (
    <span className="persona-mark" role="img" aria-label="有人設" title={`人設：${short}`}>
      <svg viewBox="0 0 16 16" width="1em" height="1em" aria-hidden="true">
        <path d="M2.5 3.5h11v7h-6l-3 2.5v-2.5h-2z" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinejoin="round" />
        <path d="M5 6.5h6M5 8.5h4" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
      </svg>
    </span>
  )
}

/** claude only: identity as a row of options（無下拉）. Renders nothing when there are no identities. */
export function IdentityOptions({
  kind,
  value,
  onChange,
}: {
  kind: BotKind
  value: string
  onChange: (v: string) => void
}) {
  // Select the stable array and filter outside: a selector that returns a fresh array
  // re-renders forever (React #185).
  const all = useStore((s) => s.identities)
  const identities = all.filter((i) => i.kind === 'claude')
  if (kind !== 'claude' || identities.length === 0) return null
  return (
    <div className="field">
      <span>身份</span>
      <div className="opt-group" role="radiogroup" aria-label="identity">
        <button type="button" className={`opt${value === '' ? ' on' : ''}`} onClick={() => onChange('')}>
          不指定身分（本機預設）
        </button>
        {identities.map((i) => (
          <button key={i.name} type="button" className={`opt${value === i.name ? ' on' : ''}`} title={Object.entries(i.env).map(([k, v]) => `${k}=${v}`).join(' ')} onClick={() => onChange(i.name)}>
            {i.name}
          </button>
        ))}
      </div>
    </div>
  )
}

export function BotSettingsPanel({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const project = useStore((s) => s.projects.find((p) => p.id === s.bots.find((b) => b.id === botId)?.project_id))
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const closeSettings = useStore((s) => s.closeSettings)
  const patchBot = useStore((s) => s.patchBot)
  const restartBot = useStore((s) => s.restartBot)
  const removeBot = useStore((s) => s.removeBot)
  const notify = useStore((s) => s.notify)

  const [name, setName] = useState(bot?.name ?? '')
  const [model, setModel] = useState<string | null>(bot?.model ?? null)
  const [effort, setEffort] = useState<string | null>(bot?.effort ?? null)
  const [fast, setFast] = useState<boolean>(bot?.fast ?? false)
  const [persona, setPersona] = useState(bot?.persona ?? '')
  const [identity, setIdentity] = useState(bot?.identity ?? '')

  const [banner, setBanner] = useState<'saved' | 'restart' | null>(null)
  const [saving, setSaving] = useState(false)
  const [restarting, setRestarting] = useState(false)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [closeConfirmOpen, setCloseConfirmOpen] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  // 彈窗貼著觸發它的齒輪開，超出視窗才翻邊/夾住；沒有 anchor（例如鍵盤流程）就置中。
  const anchor = useStore((s) => s.settingsAnchor)
  const cardRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)
  useLayoutEffect(() => {
    const el = cardRef.current
    if (!el || !anchor) {
      setPos(null)
      return
    }
    const place = () => {
      const gap = 8
      const margin = 12
      const { offsetWidth: w, offsetHeight: h } = el
      let left = anchor.right + gap
      if (left + w + margin > window.innerWidth) left = anchor.left - gap - w
      left = Math.min(Math.max(margin, left), Math.max(margin, window.innerWidth - w - margin))
      const top = Math.min(Math.max(margin, anchor.top - gap), Math.max(margin, window.innerHeight - h - margin))
      setPos({ left, top })
    }
    place()
    window.addEventListener('resize', place)
    return () => window.removeEventListener('resize', place)
  }, [anchor])

  // Esc 關閉：實際行為（髒表單要先確認）在 render 時塞進 ref，避免 effect 依賴整個表單狀態。
  const escRef = useRef<() => void>(() => {})
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') escRef.current()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [])

  // 換 bot 時整個表單重置（父層也給了 key，這裡是保險）。
  useEffect(() => {
    const b = useStore.getState().bots.find((x) => x.id === botId)
    if (!b) return
    setName(b.name)
    setModel(b.model)
    setEffort(b.effort)
    setFast(b.fast)
    setPersona(b.persona ?? '')
    setIdentity(b.identity ?? '')
    setBanner(null)
    setCloseConfirmOpen(false)
  }, [botId])

  if (!bot) {
    escRef.current = closeSettings
    return (
      <div className="bs-scrim" role="presentation" onMouseDown={(e) => e.target === e.currentTarget && closeSettings()}>
      <div className="bot-settings" role="dialog" aria-modal="true" aria-label="Bot 設定">
        <div className="bs-head">
          <strong>Bot 設定</strong>
          <span className="spacer" />
          <button type="button" className="icon-btn" onClick={closeSettings} aria-label="關閉設定">
            ✕
          </button>
        </div>
        <p className="msg-empty">這個 Bot 已不存在。</p>
      </div>
      </div>
    )
  }

  const nameOk = /^[^\s@,:;]{1,32}$/.test(name)

  const patch: PatchBotInput = {}
  if (name !== bot.name) patch.name = name
  if (model !== bot.model) patch.model = model
  if (bot.kind !== 'claude' && effort !== bot.effort) patch.effort = effort
  if (bot.kind === 'codex' && fast !== bot.fast) patch.fast = fast
  if ((persona.trim() || null) !== bot.persona) patch.persona = persona.trim() || null
  if (bot.kind === 'claude' && (identity || null) !== bot.identity) patch.identity = identity || null
  const changedKeys = Object.keys(patch)
  const dirty = changedKeys.length > 0
  const canSave = dirty && (patch.name === undefined || nameOk) && !saving

  const save = () => {
    if (!canSave) return
    setSaving(true)
    setBanner(null)
    void patchBot(botId, patch).then((needsRestart) => {
      setSaving(false)
      if (needsRestart === null) return
      setBanner(needsRestart ? 'restart' : 'saved')
      if (!needsRestart) notify('info', `已儲存 ${patch.name ?? bot.name} 的設定`)
    })
  }

  const requestClose = () => {
    if (dirty) {
      setCloseConfirmOpen(true)
      return
    }
    closeSettings()
  }

  escRef.current = () => {
    if (!deleteOpen && !closeConfirmOpen) requestClose()
  }

  return (
    <div className="bs-scrim" role="presentation" onMouseDown={(e) => e.target === e.currentTarget && requestClose()}>
    <div
      ref={cardRef}
      className={`bot-settings${pos ? ' anchored' : ''}${anchor && !pos ? ' measuring' : ''}`}
      style={pos ? { left: pos.left, top: pos.top } : undefined}
      role="dialog"
      aria-modal="true"
      aria-label={`${bot.name} 的設定`}
    >
      <div className="bs-head">
        <strong>Bot 設定</strong>
        <KindTag kind={bot.kind} />
        <span className="bs-sub" title={project?.path}>
          {bot.name}
          {project ? ` · ${project.label}` : ''}
        </span>
        <span className="spacer" />
        <button type="button" className="icon-btn" onClick={requestClose} aria-label="關閉設定" title="關閉，回到對話">
          ✕
        </button>
      </div>

      {banner === 'restart' ? (
        <div className="bs-banner warn" role="status">
          <span>⚠️ 已儲存，重啟 Bot 後生效（目前的 Run 仍跑在舊參數上）。</span>
          <button
            type="button"
            className="btn primary"
            disabled={restarting}
            onClick={() => {
              setRestarting(true)
              void restartBot(botId).then((ok) => {
                setRestarting(false)
                if (ok) {
                  setBanner(null)
                  notify('info', `${bot.name} 已重新啟動`)
                }
              })
            }}
          >
            {restarting ? '重啟中…' : '立即重啟'}
          </button>
        </div>
      ) : null}
      {banner === 'saved' ? (
        <div className="bs-banner ok" role="status">
          <span>✓ 已儲存（Bot 未在執行中，下次啟動就會套用）。</span>
        </div>
      ) : null}

      <div className="bs-body">
        <form
          className="form"
          id={`bot-settings-form-${botId}`}
          onSubmit={(e) => {
            e.preventDefault()
            save()
          }}
        >
          <label className="field">
            <span>名稱</span>
            <input
              ref={nameRef}
              type="text"
              value={name}
              spellCheck={false}
              onChange={(e) => setName(e.target.value)}
            />
            {name && !nameOk ? (
              <span className="hint">1–32 個字，不可含空白或 @ , : ;</span>
            ) : null}
          </label>

          {bot.kind === 'claude' ? (
            <ModelField kind={bot.kind} value={model} onChange={setModel} />
          ) : (
            <ApiModelFields
              kind={bot.kind}
              host={host}
              model={model}
              onModel={setModel}
              effort={effort}
              onEffort={setEffort}
              fast={fast}
              onFast={setFast}
            />
          )}
          <IdentityOptions kind={bot.kind} value={identity} onChange={setIdentity} />
          <PersonaField value={persona} onChange={setPersona} />
        </form>

        <div className="bs-danger">
          <div>
            <strong>刪除 Bot</strong>
            <p className="hint">會停止並關閉它的終端 pane，對話紀錄保留。</p>
          </div>
          <button type="button" className="btn danger" onClick={() => setDeleteOpen(true)}>
            刪除 Bot
          </button>
        </div>
      </div>

      <div className="bs-actions">
        <span className="hint">{dirty ? `已變更：${changedKeys.join(', ')}` : '沒有變更'}</span>
        <span className="spacer" />
        <button type="button" className="btn" onClick={requestClose}>
          關閉
        </button>
        <button type="submit" form={`bot-settings-form-${botId}`} className="btn primary" disabled={!canSave}>
          {saving ? '儲存中…' : '儲存'}
        </button>
      </div>

      <ConfirmDialog
        open={closeConfirmOpen}
        title="放棄未儲存的變更？"
        body={
          <>
            <strong>{bot.name}</strong> 的設定尚未儲存（{changedKeys.join(', ')}）。關閉將遺失這些變更。
          </>
        }
        confirmLabel="放棄並關閉"
        danger
        width={360}
        onCancel={() => setCloseConfirmOpen(false)}
        onConfirm={() => {
          setCloseConfirmOpen(false)
          closeSettings()
        }}
      />

      <ConfirmDialog
        open={deleteOpen}
        title="刪除 Bot"
        body={
          <>
            確定刪除 <strong>{bot.name}</strong>
            {project ? (
              <>
                （專案 <strong>{project.label}</strong>）
              </>
            ) : null}
            ？會停止並關閉它的終端 pane，設定從 config.toml 移除；對話紀錄會保留。
          </>
        }
        confirmLabel="刪除"
        danger
        requireText={bot.name}
        requireTextLabel={`請輸入完整名稱「${bot.name}」以確認刪除`}
        width={360}
        onCancel={() => setDeleteOpen(false)}
        onConfirm={() => {
          setDeleteOpen(false)
          void removeBot(botId)
        }}
      />
    </div>
    </div>
  )
}

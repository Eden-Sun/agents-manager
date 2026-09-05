import { useEffect, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { MODEL_CUSTOM, MODEL_DEFAULT, MODEL_OPTIONS } from '../api/types'
import type { BotKind, PatchBotInput } from '../api/types'
import { useStore } from '../store/store'

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
      return '留「（預設）」則不帶 -m，由 codex 自行決定'
    case 'grok':
      return '留「（預設）」則不帶 -m，由 grok 自行決定（`grok models`：grok-4.6 為預設）'
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
      <span>模型（daemon 會翻成 {kind === 'claude' ? <code>--model &lt;值&gt;</code> : <code>-m &lt;值&gt;</code>}）</span>
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
        <option value={MODEL_DEFAULT}>（預設）</option>
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

/** 等 stop 真的走完（run 消失或進入 stopped/exited）；mock 約 0.7s、真後端數秒。 */
async function waitStopped(botId: string, timeoutMs = 20000): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const run = useStore.getState().runs[botId]
    if (!run || run.state === 'stopped' || run.state === 'exited') return true
    if (Date.now() > deadline) return false
    await new Promise((r) => setTimeout(r, 250))
  }
}

export function BotSettingsPanel({ botId }: { botId: string }) {
  const bot = useStore((s) => s.bots.find((b) => b.id === botId) ?? null)
  const project = useStore((s) => s.projects.find((p) => p.id === s.bots.find((b) => b.id === botId)?.project_id))
  const run = useStore((s) => s.runs[botId] ?? null)
  const identities = useStore((s) => s.identities)
  const closeSettings = useStore((s) => s.closeSettings)
  const patchBot = useStore((s) => s.patchBot)
  const restartBot = useStore((s) => s.restartBot)
  const stopBot = useStore((s) => s.stopBot)
  const removeBot = useStore((s) => s.removeBot)
  const notify = useStore((s) => s.notify)

  const [name, setName] = useState(bot?.name ?? '')
  const [model, setModel] = useState<string | null>(bot?.model ?? null)
  const [identity, setIdentity] = useState(bot?.identity ?? '')
  const [autostart, setAutostart] = useState(bot?.autostart ?? false)
  const [autoApprove, setAutoApprove] = useState(bot?.auto_approve ?? true)

  const [banner, setBanner] = useState<'saved' | 'restart' | null>(null)
  const [saving, setSaving] = useState(false)
  const [stopping, setStopping] = useState(false)
  const [restarting, setRestarting] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)

  // 換 bot 時整個表單重置（父層也給了 key，這裡是保險）。
  useEffect(() => {
    const b = useStore.getState().bots.find((x) => x.id === botId)
    if (!b) return
    setName(b.name)
    setModel(b.model)
    setIdentity(b.identity ?? '')
    setAutostart(b.autostart)
    setAutoApprove(b.auto_approve)
    setBanner(null)
  }, [botId])

  if (!bot) {
    return (
      <div className="bot-settings">
        <div className="bs-head">
          <strong>Bot 設定</strong>
          <span className="spacer" />
          <button type="button" className="icon-btn" onClick={closeSettings} aria-label="關閉設定">
            ✕
          </button>
        </div>
        <p className="msg-empty">這個 Bot 已不存在。</p>
      </div>
    )
  }

  const active = run !== null && run.state !== 'stopped' && run.state !== 'exited'
  const nameOk = /^[a-z][a-z0-9_-]{0,31}$/.test(name)

  const patch: PatchBotInput = {}
  if (name !== bot.name) patch.name = name
  if (model !== bot.model) patch.model = model
  if ((identity || null) !== bot.identity) patch.identity = identity || null
  if (autostart !== bot.autostart) patch.autostart = autostart
  if (autoApprove !== bot.auto_approve) patch.auto_approve = autoApprove
  const changedKeys = Object.keys(patch)
  const dirty = changedKeys.length > 0
  const canSave = dirty && (patch.name === undefined || nameOk) && !saving

  const save = () => {
    if (!canSave) return
    setSaving(true)
    setBanner(null)
    void patchBot(botId, patch).then((needsRestart) => {
      setSaving(false)
      if (needsRestart === null) return // 失敗，原因已跳通知
      setBanner(needsRestart ? 'restart' : 'saved')
      if (!needsRestart) notify('info', `已儲存 ${patch.name ?? bot.name} 的設定`)
    })
  }

  return (
    <div className="bot-settings">
      <div className="bs-head">
        <strong>Bot 設定</strong>
        <span className={`kind-tag ${bot.kind}`}>{bot.kind}</span>
        <span className="bs-sub" title={project?.path}>
          {project?.label ?? ''}
        </span>
        <span className="spacer" />
        <button type="button" className="icon-btn" onClick={closeSettings} aria-label="關閉設定" title="關閉，回到對話">
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
          onSubmit={(e) => {
            e.preventDefault()
            save()
          }}
        >
          <label className="field">
            <span>名稱（herdr agent name，全域唯一）</span>
            <input
              ref={nameRef}
              type="text"
              value={name}
              disabled={active}
              spellCheck={false}
              onChange={(e) => setName(e.target.value.toLowerCase())}
            />
            {active ? (
              <span className="bs-inline-note">
                停止後才能改名（改名會換掉 herdr agent name）。
                <button
                  type="button"
                  className="link-btn"
                  disabled={stopping}
                  onClick={() => {
                    setStopping(true)
                    void (async () => {
                      await stopBot(botId)
                      const ok = await waitStopped(botId)
                      setStopping(false)
                      if (ok) nameRef.current?.focus()
                      else notify('error', '停止逾時，請稍後再試')
                    })()
                  }}
                >
                  {stopping ? '停止中…' : '停止並改名'}
                </button>
              </span>
            ) : name && !nameOk ? (
              <span className="hint">必須符合 [a-z][a-z0-9_-]&#123;0,31&#125;</span>
            ) : null}
          </label>

          <label className="field">
            <span>kind（建立後不可更改）</span>
            <input type="text" value={bot.kind} readOnly disabled />
          </label>

          <ModelField kind={bot.kind} value={model} onChange={setModel} hint={modelHint(bot.kind)} />

          <label className="field">
            <span>身份（只列同 kind 的身份；在左側「身份」面板管理）</span>
            <select value={identity} onChange={(e) => setIdentity(e.target.value)}>
              <option value="">（無）</option>
              {identities
                .filter((i) => i.kind === bot.kind)
                .map((i) => (
                  <option key={i.name} value={i.name}>
                    {i.name}
                    {Object.keys(i.env).length
                      ? ` ・ ${Object.entries(i.env).map(([k, v]) => `${k}=${v}`).join(' ')}`
                      : ''}
                  </option>
                ))}
            </select>
          </label>

          <label className="field row">
            <input type="checkbox" checked={autostart} onChange={(e) => setAutostart(e.target.checked)} />
            <span>autostart（daemon 啟動時自動執行）</span>
          </label>
          <label className="field row">
            <input type="checkbox" checked={autoApprove} onChange={(e) => setAutoApprove(e.target.checked)} />
            <span>
              自動核准全部權限（<AutoApproveFlags />）
            </span>
          </label>

          <div className="bs-actions">
            <span className="hint">{dirty ? `已變更：${changedKeys.join(', ')}` : '沒有變更'}</span>
            <span className="spacer" />
            <button type="button" className="btn" onClick={closeSettings}>
              關閉
            </button>
            <button type="submit" className="btn primary" disabled={!canSave}>
              {saving ? '儲存中…' : '儲存'}
            </button>
          </div>
        </form>

        <div className="bs-danger">
          <div>
            <strong>刪除 Bot</strong>
            <p className="hint">會停止並關閉它的終端 pane，對話紀錄保留。</p>
          </div>
          <button
            type="button"
            className="btn danger"
            onClick={() => {
              if (
                confirm(
                  `刪除 Bot「${bot.name}」？\n\n` +
                    '會停止並關閉它的終端 pane（有 active Run 也會先停止），設定從 config.toml 移除。\n' +
                    '對話紀錄會保留在資料庫裡。',
                )
              ) {
                void removeBot(botId)
              }
            }}
          >
            刪除 Bot
          </button>
        </div>
      </div>
    </div>
  )
}

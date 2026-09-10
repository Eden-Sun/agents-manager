import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { EFFORT_OPTIONS, effortLabel } from '../api/types'
import type { BotKind, IdentityStatus, PatchBotInput } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { identitiesOfHost, identityStatusOfHost, projectHostName, useStore } from '../store/store'
import { canLoginInSession } from '../lib/quotaLogin'
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
          <button key={e} type="button" className={`opt${value === e ? ' on' : ''}`} title={e} onClick={() => onChange(e)}>
            {effortLabel(e)}
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

/**
 * 一個身份在**目標主機**上的登入狀態，翻成 UI 用的一句話。
 *
 * `logged_in === null` 是「問不到」，不是「沒登入」——CLI 沒裝、偵測失敗、或這台主機還沒
 * 偵測過都會落在這裡，所以不出警語，免得把不知道講成壞掉。
 */
function identityWarning(st: IdentityStatus | undefined, hostLabel: string): { mark: string; title: string } | null {
  if (!st) {
    return {
      mark: '未知',
      title: `還沒有 ${hostLabel} 上這個身份的 auth status 結果。不要把它當成已登入，先按「重新偵測」或「登入」。`,
    }
  }
  if (st.logged_in === false) {
    return {
      mark: '未登入',
      title: `這個身份在 ${hostLabel} 上沒有登入：bot 起來會停在登入畫面，不會開始工作。先在 ${hostLabel} 用這個身份的設定登入，再回來按「重新偵測」。`,
    }
  }
  if (st.logged_in === null) {
    return {
      mark: '未知',
      title: `無法確認這個身份在 ${hostLabel} 的登入狀態：${st.reason ?? 'auth status 沒有可解析的結果'}。不要把它當成已登入，先修正原因或重新偵測。`,
    }
  }
  return null
}

/** 已登入時把帳號寫進 title，讓使用者一眼確認選到的是哪個帳號。 */
function identityTitle(env: Record<string, string>, st: IdentityStatus | undefined, hostLabel: string): string {
  const envText = Object.entries(env)
    .map(([k, v]) => `${k}=${v}`)
    .join(' ')
  const parts = [envText]
  if (st?.logged_in === true) {
    parts.push(`${hostLabel}：已登入${st.account ? ` — ${st.account}` : ''}${st.plan ? `（${st.plan}）` : ''}`)
  } else if (st?.logged_in === false) {
    parts.push(`${hostLabel}：未登入`)
  } else {
    parts.push(`${hostLabel}：登入狀態未知${st?.reason ? ` — ${st.reason}` : ''}`)
  }
  // 從 shell alias 認到的身份不在 config.toml 裡，改不了也刪不掉——講清楚它從哪來。
  if (st?.source === 'shell') {
    parts.push(`來自 ${hostLabel} 的 shell alias（ccN），不是 config.toml`)
  }
  return parts.filter(Boolean).join('\n')
}

/**
 * claude only: identity as a row of options（無下拉）. Renders nothing when there are no identities.
 *
 * 身份是全域設定，但它指到的帳號**每台主機各自登入**（`CLAUDE_CONFIG_DIR` 在每台機器都
 * 展得開，帳號卻不一定在），所以要標的是「這個身份在 bot 會跑的那台主機上」能不能用。
 * 未登入**不停用**按鈕：使用者可能正打算去登入（登入入口在下面的「帳號」那一段）。
 */
export function IdentityOptions({
  kind,
  host,
  value,
  onChange,
  recheck = true,
}: {
  kind: BotKind
  /** bot 會跑在哪台主機（`''` / `local` = 本機）。 */
  host: string
  value: string
  onChange: (v: string) => void
  /** 組隊三張卡並排時不重複「重新偵測」。 */
  recheck?: boolean
}) {
  // Select the stable array and filter outside: a selector that returns a fresh array
  // re-renders forever (React #185).
  const all = useStore((s) => s.identities)
  const status = useStore((s) => identityStatusOfHost(s, host))
  const refreshTools = useStore((s) => s.refreshTools)
  const busy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  // config 的身份加上這台主機 shell 裡的 `ccN`（SPEC §16）。
  const identities = useMemo(() => identitiesOfHost(all, status).filter((i) => i.kind === 'claude'), [all, status])
  if (kind !== 'claude' || identities.length === 0) return null
  const hostLabel = !host || host === 'local' ? '本機' : host
  const selectedStatus = value ? status[value] : undefined
  const selectedWarning = value ? identityWarning(selectedStatus, hostLabel) : null
  return (
    <div className="field">
      <span>
        身份
        {recheck ? (
          <button
            type="button"
            className="identity-recheck"
            disabled={busy}
            title={`重新問 ${hostLabel} 上的 CLI 每個身份是否已登入`}
            onClick={() => void refreshTools(host)}
          >
            {busy ? '偵測中…' : '重新偵測'}
          </button>
        ) : null}
      </span>
      <div className="opt-group" role="radiogroup" aria-label="identity">
        <button type="button" className={`opt${value === '' ? ' on' : ''}`} onClick={() => onChange('')}>
          不指定身分（本機預設）
        </button>
        {identities.map((i) => {
          const st = status[i.name]
          const warn = identityWarning(st, hostLabel)
          return (
            <span key={i.name} className="opt-wrap">
              <button
                type="button"
                className={`opt${value === i.name ? ' on' : ''}`}
                title={identityTitle(i.env, st, hostLabel)}
                onClick={() => onChange(i.name)}
              >
                {i.name}
              </button>
              {warn ? (
                <span className={`identity-logged-out${warn.mark === '未知' ? ' is-unknown' : ''}`} title={warn.title}>
                  {warn.mark}
                </span>
              ) : null}
            </span>
          )
        })}
      </div>
      {identities.some((i) => status[i.name]?.logged_in !== true) ? (
        <span className="hint">
          標「未登入」的身份在 {hostLabel} 上沒有帳號，選了它 bot 會停在登入畫面；標「未知」的身份則尚未確認，請先看提示原因。
        </span>
      ) : null}
      {selectedWarning ? (
        <span className={`hint identity-selection-warning${selectedWarning.mark === '未知' ? ' is-unknown' : ''}`}>
          已選「{value}」：{selectedWarning.mark === '未登入' ? '未登入，bot 會停在登入畫面。' : `登入狀態未知（${selectedStatus?.reason ?? '尚未取得 auth status 結果'}）。`}
        </span>
      ) : null}
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
  const loginBot = useStore((s) => s.loginBot)
  const refreshTools = useStore((s) => s.refreshTools)
  const loginBusy = useStore((s) => s.busy[`login:${botId}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)

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
  const [loginOpen, setLoginOpen] = useState(false)
  /** 送出過一次之後才冒出「重新偵測」——沒送過就沒有東西需要重新偵測。 */
  const [loginSent, setLoginSent] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  // 彈窗貼著觸發它的齒輪開，超出視窗才翻邊/夾住；沒有 anchor（例如鍵盤流程）就置中。
  // 手機沒有「貼著齒輪」這回事——卡片本來就跟畫面一樣寬，整張是全螢幕 sheet
  // （styles.css 的 mobile 區塊），量出來的座標只會把它推歪。
  const phone = useMediaQuery(PHONE_QUERY)
  const anchor = useStore((s) => (phone ? null : s.settingsAnchor))
  const running = useStore((s) => {
    const r = s.runs[botId]
    return r ? r.state !== 'stopped' && r.state !== 'exited' : false
  })
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

  // 點卡片外面就關（髒表單一樣先問）。以前是靠鋪滿全螢幕的 `.bs-scrim` 接這一下，代價是
  // 整個 app 被蓋住——設定開著就不能拖圖進對話、不能點旁邊的 bot。改成 document 上的
  // pointerdown：不需要任何一層擋住背景的 div，滑鼠與觸控也走同一條路。
  // 疊在上面的確認框走 portal，不在卡片裡，所以要一起放行。
  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      const card = cardRef.current
      if (!card || !(e.target instanceof Node)) return
      if (card.contains(e.target)) return
      const el = e.target instanceof Element ? e.target : e.target.parentElement
      if (el?.closest('.confirm-backdrop, .modal-backdrop')) return
      escRef.current()
    }
    document.addEventListener('pointerdown', onDown, true)
    return () => document.removeEventListener('pointerdown', onDown, true)
  }, [])

  // 開場落在名稱欄位。不再 trap 鍵盤：這張卡貼著齒輪開、背景也照常能用，把 Tab 關在裡面
  // 會跟「非模態」自相矛盾（焦點一離開就被搶回來，等於背景還是不能點）。
  useEffect(() => {
    const t = requestAnimationFrame(() => nameRef.current?.focus())
    return () => cancelAnimationFrame(t)
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
      <div className="bs-scrim" role="presentation">
      <div ref={cardRef} className="bot-settings" role="dialog" aria-label="Bot 設定">
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
  // `host` 在本機專案上可能是 `''` 也可能是字面的 `local`，兩個都得寫成「本機」——照
  // `IdentityOptions` 的同一條規則，兩處講法才一致。
  const hostLabel = !host || host === 'local' ? '本機' : host

  const patch: PatchBotInput = {}
  if (name !== bot.name) patch.name = name
  if (model !== bot.model) patch.model = model
  if (effort !== bot.effort) patch.effort = effort
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
    if (!deleteOpen && !closeConfirmOpen && !loginOpen) requestClose()
  }

  return (
    <div className="bs-scrim" role="presentation">
    <div
      ref={cardRef}
      className={`bot-settings${pos ? ' anchored' : ''}${anchor && !pos ? ' measuring' : ''}`}
      style={pos ? { left: pos.left, top: pos.top } : undefined}
      role="dialog"
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
          {/* 同上：`.bs-banner.warn` 的琥珀色已經說了這是提醒。 */}
          <span>已儲存，重啟 Bot 後生效（目前的 Run 仍跑在舊參數上）。</span>
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
          <span>
            {running
              ? '✓ 已套用，不用重啟。'
              : '✓ 已儲存（Bot 未在執行中，下次啟動就會套用）。'}
          </span>
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

          <ApiModelFields
            kind={bot.kind}
            host={host}
            identity={identity || null}
            model={model}
            onModel={setModel}
            effort={effort}
            onEffort={setEffort}
            fast={fast}
            onFast={setFast}
          />
          {/* 這一排本來會在「目前這個 run 用的身份」旁邊再插一顆底線樣式的「登入」。
              它送的 `/login` 跟下面「帳號」那顆一模一樣，卻夾在一排藥丸狀的選項中間，
              讀起來像多了一個身份可以選。留下面那顆——它有標題、有說明，也有登完之後的
              「重新偵測」。 */}
          <IdentityOptions kind={bot.kind} host={host} value={identity} onChange={setIdentity} />
          {canLoginInSession(bot.kind) ? (
            <div className="field">
              <span>帳號</span>
              <div className="bs-login-row">
                <button
                  type="button"
                  className="btn"
                  disabled={!running || loginBusy}
                  title={
                    running
                      ? `對 ${bot.name} 的 ${bot.kind} 送 /login`
                      : '這個 Bot 沒在跑，沒有畫面可以送指令'
                  }
                  onClick={() => setLoginOpen(true)}
                >
                  {loginBusy ? '送出中…' : '登入 / 切換帳號'}
                </button>
                {loginSent ? (
                  <button
                    type="button"
                    className="identity-recheck"
                    disabled={toolsBusy}
                    title={`重新問 ${hostLabel} 上的 CLI 現在登入了誰`}
                    onClick={() => void refreshTools(host)}
                  >
                    {toolsBusy ? '偵測中…' : '登入好了，重新偵測'}
                  </button>
                ) : null}
              </div>
              <span className="hint">
                {running ? (
                  <>
                    會對這個 Bot 的 {bot.kind} 送 <code>/login</code>：畫面切到登入流程，通常會跳出瀏覽器要你在那邊完成。
                    <strong>完成之前這個 Bot 不能工作。</strong>
                  </>
                ) : (
                  '這個 Bot 沒在跑，沒有畫面可以送指令。先啟動它再回來按。'
                )}
              </span>
            </div>
          ) : (
            // codex 的 TUI 只有 `/logout`——與其給一個按了必定失敗的按鈕，不如直接說要去哪裡
            // 登入。「重新偵測」還是留著：使用者在別的地方登完，回來就是按它。
            <div className="field">
              <span>帳號</span>
              <div className="bs-login-row">
                <button
                  type="button"
                  className="identity-recheck"
                  disabled={toolsBusy}
                  title={`重新問 ${hostLabel} 上的 CLI 現在登入了誰`}
                  onClick={() => void refreshTools(host)}
                >
                  {toolsBusy ? '偵測中…' : '重新偵測'}
                </button>
              </div>
              <span className="hint">
                {bot.kind} 的 TUI 沒有登入指令（只有 <code>/logout</code>），沒辦法從這裡登入。
                請在 {hostLabel} 上執行 <code>{bot.kind} login</code>，再回來按「重新偵測」。
              </span>
            </div>
          )}
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

      {/* 送 `/login` 不是「按了就登入好了」：畫面會跑到 agent 那邊，而且在使用者完成之前
          那個 Bot 不能工作。這三件事在按下去**之前**講清楚，按鈕才誠實。 */}
      <ConfirmDialog
        open={loginOpen}
        title="送出登入指令？"
        body={
          <>
            會對 <strong>{bot.name}</strong> 的 agent 送 <code>/login</code>。它的畫面會切到登入流程，通常會開瀏覽器要你在那邊完成登入。
            <br />
            <strong>在你完成登入之前，這個 Bot 不能工作</strong>——這期間送給它的訊息會卡住。
            <br />
            登入完成後回到這裡按「重新偵測」，身份狀態才會更新。
          </>
        }
        confirmLabel="送出 /login"
        width={400}
        onCancel={() => setLoginOpen(false)}
        onConfirm={() => {
          setLoginOpen(false)
          void loginBot(botId).then((ok) => {
            if (!ok) return
            setLoginSent(true)
            notify('info', `已送出 /login 給 ${bot.name}，請到它的畫面完成登入`)
          })
        }}
      />

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

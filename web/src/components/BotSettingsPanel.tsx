import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { INSTRUCTION_FILES_DEFAULT, type BotKind, type IdentityStatus, type InstructionFiles, type PatchBotInput } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { enabledIdentities, identitiesOfHost, identityStatusOfHost, projectHostName, useStore } from '../store/store'
import { canLoginInSession } from '../lib/quotaLogin'
import { ConfirmDialog } from './ConfirmDialog'
import { CopyChip } from './CopyChip'
import { KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'
import { computeBotPatch, effectiveForm, INSTRUCTION_FILES_CHOICES, pruneSaved, type BotFormKey } from './botSettingsForm'
import './botSettings.css'

/**
 * 「Bot 設定」面板（API.md v3.3）：`PATCH /api/bots/:id` 只送變更欄位，`needs_restart` 時提示重啟。
 * 使用者決定 UI 不提供 `args` / `env` / `inject_hooks`（不顯示也不進 PATCH body）。
 */

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

/** 身份在目標主機上的登入警語；`logged_in === null` 是「問不到」不是「沒登入」。 */
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
  // shell alias 身份不在 config.toml，改不了也刪不掉——講清楚來源。
  if (st?.source === 'shell') {
    parts.push(`來自 ${hostLabel} 的 shell alias（ccN），不是 config.toml`)
  }
  return parts.filter(Boolean).join('\n')
}

/**
 * Identity as a row of options（無下拉）. 帳號每台主機各自登入，所以標的是 bot 那台主機上能否用；
 * 未登入不停用按鈕（使用者可能正要去登入）。
 */
export function IdentityOptions({
  kind,
  host,
  value,
  onChange,
}: {
  kind: BotKind
  host: string
  value: string
  onChange: (v: string) => void
}) {
  // Filter outside the selector: a fresh array re-renders forever (React #185).
  const all = useStore((s) => s.identities)
  const status = useStore((s) => identityStatusOfHost(s, host))
  const refreshTools = useStore((s) => s.refreshTools)
  const busy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  // config 身份＋主機 shell 的 `ccN`（SPEC §16），依 kind 過濾（codex／grok 身份也要指派得到）。
  const disabledList = useStore((s) => s.disabledIdentities)
  // 停用的身份不出現在選單裡（SPEC §16），但**已經綁著的那個**還是要看得見，否則這顆 bot 的設定會像是空的。
  const identities = useMemo(() => {
    const ofKind = identitiesOfHost(all, status, host).filter((i) => i.kind === kind)
    const live = enabledIdentities(disabledList, host, ofKind)
    // 已經綁著的那一個就算被停用也要留著，否則這顆 bot 的設定看起來像沒選過。
    return ofKind.filter((i) => i.name === value || live.some((l) => l.name === i.name))
  }, [all, status, kind, disabledList, host, value])
  if (identities.length === 0) return null
  const hostLabel = !host || host === 'local' ? '本機' : host
  const selectedStatus = value ? status[value] : undefined
  const selectedWarning = value ? identityWarning(selectedStatus, hostLabel) : null
  return (
    <div className="field identity-field">
      <span>
        身份
        <button
          type="button"
          className="identity-recheck"
          disabled={busy}
          title={`重新問 ${hostLabel} 上的 CLI 每個身份是否已登入`}
          onClick={() => void refreshTools(host)}
        >
          {busy ? '偵測中…' : '重新偵測'}
        </button>
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
  /** 人設預設收起，已有人設才勾著（2026-09-13 使用者）。 */
  const [personaOn, setPersonaOn] = useState(Boolean(bot?.persona))
  const [identity, setIdentity] = useState(bot?.identity ?? '')
  const [instructionFiles, setInstructionFiles] = useState<InstructionFiles>(bot?.instruction_files ?? INSTRUCTION_FILES_DEFAULT)
  /** 沒動過的欄位跟著 store 走，別處改了 model／effort 時不會被開啟當下的舊值蓋回去。 */
  const [touched, setTouched] = useState<ReadonlySet<BotFormKey>>(() => new Set())
  const touch = (k: BotFormKey) =>
    setTouched((t) => {
      if (t.has(k)) return t
      const n = new Set(t)
      n.add(k)
      return n
    })

  const [banner, setBanner] = useState<'saved' | 'restart' | null>(null)
  /** 已存成功的欄位；dirty 要跟它比，store 慢一拍時才不會誤跳「放棄未儲存？」（2026-09-11）。 */
  const [saved, setSaved] = useState<PatchBotInput>({})
  const [saving, setSaving] = useState(false)
  const [restarting, setRestarting] = useState(false)
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [closeConfirmOpen, setCloseConfirmOpen] = useState(false)
  const [loginOpen, setLoginOpen] = useState(false)
  const [loginSent, setLoginSent] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  // 手機是全螢幕 sheet，不用 anchor 座標（只會推歪）。
  const phone = useMediaQuery(PHONE_QUERY)
  const anchor = useStore((s) => (phone ? null : s.settingsAnchor))
  const running = useStore((s) => {
    const r = s.runs[botId]
    return r ? r.state !== 'stopped' && r.state !== 'exited' : false
  })
  const cardRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<{ left: number; top: number; maxHeight: number } | null>(null)
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
      // 桌機水平置中（2026-09-13 使用者：「按下該要置中」）。
      const left = Math.min(Math.max(margin, (window.innerWidth - w) / 2), Math.max(margin, window.innerWidth - w - margin))
      // 不蓋到標題列與晶片列（2026-09-13 使用者）。
      const headBottom = Math.max(
        document.querySelector('.main-head')?.getBoundingClientRect().bottom ?? 0,
        document.querySelector('.unread-bar-wrap')?.getBoundingClientRect().bottom ?? 0,
        document.querySelector('.context-bar')?.getBoundingClientRect().bottom ?? 0,
      )
      const minTop = Math.max(margin, headBottom + gap)
      const top = Math.min(Math.max(minTop, anchor.top - gap), Math.max(minTop, window.innerHeight - h - margin))
      // 上緣被推到標題列底下之後，高度上限要跟著扣掉那一段——以前一律是 `100vh - 24px`，卡片一高，
      // 底下的「關閉／儲存」就被推出視窗外（2026-09-14 使用者）。內文自己捲，標題與按鈕列固定。
      setPos({ left, top, maxHeight: Math.max(240, window.innerHeight - top - margin) })
    }
    place()
    window.addEventListener('resize', place)
    return () => window.removeEventListener('resize', place)
  }, [anchor])

  // Esc 行為在 render 時塞進 ref，避免 effect 依賴整個表單狀態。
  const escRef = useRef<() => void>(() => {})
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') escRef.current()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [])

  // 點外面就關：用 document pointerdown 而非全螢幕 scrim，背景才能拖圖／點 bot。確認框走 portal，要放行。
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

  // 桌機非模態：只設開場焦點；手機全螢幕 sheet 才啟用 focus trap。
  useDialogFocus(phone, cardRef, { initialFocus: () => nameRef.current })
  useEffect(() => {
    if (phone) return
    const raf = requestAnimationFrame(() => nameRef.current?.focus())
    return () => cancelAnimationFrame(raf)
  }, [phone])

  useEffect(() => {
    const b = useStore.getState().bots.find((x) => x.id === botId)
    if (!b) return
    setName(b.name)
    setModel(b.model)
    setEffort(b.effort)
    setFast(b.fast)
    setPersona(b.persona ?? '')
    setPersonaOn(Boolean(b.persona))
    setIdentity(b.identity ?? '')
    setInstructionFiles(b.instruction_files ?? INSTRUCTION_FILES_DEFAULT)
    setTouched(new Set())
    setSaved({})
    setBanner(null)
    setCloseConfirmOpen(false)
  }, [botId])

  if (!bot) {
    escRef.current = closeSettings
    return (
      <div className="bs-scrim" role="presentation">
      <div ref={cardRef} className="bot-settings" role="dialog" aria-modal={phone ? 'true' : undefined} aria-label="Bot 設定">
        <div className="bs-head">
          <strong>Bot 設定</strong>
          <span className="spacer" />
          <button type="button" className="icon-btn bs-close" onClick={closeSettings} aria-label="關閉設定">
            ✕
          </button>
        </div>
        <p className="msg-empty">這個 Bot 已不存在。</p>
      </div>
      </div>
    )
  }

  // 名稱不做前端檢查（2026-09-09 使用者決定）：規則在 daemon，前端會漂走。
  const nameOk = name.trim().length > 0
  const hostLabel = !host || host === 'local' ? '本機' : host

  // store 追上存過的值就放掉那一欄，之後別處改了同一欄才看得到（render 期 setState：沒東西可放時回傳同一個物件）。
  const prunedSaved = pruneSaved(saved, bot)
  if (prunedSaved !== saved) setSaved(prunedSaved)

  const base = {
    name: 'name' in saved ? (saved.name ?? bot.name) : bot.name,
    model: 'model' in saved ? saved.model ?? null : bot.model,
    effort: 'effort' in saved ? saved.effort ?? null : bot.effort,
    fast: 'fast' in saved ? Boolean(saved.fast) : bot.fast,
    persona: 'persona' in saved ? saved.persona ?? null : bot.persona,
    identity: 'identity' in saved ? saved.identity ?? null : bot.identity,
    // `null` = 沒有這一格（codex／grok、舊 daemon）；存過的話 `saved` 的 null 是「清回預設」。
    instruction_files:
      bot.instruction_files === null ? null : 'instruction_files' in saved ? (saved.instruction_files ?? INSTRUCTION_FILES_DEFAULT) : bot.instruction_files,
  }

  const form = effectiveForm(base, { name, model, effort, fast, persona, identity, instruction_files: instructionFiles }, touched)
  const patch: PatchBotInput = computeBotPatch(base, form, touched, bot.kind)
  const changedKeys = Object.keys(patch)
  const dirty = changedKeys.length > 0
  const canSave = dirty && (patch.name === undefined || nameOk) && !saving

  const save = () => {
    if (!canSave) return
    setSaving(true)
    setBanner(null)
    const sent = patch
    void patchBot(botId, sent).then((needsRestart) => {
      setSaving(false)
      if (needsRestart === null) return
      setSaved((s) => ({ ...s, ...sent }))
      setTouched((t) => {
        const n = new Set(t)
        for (const k of Object.keys(sent)) n.delete(k as BotFormKey)
        return n
      })
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
      style={pos ? { left: pos.left, top: pos.top, maxHeight: pos.maxHeight } : undefined}
      role="dialog"
      aria-modal={phone ? 'true' : undefined}
      aria-label={`${bot.name} 的設定`}
    >
      <div className="bs-head">
        <strong>Bot 設定</strong>
        <KindTag kind={bot.kind} />
        <span className="bs-sub" title={project?.path}>
          {bot.name}
        </span>
        {/* 識別放標題列（2026-09-13 使用者）。 */}
        <RunIdents botId={botId} />
        <span className="spacer" />
        <button type="button" className="icon-btn bs-close" onClick={requestClose} aria-label="關閉設定" title="關閉，回到對話">
          ✕
        </button>
      </div>

      {banner === 'restart' ? (
        <div className="bs-banner warn" role="status">
          <span>已儲存，重啟 Bot 後生效（目前的 Run 仍跑在舊參數上）。</span>
          <button
            type="button"
            className="btn primary"
            disabled={restarting}
            onClick={() => {
              setRestarting(true)
              // 換過身分：重啟要接回原對話，不然對話就斷了（2026-09-17 AGM 手動搶救過兩顆）。
              // 用 `saved`（送出當下記的 patch），不是這次 render 重算的 `patch`——存完 touched 已清空，
              // 這裡再算會看不出剛剛動過 identity。
              void restartBot(botId, 'identity' in saved).then((ok) => {
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
              value={form.name}
              spellCheck={false}
              enterKeyHint="done"
              onChange={(e) => {
                touch('name')
                setName(e.target.value)
              }}
            />
          </label>

          <ApiModelFields
            kind={bot.kind}
            host={host}
            identity={form.identity || null}
            model={form.model}
            onModel={(v) => {
              touch('model')
              setModel(v)
            }}
            effort={form.effort}
            onEffort={(v) => {
              touch('effort')
              setEffort(v)
            }}
            fast={form.fast}
            onFast={(v) => {
              touch('fast')
              setFast(v)
            }}
          />
          <IdentityOptions
            kind={bot.kind}
            host={host}
            value={form.identity}
            onChange={(v) => {
              touch('identity')
              setIdentity(v)
            }}
          />
          {canLoginInSession(bot.kind) ? (
            <div className="field account-field">
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
            // codex TUI 只有 `/logout`：不給必定失敗的按鈕，改說去哪登入。
            <div className="field account-field">
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
          {/* 只有 claude 有；child bot 不是 daemon 帶 --settings 起的，daemon 會拒，所以不顯示。 */}
          {bot.kind === 'claude' && bot.managed_by === 'user' && base.instruction_files !== null ? (
            <div className="field instruction-files-field">
              <span>專案指示檔</span>
              <div className="opt-group" role="radiogroup" aria-label="專案指示檔">
                {INSTRUCTION_FILES_CHOICES.map((c) => (
                  <button
                    key={c.value}
                    type="button"
                    role="radio"
                    aria-checked={form.instruction_files === c.value}
                    className={`opt${form.instruction_files === c.value ? ' on' : ''}`}
                    title={c.hint}
                    onClick={() => {
                      touch('instruction_files')
                      setInstructionFiles(c.value)
                    }}
                  >
                    {c.label}
                  </button>
                ))}
              </div>
              <span className="hint">
                {INSTRUCTION_FILES_CHOICES.find((c) => c.value === form.instruction_files)?.hint}
                改了要重啟 Bot 才會讀到。
              </span>
            </div>
          ) : null}
          {/* 人設預設不勾（2026-09-13 使用者）；取消勾選＝清掉人設。 */}
          <label className="bs-persona-toggle">
            <input
              type="checkbox"
              checked={personaOn}
              onChange={(e) => {
                setPersonaOn(e.target.checked)
                if (!e.target.checked && form.persona) {
                  touch('persona')
                  setPersona('')
                }
              }}
            />
            人設
          </label>
          {personaOn ? (
            <PersonaField
              value={form.persona}
              onChange={(v) => {
                touch('persona')
                setPersona(v)
              }}
            />
          ) : null}
        </form>

        {/* 刪除只要一顆鍵，後果寫在確認框（2026-09-13 使用者）。 */}
        <div className="bs-danger bare">
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

      {/* `/login` 會讓 Bot 在登完前不能工作，按下去之前講清楚。 */}
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

/** 識別列只留 pane id（2026-09-09 使用者決定）：其餘在 `herdr pane list` 查得到，四顆並排會撐成兩行。 */
function RunIdents({ botId }: { botId: string }) {
  const run = useStore((s) => s.runs[botId] ?? null)
  if (!run) return null
  return (
    <div className="bs-idents" role="group" aria-label="Run 識別資訊">
      <span className="bs-idents-k">識別</span>
      <CopyChip label="pane" value={run.pane_id ?? ''} title="herdr pane id：herdr pane send / capture 用的就是它" />
    </div>
  )
}

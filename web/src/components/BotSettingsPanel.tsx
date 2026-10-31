import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'
import type { BotKind, IdentityStatus, PatchBotInput } from '../api/types'
import { PHONE_QUERY, useMediaQuery } from '../hooks/useMediaQuery'
import { useDialogFocus } from '../hooks/useDialogFocus'
import { enabledIdentities, identitiesOfHost, identityStatusOfHost, projectHostName, useStore } from '../store/store'
import { canLoginInSession } from '../lib/quotaLogin'
import { startCliLogin } from '../lib/cliLogin'
import { isChildBot } from '../lib/updateBatch'
import { registerSettingsLeaveGuard } from '../lib/settingsLeaveGuard'
import { isImeEnter } from '../lib/ime'
import { envDisplayText } from './identityEnv'
import { UnsavedGuard } from './UnsavedGuard'
import { ConfirmDialog } from './ConfirmDialog'
import { BotShareSection } from './BotShareSection'
import { CopyChip } from './CopyChip'
import { KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'
import { computeBotPatch, effectiveForm, pruneSaved, settledKeys, type BotFormKey, type BotFormValues } from './botSettingsForm'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { msg, t, useT } from '../lib/i18n'
import './botSettings.css'

/**
 * 「Bot 設定」面板（API.md v3.3）：`PATCH /api/bots/:id` 只送變更欄位，`needs_restart` 時提示重啟。
 * 使用者決定 UI 不提供 `args` / `env` / `inject_hooks`（不顯示也不進 PATCH body）。
 */

/** v4.0 人設：auto-growing textarea; empty = null. */
export function PersonaField({ value, onChange, collapsible }: { value: string; onChange: (v: string) => void; collapsible?: boolean }) {
  useT()
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
        <span className="chev">▶</span> {t('人設（選填）')}
      </button>
    )
  }
  return (
    <label className="field persona-field">
      <span>
        {t('人設')}{collapsible ? t('（選填）') : ''}
        <span className="field-note">{t('啟動時作為 system prompt 前置文字')}</span>
      </span>
      <textarea
        ref={ref}
        value={value}
        rows={2}
        placeholder={t('你是這個專案的 PM，回覆用繁體中文、先給結論')}
        onChange={(e) => onChange(e.target.value)}
      />
    </label>
  )
}

/** Small marker shown next to a bot that has a persona; hover = the first 80 chars. */
export function PersonaMark({ persona }: { persona: string | null }) {
  useT()
  if (!persona) return null
  // 以 code point 計：`slice` 會把 emoji 的代理對切成兩半（#1702）。
  const chars = Array.from(persona)
  const short = chars.length > 80 ? `${chars.slice(0, 80).join('')}…` : persona
  return (
    <span className="persona-mark" role="img" aria-label={t('有人設')} title={t('人設：{short}', { short })}>
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
      mark: msg('未知'),
      title: t('還沒有 {host} 上這個身份的 auth status 結果。不要把它當成已登入，先按「重新偵測」或「登入」。', { host: hostLabel }),
    }
  }
  if (st.logged_in === false) {
    return {
      mark: msg('未登入'),
      title: t('這個身份在 {host} 上沒有登入：bot 起來會停在登入畫面，不會開始工作。先在 {host} 用這個身份的設定登入，再回來按「重新偵測」。', { host: hostLabel }),
    }
  }
  if (st.logged_in === null) {
    return {
      mark: msg('未知'),
      title: t('無法確認這個身份在 {host} 的登入狀態：{reason}。不要把它當成已登入，先修正原因或重新偵測。', { host: hostLabel, reason: st.reason ?? t('auth status 沒有可解析的結果') }),
    }
  }
  return null
}

function identityTitle(env: Record<string, string>, st: IdentityStatus | undefined, hostLabel: string): string {
  // 身份的 env 可能放 API key／token：畫面上遮起來（tooltip 也會被截圖）。
  const envText = envDisplayText(env).split('\n').join(' ')
  const parts = [envText]
  if (st?.logged_in === true) {
    parts.push(t('{host}：已登入{account}{plan}', { host: hostLabel, account: st.account ? t(' — {account}', { account: st.account }) : '', plan: st.plan ? t('（{plan}）', { plan: st.plan }) : '' }))
  } else if (st?.logged_in === false) {
    parts.push(t('{host}：未登入', { host: hostLabel }))
  } else {
    parts.push(t('{host}：登入狀態未知{reason}', { host: hostLabel, reason: st?.reason ? ` — ${st.reason}` : '' }))
  }
  // shell alias 身份不在 config.toml，改不了也刪不掉——講清楚來源。
  if (st?.source === 'shell') {
    parts.push(t('來自 {host} 的 shell alias（ccN），不是 config.toml', { host: hostLabel }))
  }
  return parts.filter(Boolean).join('\n')
}

/**
 * 鍵盤導覽（⌥↑／⌥↓、Control+1…9、側欄列的 ↑／↓）換 bot／專案前會先問這裡（#925）：有未儲存變更就打開「放棄未儲存的變更？」
 * 並回 true 擋下。桌機設定卡非模態，`dialogOpen()` 看不到它；跟滑鼠點外面（`requestClose`）同一道門。
 */
function LeaveGuard({ dirty, onBlocked }: { dirty: boolean; onBlocked: () => void }) {
  const latest = useRef({ dirty, onBlocked })
  useEffect(() => {
    latest.current = { dirty, onBlocked }
  })
  useEffect(
    () =>
      registerSettingsLeaveGuard(() => {
        if (!latest.current.dirty) return false
        latest.current.onBlocked()
        return true
      }),
    [],
  )
  return null
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
  const t = useT()
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
  const hostLabel = !host || host === 'local' ? t('本機') : host
  const selectedStatus = value ? status[value] : undefined
  const selectedWarning = value ? identityWarning(selectedStatus, hostLabel) : null
  return (
    <div className="field identity-field">
      <span>
        {t('身份')}
        <button
          type="button"
          className="identity-recheck"
          disabled={busy}
          title={t('重新問 {host} 上的 CLI 每個身份是否已登入', { host: hostLabel })}
          onClick={() => void refreshTools(host)}
        >
          {busy ? t('偵測中…') : t('重新偵測')}
        </button>
      </span>
      <div className="opt-group" role="radiogroup" aria-label={t('身份')}>
        <button type="button" role="radio" aria-checked={value === ''} className={`opt${value === '' ? ' on' : ''}`} onClick={() => onChange('')}>
          {t('不指定身份（{account}）', { account: !host || host === 'local' ? t('本機預設') : t('{host} 的預設帳號', { host }) })}
        </button>
        {identities.map((i) => {
          const st = status[i.name]
          const warn = identityWarning(st, hostLabel)
          return (
            <span key={i.name} className="opt-wrap">
              <button
                type="button"
                role="radio"
                aria-checked={value === i.name}
                // 警示併進名稱：讀屏走到這顆就知道它沒登入（下面那個標記是給眼睛看的）。
                aria-label={warn ? t('{name}（{status}）', { name: i.name, status: t.tr(warn.mark) }) : undefined}
                className={`opt${value === i.name ? ' on' : ''}`}
                title={identityTitle(i.env, st, hostLabel)}
                onClick={() => onChange(i.name)}
              >
                {i.name}
              </button>
              {warn ? (
                <span aria-hidden="true" className={`identity-logged-out${warn.mark === msg('未知') ? ' is-unknown' : ''}`} title={warn.title}>
                  {t.tr(warn.mark)}
                </span>
              ) : null}
            </span>
          )
        })}
      </div>
      {identities.some((i) => status[i.name]?.logged_in !== true) ? (
        <span className="hint">
          {t('標「未登入」的身份在 {host} 上沒有帳號，選了它 bot 會停在登入畫面；標「未知」的身份則尚未確認，請先看提示原因。', { host: hostLabel })}
        </span>
      ) : null}
      {selectedWarning ? (
        <span className={`hint identity-selection-warning${selectedWarning.mark === msg('未知') ? ' is-unknown' : ''}`}>
          {t('已選「{identity}」：{status}', { identity: value, status: selectedWarning.mark === msg('未登入') ? t('未登入，bot 會停在登入畫面。') : t('登入狀態未知（{reason}）。', { reason: selectedStatus?.reason ?? t('尚未取得 auth status 結果') }) })}
        </span>
      ) : null}
    </div>
  )
}

export function BotSettingsPanel({ botId }: { botId: string }) {
  const t = useT()
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
  const [restartingLocal, setRestarting] = useState(false)
  // 這顆 bot 在別處（⟳ 徽章、更新徽章）已經在重啟：store 的 busy 旗標，所有重啟鈕共用。
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const child = bot ? isChildBot(bot) : false
  const restarting = restartingLocal || restartingShared
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [closeConfirmOpen, setCloseConfirmOpen] = useState(false)
  const [loginOpen, setLoginOpen] = useState(false)
  const [loginSent, setLoginSent] = useState(false)
  const [cliOpening, setCliOpening] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  const nameLabelId = useId()
  const nameHintId = useId()
  const formRef = useRef<BotFormValues>({ name: '', model: null, effort: null, fast: false, persona: '', identity: '' })
  formRef.current = { name, model, effort, fast, persona, identity }
  // 手機是全螢幕 sheet，不用 anchor 座標（只會推歪）。
  const phone = useMediaQuery(PHONE_QUERY)
  const anchor = useStore((s) => (phone ? null : s.settingsAnchor))
  const running = useStore((s) => {
    const r = s.runs[botId]
    return r ? r.state !== 'stopped' && r.state !== 'exited' : false
  })
  // codex 回合進行中改 model／effort：儲存會先按 Esc 停下回合、套用後送 continue 接著跑（#1681），儲存前先講清楚。
  const turnWorking = useStore((s) => s.runs[botId]?.agent_status === 'working')
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
      if (e.key === 'Escape' && !isImeEnter(e)) escRef.current()
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
      // 通知是疊在上面的浮層，不是「外面的畫面」：按它的 ✕／動作鈕不算離開設定卡。
      if (el?.closest('.confirm-backdrop, .modal-backdrop, .notices')) return
      escRef.current()
    }
    document.addEventListener('pointerdown', onDown, true)
    return () => document.removeEventListener('pointerdown', onDown, true)
  }, [])

  // 桌機非模態：只設開場焦點；手機全螢幕 sheet 才啟用 focus trap。
  useDialogFocus(phone, cardRef, { initialFocus: () => nameRef.current })
  // 桌機沒有 focus trap，關掉時自己還焦點：開場時的焦點（開它的鈕）；焦點已經在別處（點了別的地方）就不搶。
  useEffect(() => {
    if (phone) return
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : null
    return () => {
      const a = document.activeElement
      if (opener?.isConnected && (!a || a === document.body)) opener.focus()
    }
  }, [phone])
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
    setTouched(new Set())
    setSaved({})
    setBanner(null)
    setCloseConfirmOpen(false)
  }, [botId])

  if (!bot) {
    escRef.current = closeSettings
    return (
      <div className="bs-scrim" role="presentation">
      <div ref={cardRef} className="bot-settings" role="dialog" aria-modal={phone ? 'true' : undefined} aria-label={t('Bot 設定')}>
        <div className="bs-head">
          <strong>{t('Bot 設定')}</strong>
          <span className="spacer" />
          <button type="button" className="icon-btn bs-close" onClick={closeSettings} aria-label={t('關閉設定')}>
            ✕
          </button>
        </div>
        <p className="msg-empty">{t('這個 Bot 已不存在。')}</p>
      </div>
      </div>
    )
  }

  // 名稱不做前端檢查（2026-09-09 使用者決定）：規則在 daemon，前端會漂走。
  // 同 daemon `valid_bot_name`（送出去不前後裁空白）：不符就停用儲存，原因寫在欄位下面。
  const nameOk = isValidBotName(name)
  const nameBad = touched.has('name') && name !== '' && !nameOk
  const hostLabel = !host || host === 'local' ? t('本機') : host
  /** claude 用獨立終端跑 `claude auth login`（`startCliLogin`）；grok 仍把 `/login` 送進 Bot 的畫面。 */
  const cliLogin = bot.kind === 'claude'

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
  }

  const form = effectiveForm(base, { name, model, effort, fast, persona, identity }, touched)
  const patch: PatchBotInput = computeBotPatch(base, form, touched, bot.kind)
  const changedKeys = Object.keys(patch)
  const dirty = changedKeys.length > 0
  const canSave = dirty && (patch.name === undefined || nameOk) && !saving
  const interruptsTurn = bot.kind === 'codex' && turnWorking && (patch.model !== undefined || patch.effort !== undefined)

  const save = () => {
    if (!canSave) return
    setSaving(true)
    setBanner(null)
    const sent = patch
    void patchBot(botId, sent).then((res) => {
      setSaving(false)
      if (!res) return
      // 記 daemon **實際採用**的值，不是送出去的：停用別名會被換掉（#539），記送出的那個會讓
      // `pruneSaved` 永遠追不上，欄位就一直顯示一個沒在用的模型、還算不出 dirty。
      setSaved((s) => ({ ...s, ...sent, ...(res.remappedModel ? { model: res.remappedModel.to } : {}) }))
      const settled = settledKeys(sent, formRef.current)
      setTouched((t) => {
        const n = new Set(t)
        for (const k of settled) n.delete(k)
        return n
      })
      setBanner(res.needsRestart ? 'restart' : res.deferred ? null : 'saved')
      // 被換掉時 `store.patchBot` 已經講了一句，不要再疊一則「已儲存」。
      if (!res.needsRestart && !res.remappedModel) notify('info', t('已儲存 {name} 的設定', { name: patch.name ?? bot.name }))
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

  // 子 agent 由父 Bot 重開，`/restart` 會回 409：不給按鈕（#1911）。設定變更與「待套用」兩種橫幅共用同一顆。
  const restartButton = child ? null : (
    <button
      type="button"
      className="btn primary"
      disabled={restarting}
      onClick={() => {
        setRestarting(true)
        // 重啟一律接回原對話（store 的預設；接不回才退回開新對話）。以前只有換過身份才帶（#658），
        // 改模型等其他設定後的重啟會開新對話、整個失憶。
        void restartBot(botId).then((ok) => {
          setRestarting(false)
          if (ok) {
            setBanner(null)
            notify('info', t('{name} 已重新啟動', { name: bot.name }))
          }
        })
      }}
    >
      {restarting ? t('重啟中…') : t('立即重啟')}
    </button>
  )

  return (
    <div className="bs-scrim" role="presentation">
    <div
      ref={cardRef}
      className={`bot-settings${pos ? ' anchored' : ''}${anchor && !pos ? ' measuring' : ''}`}
      style={pos ? { left: pos.left, top: pos.top, maxHeight: pos.maxHeight } : undefined}
      role="dialog"
      aria-modal={phone ? 'true' : undefined}
      aria-label={t('{name} 的設定', { name: bot.name })}
    >
      <UnsavedGuard dirty={dirty} />
      <LeaveGuard dirty={dirty} onBlocked={() => setCloseConfirmOpen(true)} />
      <div className="bs-head">
        <strong>{t('Bot 設定')}</strong>
        <KindTag kind={bot.kind} />
        <span className="bs-sub" title={project?.path}>
          {bot.name}
        </span>
        {/* 識別放標題列（2026-09-13 使用者）。 */}
        <RunIdents botId={botId} />
        <span className="spacer" />
        <button type="button" className="icon-btn bs-close" onClick={requestClose} aria-label={t('關閉設定')} title={t('關閉，回到對話')}>
          ✕
        </button>
      </div>

      {/* 剛存完的提示之外，daemon 從資料算出「跑的還是舊設定」（#353）也要顯示：開面板時、回應掉了、重整之後都看得到。 */}
      {/* 儲存中：daemon 先寫 config 推 `bot_changed`、再當場套用（claude `/effort` 之類），這段時間 state 的
          `needs_restart` 暫時是真的；套用成功就清掉。等 PATCH 回應（daemon 的判斷）再決定要不要出重啟橫幅。 */}
      {saving ? (
        <div className="bs-banner ok" role="status">{t('套用中…')}</div>
      ) : null}
      {/* 子 agent 由父 Bot 重開，`/restart` 會回 409：不給按鈕（#1911）。 */}
      {!saving && (banner === 'restart' || (banner === null && bot.needs_restart)) ? (
        <div className="bs-banner warn" role="status">
          {child ? (
            <span>{t('已儲存，設定要重啟才生效，由父 Bot 安排重啟（目前的 Run 仍跑在舊參數上）。')}</span>
          ) : (
            <span>{t('已儲存，重啟 Bot 後生效（目前的 Run 仍跑在舊參數上）。')}</span>
          )}
          {restartButton}
        </div>
      ) : null}
      {banner === 'saved' ? (
        <div className="bs-banner ok" role="status">
          <span>
            {running
              ? t('✓ 已套用，不用重啟。')
              : t('✓ 已儲存（Bot 未在執行中，下次啟動就會套用）。')}
          </span>
        </div>
      ) : null}
      {!saving && banner === null && bot.live_apply_deferred ? (
        <div className="bs-banner ok" role="status">{t('已儲存，回合結束後自動套用。')}</div>
      ) : null}

      {bot.role_pending ? <div className="bs-banner warn" role="status"><span>待套用（需重啟）：{bot.daemon_url_pending ? '此 bot 啟動時未取得 daemon 位址。' : <>設定角色 {bot.role ?? '未分配'}；執行中的政策 {bot.runtime_role_policy ? (bot.runtime_role_policy.split(':').slice(-2).join(' / ') || '未分配') : '未知'}。</>}</span>{restartButton}</div> : null}
      {project?.mode === 'expert' ? <label>角色
        <select value={bot.role ?? ''} onChange={(e) => void patchBot(botId, { role: (e.target.value || null) as 'manager' | 'executor' | 'advisor' | null })}>
          <option value="">未分配</option><option value="executor">執行者</option><option value="advisor">顧問</option><option value="manager" disabled>管理者（在專案設定中指定）</option>
        </select>
      </label> : null}
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
            <span id={nameLabelId}>{t('名稱')}</span>
            <input
              ref={nameRef}
              type="text"
              aria-labelledby={nameLabelId}
              aria-invalid={nameBad || undefined}
              aria-describedby={nameBad ? nameHintId : undefined}
              value={form.name}
              spellCheck={false}
              enterKeyHint="done"
              onChange={(e) => {
                touch('name')
                setName(e.target.value)
              }}
            />
            {nameBad ? <span id={nameHintId} className="hint">{t.tr(BOT_NAME_HINT)}</span> : null}
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
              <span>{t('帳號')}</span>
              <div className="bs-login-row">
                <button
                  type="button"
                  className="btn"
                  disabled={cliLogin ? cliOpening : !running || loginBusy}
                  title={
                    cliLogin
                      ? t('在 {host} 開獨立終端跑 claude auth login（{identity}）', { host: hostLabel, identity: bot.identity ?? t('預設帳號') })
                      : running
                      ? t('對 {name} 的 {kind} 送 /login', { name: bot.name, kind: bot.kind })
                      : t('這個 Bot 沒在跑，沒有畫面可以送指令')
                  }
                  onClick={() => setLoginOpen(true)}
                >
                  {loginBusy || cliOpening ? t('送出中…') : t('登入 / 切換帳號')}
                </button>
                {loginSent ? (
                  <button
                    type="button"
                    className="identity-recheck"
                    disabled={toolsBusy}
                    title={t('重新問 {host} 上的 CLI 現在登入了誰', { host: hostLabel })}
                    onClick={() => void refreshTools(host)}
                  >
                    {toolsBusy ? t('偵測中…') : t('登入好了，重新偵測')}
                  </button>
                ) : null}
              </div>
              <span className="hint">
                {cliLogin ? (
                  <>
                    {t.x('會在 {host} 開一個獨立終端跑 {command}，這個 Bot 的畫面不受影響。', { host: hostLabel, command: <code>claude auth login</code> })}
                  </>
                ) : running ? (
                  <>
                    {t.x('會對這個 Bot 的 {kind} 送 {command}：畫面切到登入流程，通常會跳出瀏覽器要你在那邊完成。', { kind: bot.kind, command: <code>/login</code> })}
                    <strong>{t('完成之前這個 Bot 不能工作。')}</strong>
                  </>
                ) : (
                  t('這個 Bot 沒在跑，沒有畫面可以送指令。先啟動它再回來按。')
                )}
              </span>
            </div>
          ) : (
            // codex TUI 只有 `/logout`：不給必定失敗的按鈕，改說去哪登入。
            <div className="field account-field">
              <span>{t('帳號')}</span>
              <div className="bs-login-row">
                <button
                  type="button"
                  className="identity-recheck"
                  disabled={toolsBusy}
                  title={t('重新問 {host} 上的 CLI 現在登入了誰', { host: hostLabel })}
                  onClick={() => void refreshTools(host)}
                >
                  {toolsBusy ? t('偵測中…') : t('重新偵測')}
                </button>
              </div>
              <span className="hint">
                {t.x('{kind} 的 TUI 沒有登入指令（只有 {logout}），沒辦法從這裡登入。請在 {host} 上執行 {login}，再回來按「重新偵測」。', { kind: bot.kind, logout: <code>/logout</code>, host: hostLabel, login: <code>{bot.kind} login</code> })}
              </span>
            </div>
          )}
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
            {t('人設')}
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

        <BotShareSection botId={botId} />

        {/* 刪除只要一顆鍵，後果寫在確認框（2026-09-13 使用者）。 */}
        <div className="bs-danger bare">
          <button type="button" className="btn danger" onClick={() => setDeleteOpen(true)}>
            {t('刪除 Bot')}
          </button>
        </div>
      </div>

      {interruptsTurn ? (
        <div className="bs-banner warn" role="note">
          <span>
            {t.x('這顆正在跑回合：儲存會先按 Esc 停下目前回合，套用新設定後自動送出 {command} 接著跑（立即生效，不用等回合結束）。', { command: <code>continue</code> })}
          </span>
        </div>
      ) : null}
      <div className="bs-actions">
        <span className="hint">{dirty ? t('已變更：{keys}', { keys: changedKeys.join(', ') }) : t('沒有變更')}</span>
        <span className="spacer" />
        <button type="button" className="btn" onClick={requestClose}>
          {t('關閉')}
        </button>
        <button type="submit" form={`bot-settings-form-${botId}`} className="btn primary" disabled={!canSave}>
          {saving ? t('儲存中…') : t('儲存')}
        </button>
      </div>

      {/* `/login` 會讓 Bot 在登完前不能工作，按下去之前講清楚；claude 改用獨立終端跑 CLI，不佔 Bot 的畫面。 */}
      <ConfirmDialog
        open={loginOpen}
        title={cliLogin ? t('開終端用 CLI 登入？') : t('送出登入指令？')}
        body={
          cliLogin ? (
            <>
              {t.x('會在 {host} 開一個獨立終端，帶 {identity} 的設定跑 {command}，通常會跳出瀏覽器要你完成授權（沒跳就照終端上的網址開、把 code 貼回去）。', { host: <strong>{hostLabel}</strong>, identity: <strong>{bot.identity ?? t('預設帳號')}</strong>, command: <code>claude auth login</code> })}
              <br />
              <strong>{t('{name} 的畫面不會被登入流程佔住', { name: bot.name })}</strong>{t('；登完 CLI 自己結束{extra}。', { extra: bot.identity ? t('，終端會收掉、身份會重新偵測') : '' })}
            </>
          ) : (
          <>
            {t.x('會對 {name} 的 agent 送 {command}。它的畫面會切到登入流程，通常會開瀏覽器要你在那邊完成登入。', { name: <strong>{bot.name}</strong>, command: <code>/login</code> })}
            <br />
            <strong>{t('在你完成登入之前，這個 Bot 不能工作')}</strong>{t('——這期間送給它的訊息會卡住。')}
            <br />
            {t('登入完成後回到這裡按「重新偵測」，身份狀態才會更新。')}
          </>
          )
        }
        confirmLabel={cliLogin ? t('開終端登入') : t('送出 /login')}
        width={400}
        onCancel={() => setLoginOpen(false)}
        onConfirm={() => {
          setLoginOpen(false)
          if (cliLogin) {
            setCliOpening(true)
            void startCliLogin(botId).finally(() => setCliOpening(false))
            return
          }
          void loginBot(botId).then((ok) => {
            if (!ok) return
            setLoginSent(true)
            notify('info', t('已送出 /login 給 {name}，請到它的畫面完成登入', { name: bot.name }))
          })
        }}
      />

      <ConfirmDialog
        open={closeConfirmOpen}
        title={t('放棄未儲存的變更？')}
        body={
          <>
            {t.x('{name} 的設定尚未儲存（{keys}）。關閉將遺失這些變更。', { name: <strong>{bot.name}</strong>, keys: changedKeys.join(', ') })}
          </>
        }
        confirmLabel={t('放棄並關閉')}
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
        title={t('刪除 Bot')}
        body={
          <>
            {t.x('確定刪除 {bot}{project}？會停止並關閉它的終端 pane，設定從 config.toml 移除；對話紀錄會保留。', { bot: <strong>{bot.name}</strong>, project: project ? t.x('（專案 {name}）', { name: <strong>{project.label}</strong> }) : '' })}
          </>
        }
        confirmLabel={t('刪除')}
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
  useT()
  const run = useStore((s) => s.runs[botId] ?? null)
  if (!run) return null
  return (
    <div className="bs-idents" role="group" aria-label={t('Run 識別資訊')}>
      <span className="bs-idents-k">{t('識別')}</span>
      <CopyChip label="pane" value={run.pane_id ?? ''} title={t('herdr pane id：herdr pane send / capture 用的就是它')} />
    </div>
  )
}

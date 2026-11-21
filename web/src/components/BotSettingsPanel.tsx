import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { isBuiltinDefaultIdentity } from '../lib/defaultIdentity'
import { isApiKind } from './kindMeta'
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
import { ApiBotControls } from './ApiBotControls'
import { ClaudeApiBotLimits } from './ClaudeApiBotLimits'
import { BaseSnapshotControls } from './BaseSnapshotControls'
import { CopyChip } from './CopyChip'
import { listSchedules, rebindSchedules, type Schedule } from '../api/schedules'
import { KindTag } from './KindTag'
import { ApiModelFields } from './ModelPicker'
import { RuntimeSettingsFields } from './RuntimeSettingsFields'
import { cleanSettings, firstRangeError, sameSettings } from '../lib/runtimePolicy'
import type { RuntimeSettings } from '../api/types'
import { computeBotPatch, effectiveForm, pruneSaved, settledKeys, type BotFormKey, type BotFormValues } from './botSettingsForm'
import { BOT_NAME_HINT, isValidBotName } from '../lib/botName'
import { msg, t, useT } from '../lib/i18n'
import './botSettings.css'

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

export function PersonaMark({ persona }: { persona: string | null }) {
  useT()
  if (!persona) return null
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
  const envText = envDisplayText(env).split('\n').join(' ')
  const parts = [envText]
  if (st?.logged_in === true) {
    parts.push(t('{host}：已登入{account}{plan}', { host: hostLabel, account: st.account ? t(' — {account}', { account: st.account }) : '', plan: st.plan ? t('（{plan}）', { plan: st.plan }) : '' }))
  } else if (st?.logged_in === false) {
    parts.push(t('{host}：未登入', { host: hostLabel }))
  } else {
    parts.push(t('{host}：登入狀態未知{reason}', { host: hostLabel, reason: st?.reason ? ` — ${st.reason}` : '' }))
  }
  if (st?.source === 'shell') {
    parts.push(t('來自 {host} 的 shell alias（ccN），不是 config.toml', { host: hostLabel }))
  }
  return parts.filter(Boolean).join('\n')
}

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
  const all = useStore((s) => s.identities)
  const status = useStore((s) => identityStatusOfHost(s, host))
  const refreshTools = useStore((s) => s.refreshTools)
  const busy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)
  const disabledList = useStore((s) => s.disabledIdentities)
  const identities = useMemo(() => {
    const ofKind = identitiesOfHost(all, status, host).filter((i) => i.kind === kind)
    const live = enabledIdentities(disabledList, host, ofKind)
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
        <button type="button" role="radio" aria-checked={value === '' || isBuiltinDefaultIdentity(kind, value)} className={`opt${value === '' || isBuiltinDefaultIdentity(kind, value) ? ' on' : ''}`} onClick={() => onChange('')}>
          {kind === 'agy'
            ? t('ag0（{account}）', { account: !host || host === 'local' ? t('本機預設') : t('{host} 的預設帳號', { host }) })
            : t('不指定身份（{account}）', { account: !host || host === 'local' ? t('本機預設') : t('{host} 的預設帳號', { host }) })}
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
  const projectBots = useStore((s) => s.bots)
  const project = useStore((s) => s.projects.find((p) => p.id === s.bots.find((b) => b.id === botId)?.project_id))
  const host = useStore((s) => projectHostName(s, s.bots.find((b) => b.id === botId)?.project_id ?? null))
  const closeSettings = useStore((s) => s.closeSettings)
  const patchBot = useStore((s) => s.patchBot)
  const restartBot = useStore((s) => s.restartBot)
  const startBot = useStore((s) => s.startBot)
  const busyStart = useStore((s) => Boolean(s.busy[`start:${botId}`]))
  const removeBot = useStore((s) => s.removeBot)
  const notify = useStore((s) => s.notify)
  const loginBot = useStore((s) => s.loginBot)
  const refreshTools = useStore((s) => s.refreshTools)
  const loginBusy = useStore((s) => s.busy[`login:${botId}`] === true)
  const toolsBusy = useStore((s) => s.busy[`tools:${host || 'local'}`] === true)

  const [roleDraft, setRoleDraft] = useState<'manager' | 'executor' | 'advisor' | 'specialist' | 'scheduler' | null | undefined>(undefined)
  const [specTrack, setSpecTrack] = useState(bot?.track ?? '')
  const roleLabelId = useId()
  const trackHintId = useId()
  const trackOk = specTrack.trim() !== ''
  const roleValue = roleDraft !== undefined ? roleDraft : (bot?.role ?? null)
  const [overrideDraft, setOverrideDraft] = useState<RuntimeSettings | undefined>(undefined)
  const overrideValue: RuntimeSettings = overrideDraft !== undefined ? overrideDraft : (bot?.runtime_override ?? {})
  const [name, setName] = useState(bot?.name ?? '')
  const [model, setModel] = useState<string | null>(bot?.model ?? null)
  const [effort, setEffort] = useState<string | null>(bot?.effort ?? null)
  const [fast, setFast] = useState<boolean>(bot?.fast ?? false)
  const [persona, setPersona] = useState(bot?.persona ?? '')
  const [personaOn, setPersonaOn] = useState(Boolean(bot?.persona))
  const [identity, setIdentity] = useState(bot?.identity ?? '')
  const [touched, setTouched] = useState<ReadonlySet<BotFormKey>>(() => new Set())
  const touch = (k: BotFormKey) =>
    setTouched((t) => {
      if (t.has(k)) return t
      const n = new Set(t)
      n.add(k)
      return n
    })

  const [banner, setBanner] = useState<'saved' | 'restart' | null>(null)
  const [saved, setSaved] = useState<PatchBotInput>({})
  const [saving, setSaving] = useState(false)
  const [restartingLocal, setRestarting] = useState(false)
  const restartingShared = useStore((s) => Boolean(s.busy[`restart:${botId}`]))
  const child = bot ? isChildBot(bot) : false
  const restarting = restartingLocal || restartingShared
  const [deleteOpen, setDeleteOpen] = useState(false)
  const [deleteSchedules, setDeleteSchedules] = useState<Schedule[] | null>(null)
  const [deleteScheduleError, setDeleteScheduleError] = useState(false)
  const [deleteTarget, setDeleteTarget] = useState('')
  const [deleteBusy, setDeleteBusy] = useState(false)
  useEffect(() => {
    if (!deleteOpen) return
    let active = true
    setDeleteSchedules(null)
    setDeleteScheduleError(false)
    setDeleteTarget('')
    if (!bot?.project_id) { setDeleteSchedules([]); return }
    void listSchedules(bot.project_id).then((rows) => {
      if (active) setDeleteSchedules(rows.filter((s) => s.bot_id === botId))
    }).catch(() => { if (active) setDeleteScheduleError(true) })
    return () => { active = false }
  }, [deleteOpen, bot?.project_id, botId])
  const [closeConfirmOpen, setCloseConfirmOpen] = useState(false)
  const [loginOpen, setLoginOpen] = useState(false)
  const [loginSent, setLoginSent] = useState(false)
  const [cliOpening, setCliOpening] = useState(false)
  const nameRef = useRef<HTMLInputElement>(null)
  const nameLabelId = useId()
  const nameHintId = useId()
  const formRef = useRef<BotFormValues>({ name: '', model: null, effort: null, fast: false, persona: '', identity: '' })
  formRef.current = { name, model, effort, fast, persona, identity }
  const phone = useMediaQuery(PHONE_QUERY)
  const anchor = useStore((s) => (phone ? null : s.settingsAnchor))
  const running = useStore((s) => {
    const r = s.runs[botId]
    return r ? r.state !== 'stopped' && r.state !== 'exited' : false
  })
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
      const left = Math.min(Math.max(margin, (window.innerWidth - w) / 2), Math.max(margin, window.innerWidth - w - margin))
      const headBottom = Math.max(
        document.querySelector('.main-head')?.getBoundingClientRect().bottom ?? 0,
        document.querySelector('.unread-bar-wrap')?.getBoundingClientRect().bottom ?? 0,
        document.querySelector('.context-bar')?.getBoundingClientRect().bottom ?? 0,
      )
      const minTop = Math.max(margin, headBottom + gap)
      const top = Math.min(Math.max(minTop, anchor.top - gap), Math.max(minTop, window.innerHeight - h - margin))
      setPos({ left, top, maxHeight: Math.max(240, window.innerHeight - top - margin) })
    }
    place()
    window.addEventListener('resize', place)
    return () => window.removeEventListener('resize', place)
  }, [anchor])

  const escRef = useRef<() => void>(() => {})
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && !isImeEnter(e)) escRef.current()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [])

  useEffect(() => {
    const onDown = (e: PointerEvent) => {
      const card = cardRef.current
      if (!card || !(e.target instanceof Node)) return
      if (card.contains(e.target)) return
      const el = e.target instanceof Element ? e.target : e.target.parentElement
      if (el?.closest('.confirm-backdrop, .modal-backdrop, .notices')) return
      escRef.current()
    }
    document.addEventListener('pointerdown', onDown, true)
    return () => document.removeEventListener('pointerdown', onDown, true)
  }, [])

  useDialogFocus(phone, cardRef, { initialFocus: () => nameRef.current })
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

  const nameOk = isValidBotName(name)
  const nameBad = touched.has('name') && name !== '' && !nameOk
  const hostLabel = !host || host === 'local' ? t('本機') : host
  const cliLogin = bot.kind === 'claude'

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
  const roleChanged = roleDraft !== undefined && roleDraft !== (bot.role ?? null)
  const trackChanged = roleValue === 'specialist' && specTrack.trim() !== (bot.track ?? '')
  const roleTrackPatch: PatchBotInput = roleChanged
    ? { role: roleValue, ...(roleValue === 'specialist' ? { track: specTrack.trim() } : {}) }
    : trackChanged
      ? { track: specTrack.trim() }
      : {}
  const overrideChanged = overrideDraft !== undefined && !sameSettings(overrideDraft, bot.runtime_override ?? {})
  const overridePatch: PatchBotInput = overrideChanged
    ? { runtime_override: Object.keys(cleanSettings(overrideValue)).length > 0 ? cleanSettings(overrideValue) : null }
    : {}
  const sendPatch: PatchBotInput = { ...patch, ...roleTrackPatch, ...overridePatch }
  const changedKeys = Object.keys(sendPatch)
  const dirty = changedKeys.length > 0
  const overrideRangeBad = firstRangeError(overrideValue) !== null
  const canSave = dirty && (sendPatch.name === undefined || nameOk) && (roleValue !== 'specialist' || trackOk) && !overrideRangeBad && !saving
  const interruptsTurn = bot.kind === 'codex' && turnWorking && (sendPatch.model !== undefined || sendPatch.effort !== undefined)

  const save = () => {
    if (!canSave) return
    setSaving(true)
    setBanner(null)
    const sent = sendPatch
    void patchBot(botId, sent).then((res) => {
      setSaving(false)
      if (!res) return
      setRoleDraft(undefined)
      setOverrideDraft(undefined)
      setSaved((s) => ({ ...s, ...sent, ...(res.remappedModel ? { model: res.remappedModel.to } : {}) }))
      const settled = settledKeys(sent, formRef.current)
      setTouched((t) => {
        const n = new Set(t)
        for (const k of settled) n.delete(k)
        return n
      })
      setBanner(res.needsRestart ? 'restart' : res.deferred ? null : 'saved')
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

  const doRestart = () => {
    setRestarting(true)
    void restartBot(botId).then((ok) => {
      setRestarting(false)
      if (ok) {
        setBanner(null)
        notify('info', t('{name} 已重新啟動', { name: bot.name }))
      }
    })
  }
  const restartButton = child || isApiKind(bot.kind) ? null : (
    <button type="button" className="btn primary" disabled={restarting} onClick={doRestart}>
      {restarting ? t('重啟中…') : t('立即重啟')}
    </button>
  )
  const footerRestart = child || isApiKind(bot.kind) ? null : running ? (
    <button type="button" className="btn" disabled={restarting} onClick={doRestart}>
      {restarting ? t('重啟中…') : t('重新啟動')}
    </button>
  ) : (
    <button type="button" className="btn" disabled={busyStart} onClick={() => void startBot(botId)}>
      {t('啟動')}
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
        {}
        <RunIdents botId={botId} />
        <span className="spacer" />
        <button type="button" className="icon-btn bs-close" onClick={requestClose} aria-label={t('關閉設定')} title={t('關閉，回到對話')}>
          ✕
        </button>
      </div>

      {}
      {}
      {saving ? (
        <div className="bs-banner ok" role="status">{t('套用中…')}</div>
      ) : null}
      {}
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

      {}
      {bot.role_pending && !isApiKind(bot.kind) ? <div className="bs-banner warn" role="status"><span>待套用（需重啟）：{bot.daemon_url_pending ? '此 bot 啟動時未取得 daemon 位址。' : <>設定角色 {bot.role ?? '未分配'}；執行中的政策 {bot.runtime_role_policy ? (bot.runtime_role_policy.split(':').slice(-2).join(' / ') || '未分配') : '未知'}。</>}</span>{restartButton}</div> : null}
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

          {project?.mode === 'expert' ? (
            <div className="field role-field">
              <span id={roleLabelId}>{t('角色')}</span>
              <div className="opt-group" role="radiogroup" aria-labelledby={roleLabelId}>
                {bot.role === 'scheduler' ? <button type="button" role="radio" aria-checked={roleValue === 'scheduler'} className={`opt${roleValue === 'scheduler' ? ' on' : ''}`} onClick={() => setRoleDraft('scheduler')}>{t('排程專用')}</button> : null}
                <button type="button" role="radio" aria-checked={roleValue === null} className={`opt${roleValue === null ? ' on' : ''}`} onClick={() => setRoleDraft(null)}>{t('未分配')}</button>
                <button type="button" role="radio" aria-checked={roleValue === 'specialist'} className={`opt${roleValue === 'specialist' ? ' on' : ''}`} onClick={() => setRoleDraft('specialist')}>{t('專家')}</button>
                <button type="button" role="radio" aria-checked={roleValue === 'advisor'} className={`opt${roleValue === 'advisor' ? ' on' : ''}`} onClick={() => setRoleDraft('advisor')}>{t('顧問')}</button>
                <button type="button" role="radio" aria-checked={roleValue === 'executor'} className={`opt${roleValue === 'executor' ? ' on' : ''}`} onClick={() => setRoleDraft('executor')}>{t('執行者')}</button>
                <button type="button" role="radio" aria-checked={roleValue === 'manager'} className={`opt${roleValue === 'manager' ? ' on' : ''}`} disabled title={t('管理者在專案設定中指定')}>{t('管理者')}</button>
              </div>
            </div>
          ) : null}
          {project?.mode === 'expert' && roleValue === 'specialist' ? (
            <label className="field track-field">
              <span>{t('負責任務（track）')}</span>
              <input
                type="text"
                value={specTrack}
                placeholder={t('例如 infra、ui')}
                aria-invalid={!trackOk || undefined}
                aria-describedby={trackOk ? undefined : trackHintId}
                onChange={(e) => setSpecTrack(e.target.value)}
              />
              {trackOk ? null : <span id={trackHintId} className="hint">{t('專家必須填負責任務（track），空白不能儲存')}</span>}
            </label>
          ) : null}
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
          {isApiKind(bot.kind) ? null : <div className="field rt-override">
            <span>{t('執行期覆寫')}</span>
            <p className="hint">{t('只覆寫這顆 Bot；沒設＝跟角色（角色設定在專案設定）。改動後需重啟 Bot 才生效。')}</p>
            <RuntimeSettingsFields
              idPrefix={`bot-ov-${botId}`}
              value={overrideValue}
              inheritLabel={t('跟角色')}
              codexNote={bot.kind === 'codex'}
              disabled={saving}
              onChange={(next) => setOverrideDraft(next)}
            />
            {overrideRangeBad ? <span className="hint rp-err">{t('數值超出範圍，請修正後再儲存')}</span> : null}
          </div>}
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
          {}
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

        {isApiKind(bot.kind) ? (
          <>
            {}
            {bot.kind === 'claude-api' ? <ClaudeApiBotLimits botId={botId} /> : null}
            <ApiBotControls botId={botId} name={bot.name} />
          </>
        ) : null}
        {bot.kind === 'codex' ? <BaseSnapshotControls botId={botId} name={bot.name} /> : null}
        <BotShareSection botId={botId} />

        {}
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
        {footerRestart}
        <button type="button" className="btn" onClick={requestClose}>
          {t('關閉')}
        </button>
        <button type="submit" form={`bot-settings-form-${botId}`} className="btn primary" disabled={!canSave}>
          {saving ? t('儲存中…') : t('儲存')}
        </button>
      </div>

      {}
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
            {deleteScheduleError && <p role="alert">{t('無法載入排程，請關閉後重試。')}</p>}
            {deleteSchedules === null && !deleteScheduleError && <p>{t('載入排程中…')}</p>}
            {deleteSchedules && deleteSchedules.length > 0 && <div>
              <p>{t('此 Bot 綁定的排程：')}</p>
              <ul>{deleteSchedules.map((s) => <li key={s.id}>{s.name}</li>)}</ul>
              <label>{t('刪除前改綁到 Bot（選填）')}
                <select value={deleteTarget} onChange={(e) => setDeleteTarget(e.target.value)}>
                  <option value="">{t('不改綁')}</option>
                  {projectBots.filter((b) => b.id !== botId && b.project_id === bot.project_id && b.managed_by === 'user' && !b.parent_bot_id && !b.share_profile).map((b) => <option key={b.id} value={b.id}>{b.name}</option>)}
                </select>
              </label>
              {!deleteTarget && <p>{t('未改綁的排程將需要重新指定 Bot。')}</p>}
            </div>}
          </>
        }
        confirmLabel={t('刪除')}
        danger
        width={360}
        confirmDisabled={deleteSchedules === null || deleteScheduleError || deleteBusy}
        onCancel={() => { if (!deleteBusy) setDeleteOpen(false) }}
        onConfirm={() => {
          if (deleteBusy || !deleteSchedules) return
          setDeleteBusy(true)
          void (async () => {
            try {
              if (deleteTarget && bot.project_id) await rebindSchedules(bot.project_id, deleteSchedules.map((s) => s.id), deleteTarget)
              setDeleteOpen(false)
              await removeBot(botId)
            } catch (e) { notify('error', String(e)) }
            finally { setDeleteBusy(false) }
          })()
        }}
      />
    </div>
    </div>
  )
}

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

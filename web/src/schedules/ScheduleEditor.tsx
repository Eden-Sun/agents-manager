import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { Modal } from '../components/Modal'
import { NewBotForm } from '../components/Sidebar'
import {
  createSchedule,
  deleteSchedule,
  getSchedule,
  listSchedules,
  patchSchedule,
  runScheduleNow,
  type Schedule,
  type ScheduleDetail,
  type ScheduleMode,
  type ScheduleNotify,
  type ScheduleSpecIn,
} from '../api/schedules'
import { useStore } from '../store/store'
import { ScheduleBotPicker } from './ScheduleBotPicker'
import { createRequestId, settleCreateRequest } from '../lib/createRequestId'
import { newClientRequestId, patchBot } from '../api'
import { ApiError } from '../api/types'
import { getBaseSnapshot } from '../api/baseSnapshot'
import { isImeEnter } from '../lib/ime'
import { defaultTimeZone, formatDue, formatOccurrenceTime, OCCURRENCE_LABEL, occurrenceLine, randomSummary, timeZoneOptions } from './format'
import { useLiveTurnStatus, useOnTurnChange } from './liveTurn'
import { outcomeLabel, ScheduleResult } from './ScheduleResult'
import { formatDuration, totalDurationMs, useNowMs } from './duration'
import { copyScheduleName } from './scheduleCopy'
import { ScheduleWhenSlots } from './ScheduleWhenSlots'
import { msg, t, useT } from '../lib/i18n'
import './schedules.css'
import './scheduleEditor.css'

const WEEKDAYS = [msg('週一'), msg('週二'), msg('週三'), msg('週四'), msg('週五'), msg('週六'), msg('週日')]

interface ScheduleSlot {
  time: string
  until: string | null
}

const MAX_SLOTS = 8
const MAX_RANGE_MINUTES = 12 * 60

function secondsOfDay(v: string): number | null {
  const m = /^(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(v)
  return m ? Number(m[1]) * 3600 + Number(m[2]) * 60 + Number(m[3] ?? 0) : null
}

function defaultUntil(time: string): string {
  const start = secondsOfDay(time) ?? 9 * 3600
  const end = Math.min(start + 300, 24 * 3600 - 60)
  return `${String(Math.floor(end / 3600)).padStart(2, '0')}:${String(Math.floor((end % 3600) / 60)).padStart(2, '0')}`
}

function rangeError(s: ScheduleSlot): string | null {
  if (s.until == null) return null
  const a = secondsOfDay(s.time)
  const b = secondsOfDay(s.until)
  if (b == null) return t('隨機區間需要結束時間')
  if (a == null || b <= a) return t('結束時間要晚於開始時間（不可跨午夜）')
  if (b - a > MAX_RANGE_MINUTES * 60) return t('隨機區間最長 12 小時')
  return null
}

function nextFreeTime(slots: ScheduleSlot[]): string {
  const used = new Set(slots.map((s) => s.time))
  for (let h = 9; h < 24; h++) {
    const hm = `${String(h).padStart(2, '0')}:00`
    if (!used.has(hm)) return hm
  }
  return '09:00'
}

interface FormState {
  name: string
  prompt: string
  botId: string
  tz: string
  notify: ScheduleNotify
  mode: ScheduleMode | 'weekdays'
  at: string
  everySecs: number
  slots: ScheduleSlot[]
  weekdays: number[]
  clearAfter: boolean
  fromBase: boolean
  retryCount: number
  retryInterval: number
}

function botNameFrom(scheduleName: string): string {
  return scheduleName.replace(/[@,:;]/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 32).trim()
}

function emptyForm(botId: string): FormState {
  return { name: '', prompt: '', botId, tz: defaultTimeZone(), notify: 'failure', mode: 'daily', at: '', everySecs: 3600, slots: [{ time: '09:00', until: null }], weekdays: [0], clearAfter: false, fromBase: false, retryCount: 2, retryInterval: 1 }
}

function fromDetail(d: Schedule): FormState {
  const spec = (d.spec ?? {}) as { at?: string; every_secs?: number; time?: string; weekdays?: number[]; slots?: { time: string; label?: string; until?: string | null }[] }
  return {
    name: d.name,
    prompt: d.prompt,
    botId: d.bot_id,
    tz: d.tz,
    notify: d.notify,
    mode: d.mode === 'weekly' && spec.weekdays?.length === 5 && [0, 1, 2, 3, 4].every((day) => spec.weekdays!.includes(day)) ? 'weekdays' : d.mode,
    at: spec.at ?? '',
    everySecs: spec.every_secs || 3600,
    slots: spec.slots?.length ? spec.slots.map((s) => ({ time: s.time, until: s.until ?? null })) : [{ time: spec.time ?? '09:00', until: null }],
    weekdays: spec.weekdays ?? [0],
    clearAfter: d.clear_after === true,
    fromBase: d.from_base === true,
    retryCount: d.retry_count ?? 0, retryInterval: d.retry_interval_mins ?? 3,
  }
}

function specOf(f: FormState): ScheduleSpecIn {
  const slots = f.slots.map((s) => ({ time: s.time, ...(s.until != null ? { until: s.until } : {}) }))
  const extra = slots.length > 1 || slots.some((s) => s.until) ? { slots } : {}
  switch (f.mode) {
    case 'once':
      return { mode: 'once', at: f.at }
    case 'interval':
      return { mode: 'interval', every_secs: f.everySecs }
    case 'daily':
      return { mode: 'daily', time: slots[0]?.time ?? '', ...extra }
    case 'weekdays':
      return { mode: 'weekly', time: slots[0]?.time ?? '', weekdays: [0, 1, 2, 3, 4], ...extra }
    case 'weekly':
      return { mode: 'weekly', time: slots[0]?.time ?? '', weekdays: [...f.weekdays].sort((a, b) => a - b), ...extra }
  }
}

function minutesOfDayIn(iso: string, tz: string): number | null {
  try {
    const parts = new Intl.DateTimeFormat('en-GB', { timeZone: tz, hour: '2-digit', minute: '2-digit', hourCycle: 'h23' }).formatToParts(new Date(iso))
    const h = Number(parts.find((p) => p.type === 'hour')?.value)
    const m = Number(parts.find((p) => p.type === 'minute')?.value)
    return Number.isFinite(h) && Number.isFinite(m) ? h * 60 + m : null
  } catch {
    return null
  }
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message : t('操作失敗')
}

export function ScheduleEditor({
  projectId,
  scheduleId,
  copyOf,
  focusBot = false,
  onClose,
  onSelect,
}: {
  projectId: string
  scheduleId: string | null
  copyOf?: Schedule
  focusBot?: boolean
  onClose: () => void
  onSelect: (id: string) => void
}) {
  const t = useT()
  const bots = useStore((s) => s.bots)
  const schedulesRev = useStore((s) => s.schedulesRev)
  const selectBot = useStore((s) => s.selectBot)
  const [detail, setDetail] = useState<ScheduleDetail | null>(null)
  const recentNow = useNowMs((detail?.recent ?? []).some((o) => !!o.running_since))
  const liveStatus = useLiveTurnStatus(detail?.open?.bot_id, detail?.open?.turn_id)
  const liveTurn = useStore((s) => detail?.open?.turn_id ? s.turns[detail.open.bot_id]?.[detail.open.turn_id] : undefined)
  const [form, setForm] = useState<FormState>(() =>
    copyOf && !scheduleId ? { ...fromDetail(copyOf), name: copyScheduleName(copyOf.name) } : emptyForm(''),
  )
  const [baseline, setBaseline] = useState<{ id: string; tz: string; spec: ScheduleSpecIn } | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [botSheet, setBotSheet] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmPause, setConfirmPause] = useState(false)
  const [confirmRun, setConfirmRun] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [loading, setLoading] = useState(() => Boolean(scheduleId))
  const [createKey, setCreateKey] = useState(() => newClientRequestId())
  const detailRequestSequence = useRef(0)
  const formLoadedFor = useRef<string | null>(null)

  const loadLatestDetail = useCallback(async () => {
    if (!scheduleId) return null
    const requestId = ++detailRequestSequence.current
    try {
      const d = await getSchedule(scheduleId)
      if (detailRequestSequence.current !== requestId) return null
      setDetail(d)
      if (formLoadedFor.current !== scheduleId) {
        formLoadedFor.current = scheduleId
        const loaded = fromDetail(d)
        setForm((prev) => {
          const next = { ...loaded }
          for (const key of touchedRef.current) next[key] = prev[key] as never
          return next
        })
        setBaseline({ id: scheduleId, tz: loaded.tz, spec: specOf(loaded) })
      }
      setLoadError(null)
      setLoading(false)
      return d
    } catch (e) {
      if (detailRequestSequence.current !== requestId) return null
      if (formLoadedFor.current !== scheduleId) setLoadError(errText(e))
      setLoading(false)
      throw e
    }
  }, [scheduleId])
  const reload = useCallback(() => {
    void loadLatestDetail().catch(() => undefined)
  }, [loadLatestDetail])

  const seenScheduleRev = useRef({ scheduleId, rev: schedulesRev })
  useEffect(() => {
    const previous = seenScheduleRev.current
    seenScheduleRev.current = { scheduleId, rev: schedulesRev }
    if (previous.scheduleId === scheduleId && previous.rev !== schedulesRev) reload()
  }, [reload, scheduleId, schedulesRev])
  useOnTurnChange(liveStatus, reload)
  useEffect(() => {
    if (!scheduleId) return
    const onChanged = (ev: Event) => {
      const d = (ev as CustomEvent<{ id?: string }>).detail
      if (d?.id === scheduleId) reload()
    }
    window.addEventListener('agm:schedules_changed', onChanged)
    return () => window.removeEventListener('agm:schedules_changed', onChanged)
  }, [scheduleId, reload])

  const eligible = useMemo(
    () => bots.filter((b) => b.project_id === projectId && b.managed_by === 'user' && !b.parent_bot_id && !b.share_profile),
    [bots, projectId],
  )
  const pickable = useMemo(
    () => eligible.filter((b) => b.role === 'scheduler' || b.id === detail?.bot_id),
    [eligible, detail?.bot_id],
  )

  const [projectSchedules, setProjectSchedules] = useState<Schedule[]>([])
  useEffect(() => {
    let alive = true
    listSchedules(projectId).then((list) => alive && setProjectSchedules(list), () => alive && setProjectSchedules([]))
    return () => { alive = false }
  }, [projectId, schedulesRev])
  const botSelectRef = useRef<HTMLDivElement>(null)
  const focusedBotFor = useRef<string | null>(null)
  useEffect(() => {
    if (!focusBot || loading || !scheduleId || detail?.id !== scheduleId || focusedBotFor.current === scheduleId) return
    const frame = requestAnimationFrame(() => { (botSelectRef.current?.querySelector<HTMLButtonElement>('.sched-bot-row.selected') ?? botSelectRef.current)?.focus(); focusedBotFor.current = scheduleId })
    return () => cancelAnimationFrame(frame)
  }, [focusBot, loading, scheduleId, detail?.id])
  const touchedRef = useRef<Set<keyof FormState>>(new Set())

  useEffect(() => {
    if (!scheduleId) {
      setLoading(false)
      touchedRef.current.clear()
      formLoadedFor.current = null
      return
    }
    setLoading(true)
    touchedRef.current.clear()
    formLoadedFor.current = null
    void loadLatestDetail().catch(() => undefined)
    return () => {
      detailRequestSequence.current += 1
    }
  }, [loadLatestDetail, scheduleId])

  const effectiveBotId = form.botId || (scheduleId ? '' : (pickable[0]?.id ?? ''))
  const boundBot = detail ? (bots.find((b) => b.id === detail.bot_id) ?? null) : null
  const refreshState = useStore((s) => s.refreshState)
  const convertToDedicated = async (botId: string) => {
    try {
      await patchBot(botId, { role: 'scheduler' })
      await refreshState()
      setNotice(t('已轉成排程專屬 bot。'))
    } catch (e) {
      setError(errText(e))
    }
  }
  const [baseLocked, setBaseLocked] = useState(false)
  useEffect(() => {
    let live = true
    setBaseLocked(false)
    if (effectiveBotId) {
      getBaseSnapshot(effectiveBotId).then((s) => live && setBaseLocked(s.locked), () => undefined)
    }
    return () => {
      live = false
    }
  }, [effectiveBotId])
  const unsaved = detail !== null && (
    form.name.trim() !== detail.name || form.prompt !== detail.prompt || effectiveBotId !== detail.bot_id ||
    form.tz !== detail.tz || form.notify !== detail.notify || form.clearAfter !== (detail.clear_after === true) || form.fromBase !== (detail.from_base === true) ||
    JSON.stringify(specOf(form)) !== JSON.stringify(specOf(fromDetail(detail)))
  )
  const botUnavailable = detail !== null && (detail.bot_available === false || !eligible.some((b) => b.id === detail.bot_id))
  const changingBotWithOpen = detail?.open != null && !(detail.open.status === 'pending' && detail.open.waiting_reason === 'bot_unavailable') && effectiveBotId !== detail.bot_id
  const openIsRunning = (liveStatus ?? detail?.open?.turn_status) === 'in_flight'
  const openForDisplay = detail?.open ? {
    ...detail.open,
    turn_start_error: liveTurn ? liveTurn.startError : detail.open.turn_start_error,
    turn_awaits_start: liveTurn ? Number(liveTurn.awaitsStart) : detail.open.turn_awaits_start,
  } : null
  const hasStartError = openForDisplay?.status === 'queued' && (liveStatus ?? openForDisplay.turn_status) === 'queued' && openForDisplay.turn_awaits_start === 1 && Boolean(openForDisplay.turn_start_error)
  const lastDone = detail?.last ?? (detail?.recent ?? []).find((o) => o.status !== 'pending' && o.status !== 'queued') ?? null

  const set = <K extends keyof FormState>(k: K, v: FormState[K]) => {
    touchedRef.current.add(k)
    setForm((f) => ({ ...f, [k]: v }))
  }
  const setSlot = (i: number, patch: Partial<ScheduleSlot>) => set('slots', form.slots.map((s, j) => (j === i ? { ...s, ...patch } : s)))

  const promptRef = useRef<HTMLTextAreaElement>(null)
  useLayoutEffect(() => {
    const el = promptRef.current
    if (!el || el.scrollHeight <= el.clientHeight) return
    el.style.height = `${el.scrollHeight + (el.offsetHeight - el.clientHeight)}px`
  }, [form.prompt])

  const save = async () => {
    setError(null)
    setNotice(null)
    if (!form.name.trim()) return setError(t('請填排程名稱'))
    if (!form.prompt.trim()) return setError(t('請填任務內容'))
    if (!effectiveBotId) return setError(t('請選執行的 Bot（或建立專用 Bot）'))
    if (form.mode === 'once' && !form.at) return setError(t('單次需要日期與時間'))
    if (form.mode === 'weekly' && form.weekdays.length === 0) return setError(t('每週至少選一天'))
    if (form.mode === 'daily' || form.mode === 'weekly' || form.mode === 'weekdays') {
      if (form.slots.length === 0 || form.slots.some((s) => !s.time)) return setError(t('時段需要時間'))
      if (new Set(form.slots.map((s) => s.time)).size !== form.slots.length) return setError(t('時段時間不能重複'))
      if (form.slots.length > MAX_SLOTS) return setError(t('時段最多 {n} 個', { n: MAX_SLOTS }))
      const badRange = form.slots.map(rangeError).find((m) => m)
      if (badRange) return setError(badRange)
    }
    const retrySettings = { retry_count: form.retryCount, retry_interval_mins: form.retryInterval }
    const retryChanged = !scheduleId || form.retryCount !== (detail?.retry_count ?? 0) || form.retryInterval !== (detail?.retry_interval_mins ?? 3)
    setBusy(true)
    try {
      if (!scheduleId) {
        const s: Schedule = await createSchedule(projectId, {
          client_request_id: createKey,
          name: form.name.trim(),
          prompt: form.prompt,
          bot_id: effectiveBotId,
          tz: form.tz,
          spec: specOf(form),
          notify: form.notify,
          clear_after: form.clearAfter,
          ...(form.fromBase ? { from_base: true } : {}),
          ...(retryChanged ? retrySettings : {}),
        })
        setNotice(t('已建立。排程不會立即執行，到時間才會排進 Bot。'))
        onSelect(s.id)
      } else {
        const timing = { tz: form.tz, spec: specOf(form) }
        const same = baseline !== null && baseline.id === scheduleId && JSON.stringify(timing) === JSON.stringify({ tz: baseline.tz, spec: baseline.spec })
        const timingChanged = !same
        const d = await patchSchedule(scheduleId, {
          name: form.name.trim(),
          prompt: form.prompt,
          bot_id: effectiveBotId,
          tz: form.tz,
          ...(timingChanged ? { spec: timing.spec } : {}),
          notify: form.notify,
          clear_after: form.clearAfter,
          ...(form.fromBase !== (detail?.from_base === true) ? { from_base: form.fromBase } : {}),
          ...(retryChanged ? retrySettings : {}),
        })
        if (timingChanged) setBaseline({ id: scheduleId, ...timing })
        setDetail((prev) => (prev ? { ...prev, ...d, recent: prev.recent } : prev))
        setNotice(t('已儲存。已排的未送出執行會被撤回，已送出的照跑。'))
      }
    } catch (e) {
      setError(errText(e))
      if (!scheduleId && e instanceof ApiError && e.status === 409) setCreateKey(newClientRequestId())
    } finally {
      setBusy(false)
    }
  }

  const runKey = scheduleId ? `schedule-run:${scheduleId}` : null
  const runNow = async (dryRun = false) => {
    const key = dryRun && scheduleId ? `schedule-dry-run:${scheduleId}` : runKey
    if (!scheduleId || !key || unsaved || botUnavailable) return
    setBusy(true)
    setError(null)
    let accepted = false
    try {
      const r = await runScheduleNow(scheduleId, createRequestId(key), dryRun)
      accepted = true
      setNotice(r.occurrence ? t('已排進 Bot：{line}', { line: occurrenceLine(r.occurrence) }) : t('已有未結束的執行，沿用它'))
      if (await loadLatestDetail()) settleCreateRequest(key)
    } catch (e) {
      if (!accepted && e instanceof ApiError && e.status >= 400 && e.status < 500) settleCreateRequest(key)
      setError(errText(e))
    } finally {
      setBusy(false)
    }
  }

  const toggle = async () => {
    if (!scheduleId || !detail) return
    setBusy(true)
    setError(null)
    try {
      await patchSchedule(scheduleId, { enabled: !detail.enabled })
      await loadLatestDetail()
    } catch (e) {
      setError(errText(e))
    } finally {
      setBusy(false)
    }
  }

  const remove = async () => {
    if (!scheduleId) return
    setConfirmDelete(false)
    try {
      await deleteSchedule(scheduleId)
      onClose()
    } catch (e) {
      setError(errText(e))
    }
  }

  const otherSchedules = projectSchedules.filter((x) => x.id !== scheduleId && x.enabled)
  const sameSlot = (x: Schedule): boolean => {
    const mine = detail?.next_due ?? null
    if (mine && x.next_due) return Math.abs(Date.parse(mine) - Date.parse(x.next_due)) < 10 * 60_000
    if (!x.next_due || !(form.mode === 'daily' || form.mode === 'weekly' || form.mode === 'weekdays')) return false
    const theirs = minutesOfDayIn(x.next_due, form.tz)
    if (theirs === null) return false
    return form.slots.some((slot) => {
      if (!/^\d{2}:\d{2}$/.test(slot.time)) return false
      const [h, m] = slot.time.split(':').map(Number)
      const diff = Math.abs(theirs - (h * 60 + m))
      return Math.min(diff, 1440 - diff) < 10
    })
  }
  const conflicts = otherSchedules.filter((x) => x.bot_id === effectiveBotId && sameSlot(x))
  const conflictNote = conflicts.length > 0 ? (
    <p className="hint sched-conflict" role="status">
      {t('同一顆 Bot 會依序執行：{name} 先跑完才輪到這個，可能延遲；共用同一段對話脈絡。建議換一顆 Bot 或錯開時間（不強制）。', { name: conflicts[0].name })}
    </p>
  ) : null

  const body = (
    <div className="form schedule-editor">
      {copyOf && !scheduleId ? <p className="hint">{t('已帶入「{name}」的設定。儲存後會建立並啟用新排程，與原排程各自執行。', { name: copyOf.name })}</p> : null}
      {loading ? <p className="hint schedule-loading">{t('載入排程中…')}</p> : null}
      {loadError ? <p className="hint schedule-error">{t('讀取失敗：{reason}', { reason: loadError })}</p> : null}
      {detail ? (
        <section className="schedule-status" aria-label={t('排程狀態')}>
          <div className="schedule-glance">
            <p className="schedule-glance-next">
              <span className="schedule-glance-k">{t('下次觸發')}</span>
              <span className="schedule-glance-value">{formatDue(detail.next_due, detail.tz, randomSummary(detail.spec) !== '')}</span>
            </p>
            <p className="schedule-glance-state">
              <span className="schedule-glance-k">{t('狀態')}</span>
              <span className="schedule-glance-value">{detail.enabled ? (openForDisplay ? occurrenceLine(openForDisplay, liveStatus) : t('啟用中')) : t('已停用')}</span>
            </p>
            <p className="schedule-glance-last">
              <span className="schedule-glance-k">{t('上次結果')}</span>
              <span className="schedule-glance-value">
                {lastDone ? (
                  <>
                    {lastDone.outcome ? <span className={`schedule-outcome ${lastDone.outcome}`}>{outcomeLabel(lastDone.outcome)}</span> : null}
                    {lastDone.duration_ms != null ? t('上次 {d}', { d: formatDuration(lastDone.duration_ms) }) : t('沒有花費時間')}
                  </>
                ) : t('還沒有執行紀錄')}
              </span>
            </p>
          </div>
          <p className="schedule-save-state" role="status">{unsaved ? t('有未儲存的變更') : t('設定已儲存')}</p>
          {randomSummary(detail.spec) ? <p className="schedule-random-summary">{randomSummary(detail.spec)}</p> : null}
          {openForDisplay?.waiting_reason ? <p className="hint">{occurrenceLine(openForDisplay, liveStatus)}</p> : null}
          {hasStartError && detail.open ? <button type="button" className="btn" onClick={() => { selectBot(detail.open!.bot_id); onClose() }}>{t('開啟綁定 Bot 的對話')}</button> : null}
          {changingBotWithOpen ? <p className="schedule-change-help" role="status">{openIsRunning
            ? t('目前的執行已送出，請等它結束後再儲存新 Bot。')
            : t('目前有尚未結束的執行。若尚未送出，可先停用排程撤回，再儲存新 Bot。')}</p> : null}
        </section>
      ) : null}

      <label className="field">
        <span>{t('名稱')}</span>
        <input value={form.name} maxLength={80} disabled={loading} onChange={(e) => set('name', e.target.value)} />
      </label>

      <p className="schedule-section-title">{t('誰來執行')}</p>
      <label className="field">
        <span>{t('執行的 Bot')}</span>
        <div ref={botSelectRef} tabIndex={-1} className="schedule-bot-focus">
        <ScheduleBotPicker
          projectId={projectId}
          bots={pickable}
          selectedId={effectiveBotId}
          usageOf={(id) => otherSchedules.filter((x) => x.bot_id === id)}
          onSelect={(id) => set('botId', id)}
          disabled={loading}
        />
        </div>
      </label>
      <p className="hint">
        {t('排程一律用「排程專屬 bot」：它不算執行者、不會被派工，側欄收在「排程 bot」區。排程會排進這顆 Bot 的對話與佇列；同一網站的上班／下班可共用一顆，預設建新的。')}
      </p>
      {boundBot && boundBot.role !== 'scheduler' ? (
        <p className="hint" role="status">
          {t('這個排程綁的不是排程專屬 bot（{name}）：照常執行不受影響。可「轉成專屬」（把它標成排程專屬，之後不再當執行者），或「＋ 建立專屬 bot」改綁到新的。', { name: boundBot.name })}
        </p>
      ) : null}
      <div className="schedule-bot-actions">
        <button type="button" className="btn" disabled={loading} onClick={() => setBotSheet(true)}>
          {t('＋ 建立專屬 bot')}
        </button>
        {boundBot && boundBot.role !== 'scheduler' && effectiveBotId === boundBot.id ? (
          <button type="button" className="btn" disabled={loading} onClick={() => void convertToDedicated(boundBot.id)}>
            {t('轉成專屬')}
          </button>
        ) : null}
        {effectiveBotId ? (
          <button
            type="button"
            className="btn"
            disabled={loading}
            onClick={() => {
              selectBot(effectiveBotId)
              onClose()
            }}
          >
            {t('開啟 Bot 對話')}
          </button>
        ) : null}
      </div>

      {conflictNote}
      <p className="schedule-section-title">{t('何時執行')}</p>
      <div className="schedule-time-grid">
        <div className="schedule-when-context">
          <label className="field">
            <span>{t('模式')}</span>
            <select value={form.mode} disabled={loading} onChange={(e) => set('mode', e.target.value as FormState['mode'])}>
              <option value="once">{t('單次')}</option>
              <option value="interval">{t('固定間隔')}</option>
              <option value="daily">{t('每天')}</option>
              <option value="weekdays">{t('週一至週五')}</option>
              <option value="weekly">{t('每週')}</option>
            </select>
          </label>
          <label className="field">
            <span>{t('時區')}</span>
            <select value={form.tz} disabled={loading} onChange={(e) => set('tz', e.target.value)}>
              {timeZoneOptions(form.tz).map((z) => (
                <option key={z} value={z}>
                  {z}
                </option>
              ))}
            </select>
          </label>
        </div>
        {form.mode === 'once' ? (
          <label className="field">
            <span>{t('時間（所選時區）')}</span>
            <input type="datetime-local" value={form.at} disabled={loading} onChange={(e) => set('at', e.target.value)} />
          </label>
        ) : null}
        {form.mode === 'interval' ? (
          <label className="field">
            <span>{t('每幾分鐘（以建立時為起點，不隨任務耗時漂移）')}</span>
            <input type="number" min={1} max={525600} value={Math.round(form.everySecs / 60)} disabled={loading} onChange={(e) => set('everySecs', Number(e.target.value) * 60)} />
          </label>
        ) : null}
        {form.mode === 'daily' || form.mode === 'weekly' || form.mode === 'weekdays' ? (
          <ScheduleWhenSlots
            slots={form.slots}
            tz={form.tz}
            modeLabel={form.mode === 'daily' ? t('每天') : form.mode === 'weekdays' ? t('週一至週五') : t('每週')}
            loading={loading}
            maxSlots={MAX_SLOTS}
            errorOf={rangeError}
            onChange={setSlot}
            onToggle={(i) => setSlot(i, { until: form.slots[i].until != null ? null : defaultUntil(form.slots[i].time) })}
            onRemove={(i) => set('slots', form.slots.filter((_, j) => j !== i))}
            onAdd={() => set('slots', [...form.slots, { time: nextFreeTime(form.slots), until: null }])}
          />
        ) : null}
        {form.mode === 'weekly' ? (
          <fieldset className="field">
            <legend>{t('星期')}</legend>
            {WEEKDAYS.map((label, i) => (
              <label key={label} className="inline-check">
                <input
                  type="checkbox"
                  checked={form.weekdays.includes(i)}
                  disabled={loading}
                  onChange={(e) => set('weekdays', e.target.checked ? [...form.weekdays, i] : form.weekdays.filter((d) => d !== i))}
                />
                {t.tr(label)}
              </label>
            ))}
          </fieldset>
        ) : null}

      </div>

      <p className="schedule-section-title">{t('通知、重試與每次怎麼開始')}</p>
      <label className="field">
        <span>{t('通知')}</span>
        <select value={form.notify} disabled={loading} onChange={(e) => set('notify', e.target.value as ScheduleNotify)}>
          <option value="failure">{t('失敗、拒絕或結果不明時通知')}</option>
          <option value="all">{t('每次執行都通知')}</option>
          <option value="none">{t('不通知')}</option>
        </select>
      </label>
      <p className="hint">{t('推播跟這項一起走：每次執行都通知時，成功也會推到手機；只在失敗時通知，就只推失敗、拒絕與結果不明。不通知則不推。全站開關在環境設定的推播。')}</p>

      <fieldset className="schedule-retry">
        <legend>{t('失敗重試')}</legend>
        <div className="schedule-time-grid">
          <label className="field"><span>{t('重試次數（0 表示不重試）')}</span><input type="number" min={0} max={5} value={form.retryCount} disabled={loading} onChange={(e) => set('retryCount', Number(e.target.value))} /></label>
          <label className="field"><span>{t('重試間隔（分鐘）')}</span><input type="number" min={1} max={1440} value={form.retryInterval} disabled={loading} onChange={(e) => set('retryInterval', Number(e.target.value))} /></label>
        </div>
        <p className="hint">{t('只有明確失敗才重試；拒絕與結果不明不會重試。重試可能重複外部操作，請確認任務可安全重做。')}</p>
      </fieldset>

      <label className="inline-check schedule-clear-after">
        <input type="checkbox" checked={form.clearAfter} disabled={loading} onChange={(e) => set('clearAfter', e.target.checked)} />
        {t('跑完就清空對話（不留記憶、不佔 context）')}
      </label>
      <p className="hint">{t('每次跑完會讓這顆 Bot 開新對話；它跟你手動聊的內容也會一起清掉，建議搭配專用 Bot。')}</p>

      <label className="inline-check schedule-from-base">
        <input type="checkbox" checked={form.fromBase} disabled={loading} onChange={(e) => set('fromBase', e.target.checked)} />
        {t('每次從基準快照開始')}
      </label>
      <p className="hint">{baseLocked ? t('每次觸發前，Bot 會先重啟成「基準快照」的一份新對話再執行；基準本身不會被改動，基準不見了這次就會失敗。') : t('這顆 Bot 還沒有基準快照：儲存時會將以此 Bot 目前對話建立基準（沒有對話就先啟動它一次，最久約兩分鐘）。只支援 codex、agy Bot。')}</p>

      <label className="field schedule-prompt-field">
        <span>
          {t('任務內容')}<small className="hint schedule-prompt-hint">{t('⌘/Ctrl+Enter 儲存')}</small>
        </span>
        <textarea
          ref={promptRef}
          className="schedule-prompt"
          rows={8}
          value={form.prompt}
          disabled={loading}
          onChange={(e) => set('prompt', e.target.value)}
          onKeyDown={(e) => {
            if (busy || loading || e.key !== 'Enter' || !(e.ctrlKey || e.metaKey) || isImeEnter(e.nativeEvent)) return
            e.preventDefault()
            void save()
          }}
        />
      </label>

      {error ? <p className="hint schedule-error" role="alert">{error}</p> : null}
      {notice ? <p className="hint" role="status">{notice}</p> : null}
      {unsaved ? <p className="hint" role="status">{t('請先儲存變更，再立即執行。')}</p> : null}
      {!unsaved && botUnavailable ? <p className="hint" role="status">{t('已綁定的 Bot 無法使用，請選擇其他 Bot 並儲存。')}</p> : null}

      <div className="form-actions">
        <button type="button" className="btn primary" disabled={busy || (Boolean(scheduleId) && Boolean(loadError))} onClick={() => void save()}>
          {scheduleId ? t('儲存') : t('建立排程')}
        </button>
        {detail ? (
          <>
            <button type="button" className="btn" disabled={busy || loading || !detail.enabled || unsaved || botUnavailable} onClick={() => setConfirmRun(true)}>
              {t('立即執行')}
            </button>
            <button type="button" className="btn" disabled={busy || loading || !detail.enabled || unsaved || botUnavailable} onClick={() => void runNow(true)}>{t('試跑')}</button>
            <span className="hint schedule-action-note">{t('試跑會要求 Bot 在實際操作前停下，但不能保證不會產生實際效果。')}</span>
            <button type="button" className="btn" disabled={busy || loading} onClick={() => detail.enabled ? setConfirmPause(true) : void toggle()}>
              {detail.enabled ? t('停用') : t('啟用')}
            </button>
            <button type="button" className="btn danger" disabled={busy || loading} onClick={() => setConfirmDelete(true)}>
              {t('刪除')}
            </button>
          </>
        ) : null}
      </div>

      {detail ? (
        <details className="schedule-recent">
          <summary>
            <span>{detail.code ? `${detail.code} ` : ''}{t('最近執行（{count}）', { count: (detail.recent ?? []).length })}</span>
            {lastDone?.outcome ? <span className={`schedule-outcome ${lastDone.outcome}`}>{outcomeLabel(lastDone.outcome)}</span> : null}
            {lastDone?.duration_ms != null ? <span className="schedule-recent-duration">{t('上次 {d}', { d: formatDuration(lastDone.duration_ms) })}</span> : null}
          </summary>
          {(detail.recent ?? []).length > 0 ? (
            <ul className="schedule-history" aria-label={t('最近執行')}>
              {(detail.recent ?? []).map((o) => (
                <li key={o.id} className="schedule-history-item">
                  <p className="schedule-history-meta">
                    {o.source === 'dry_run' ? t('試跑') : o.source === 'manual' ? t('手動') : t('排程')} · {formatOccurrenceTime(o.scheduled_for, detail.tz)} · {OCCURRENCE_LABEL[o.status]}{(() => { const d = totalDurationMs(o, recentNow); return d === null ? '' : ` · ${o.status === 'pending' || o.status === 'queued' ? t('已進行 {d}', { d: formatDuration(d) }) : formatDuration(d)}` })()}
                    {o.error ? t('（{error}）', { error: o.error }) : ''}
                    {o.merged_count > 0 ? t(' · 合併 {count} 次', { count: o.merged_count }) : ''}
                  </p>
                  <ScheduleResult occurrence={o} onConversation={() => { selectBot(o.bot_id); onClose() }} />
                </li>
              ))}
            </ul>
          ) : (
            <p className="hint">{t('還沒有執行紀錄。')}</p>
          )}
        </details>
      ) : null}
    </div>
  )

  return createPortal(
    <>
      <Modal open width={760} title={scheduleId ? (detail?.code ? `${t('排程')} ${detail.code}` : t('排程')) : copyOf ? t('複製排程') : t('新增排程')} onClose={onClose}>
        {body}
      </Modal>
      <Modal open={botSheet} title={t('建立排程專屬 bot')} onClose={() => setBotSheet(false)}>
        <NewBotForm
          key={projectId}
          scheduler
          defaultName={botNameFrom(form.name)}
          initialProjectId={projectId}
          onDone={() => setBotSheet(false)}
          onCreated={(id) => setForm((f) => ({ ...f, botId: id }))}
        />
      </Modal>
      <ConfirmDialog
        open={confirmRun}
        title={t('立即執行')}
        body={<>{t('會真的執行這份任務（跟到點執行一樣）。')}</>}
        confirmLabel={t('立即執行')}
        onConfirm={() => { setConfirmRun(false); void runNow() }}
        onCancel={() => setConfirmRun(false)}
      />
      <ConfirmDialog
        open={confirmPause}
        title={t('停用排程')}
        body={<>{t('只會撤回還沒送出的排隊執行，已經在跑的會繼續。')}</>}
        confirmLabel={t('停用')}
        onConfirm={() => { setConfirmPause(false); void toggle() }}
        onCancel={() => setConfirmPause(false)}
      />
      <ConfirmDialog
        open={confirmDelete}
        title={t('刪除排程')}
        body={<>{t('刪除後不會再排新的執行。已經排進 Bot 還沒送出的會被撤回；已經送出的回合照跑。')}</>}
        confirmLabel={t('刪除')}
        danger
        onConfirm={() => void remove()}
        onCancel={() => setConfirmDelete(false)}
      />
    </>,
    document.body,
  )
}

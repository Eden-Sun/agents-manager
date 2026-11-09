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
import { ScheduleResult } from './ScheduleResult'
import { copyScheduleName } from './scheduleCopy'
import { contextAt, expandPrompt, PROMPT_VARS, unknownPromptVars } from './promptVars'
import { msg, t, useT } from '../lib/i18n'
import './schedules.css'
import './scheduleEditor.css'

const WEEKDAYS = [msg('週一'), msg('週二'), msg('週三'), msg('週四'), msg('週五'), msg('週六'), msg('週日')]

/** 一天內的一個時段：本地時間 `HH:MM` ＋ 標籤（例：「上班」）。 */
interface ScheduleSlot {
  time: string
  label: string
  /** 隨機區間的結束時間 `HH:MM`；null＝固定時間（開關關著）。 */
  until: string | null
}

const MAX_SLOTS = 8
const MAX_SLOT_LABEL = 20
const MAX_RANGE_MINUTES = 12 * 60

/** `HH:MM[:SS]` 轉一天中的秒數；格式不對回 null。 */
function secondsOfDay(v: string): number | null {
  const m = /^(\d{2}):(\d{2})(?::(\d{2}))?$/.exec(v)
  return m ? Number(m[1]) * 3600 + Number(m[2]) * 60 + Number(m[3] ?? 0) : null
}

/** 打開隨機區間時預設的結束時間：開始後 5 分鐘（不跨午夜）。 */
function defaultUntil(time: string): string {
  const start = secondsOfDay(time) ?? 9 * 3600
  const end = Math.min(start + 300, 24 * 3600 - 60)
  return `${String(Math.floor(end / 3600)).padStart(2, '0')}:${String(Math.floor((end % 3600) / 60)).padStart(2, '0')}`
}

/** 隨機區間的錯誤文字；沒問題回 null（對應 daemon 的 slot_range_* 驗證）。 */
function rangeError(s: ScheduleSlot): string | null {
  if (s.until == null) return null
  const a = secondsOfDay(s.time)
  const b = secondsOfDay(s.until)
  if (b == null) return t('隨機區間需要結束時間')
  if (a == null || b <= a) return t('結束時間要晚於開始時間（不可跨午夜）')
  if (b - a > MAX_RANGE_MINUTES * 60) return t('隨機區間最長 12 小時')
  return null
}

/** 新增時段時預設的時間：第一個還沒用到的整點（09:00 起），都用了就 09:00。 */
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
  /** 間隔的秒數：保留 API 收的精度（例如 61 秒）；分鐘欄只是顯示與改分鐘用，沒動就沿用原值。 */
  everySecs: number
  /** daily／weekly 的時段（至少一個）。 */
  slots: ScheduleSlot[]
  weekdays: number[]
  clearAfter: boolean
  fromBase: boolean
  retryCount: number
  retryInterval: number
  retryDeadline: number
}

/** 預設的專屬 bot 名稱＝排程名稱（去掉 bot 名稱不收的字元、最長 32）；清完是空的就讓表單自己排。 */
function botNameFrom(scheduleName: string): string {
  return scheduleName.replace(/[@,:;]/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 32).trim()
}

function emptyForm(botId: string): FormState {
  return { name: '', prompt: '', botId, tz: defaultTimeZone(), notify: 'failure', mode: 'daily', at: '', everySecs: 3600, slots: [{ time: '09:00', label: '', until: null }], weekdays: [0], clearAfter: false, fromBase: false, retryCount: 0, retryInterval: 3, retryDeadline: 15 }
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
    // 舊排程只有單一 `time`，讀成沒有標籤的一個時段。
    slots: spec.slots?.length ? spec.slots.map((s) => ({ time: s.time, label: s.label ?? '', until: s.until ?? null })) : [{ time: spec.time ?? '09:00', label: '', until: null }],
    weekdays: spec.weekdays ?? [0],
    clearAfter: d.clear_after === true,
    fromBase: d.from_base === true,
    retryCount: d.retry_count ?? 0, retryInterval: d.retry_interval_mins ?? 3, retryDeadline: d.retry_deadline_mins ?? 15,
  }
}

function specOf(f: FormState): ScheduleSpecIn {
  const slots = f.slots.map((s) => ({ time: s.time, label: s.label.trim(), ...(s.until != null ? { until: s.until } : {}) }))
  // 單一、沒標籤、沒隨機區間的時段仍送舊格式（只有 time），跟既有排程的 spec 一致。
  const extra = slots.length > 1 || slots.some((s) => s.label || s.until) ? { slots } : {}
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

/** 某時刻在指定時區的「一天中的分鐘數」；解析失敗回 null。 */
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

/** 新增或編輯排程。建立後只存排程，不立即執行；要執行請按「立即執行」。 */
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
  /** 複製只預填新建表單；狀態、執行紀錄與來源 id 都不帶入。 */
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
  // 綁定 turn 的即時狀態（queued→in_flight→終態）：變了就重抓詳情；排程自己的事件也重抓。
  const liveStatus = useLiveTurnStatus(detail?.open?.bot_id, detail?.open?.turn_id)
  const liveTurn = useStore((s) => detail?.open?.turn_id ? s.turns[detail.open.bot_id]?.[detail.open.turn_id] : undefined)
  const [form, setForm] = useState<FormState>(() =>
    copyOf && !scheduleId ? { ...fromDetail(copyOf), name: copyScheduleName(copyOf.name) } : emptyForm(''),
  )
  // 載入當下的排程（id）、時區與時間（`specOf`）：存檔時跟表單比，沒變就不送 spec（#1807）。
  // 帶 id：換了排程、還沒載入完成時對不上，保守送 spec（#1812）。
  const [baseline, setBaseline] = useState<{ id: string; tz: string; spec: ScheduleSpecIn } | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [botSheet, setBotSheet] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [confirmPause, setConfirmPause] = useState(false)
  const [confirmRun, setConfirmRun] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  // 建立的冪等 key：重送同一張表單用同一個；後端說 key 已用在別的內容上才換新的。
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
      // 若初次載入被較新的重抓取代，由第一筆最新成功的回應初始化表單。
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

  // 重連／resync 時重抓詳情；只更新 detail，不覆蓋使用者正在編輯的 form。
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

  // 只列能接收排程的 Bot：同專案、一般 Bot、非 child／分享／受限（daemon 也會再驗一次）。
  const eligible = useMemo(
    () => bots.filter((b) => b.project_id === projectId && b.managed_by === 'user' && !b.parent_bot_id && !b.share_profile),
    [bots, projectId],
  )
  // 一律用排程專屬 bot（role = scheduler）；只有既有排程綁著的一般 Bot 例外（照常執行，旁邊提示轉成專屬）。
  const pickable = useMemo(
    () => eligible.filter((b) => b.role === 'scheduler' || b.id === detail?.bot_id),
    [eligible, detail?.bot_id],
  )

  const [loading, setLoading] = useState(() => Boolean(scheduleId))
  // 同專案的其他排程：挑 Bot 時看它已經被哪些排程用、有沒有時間衝突（只讀，不影響存檔）。
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

  // 建立時預設第一顆可用 Bot（之後使用者可改）：推導出來，不用 effect 寫回表單。
  const effectiveBotId = form.botId || (scheduleId ? '' : (pickable[0]?.id ?? ''))
  // 既有排程綁的 Bot（可能是一般 Bot／執行者）：提示轉成專屬。
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
  // 選到的 Bot 有基準快照才能開「每次從基準快照開始」。
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
  // 執行端只讀已存的排程；表單有變動時不得用舊 Bot 或舊內容執行。
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

  const set = <K extends keyof FormState>(k: K, v: FormState[K]) => {
    touchedRef.current.add(k)
    setForm((f) => ({ ...f, [k]: v }))
  }
  const setSlot = (i: number, patch: Partial<ScheduleSlot>) => set('slots', form.slots.map((s, j) => (j === i ? { ...s, ...patch } : s)))
  // 點變數：插在游標處（沒有游標就接在最後）。
  const insertVar = (v: string) => {
    const token = `{{${v}}}`
    const el = promptRef.current
    const start = el?.selectionStart ?? form.prompt.length
    const end = el?.selectionEnd ?? start
    set('prompt', form.prompt.slice(0, start) + token + form.prompt.slice(end))
    requestAnimationFrame(() => {
      el?.focus()
      el?.setSelectionRange(start + token.length, start + token.length)
    })
  }


  // 任務內容框只在內容超出目前高度時長高（最少 8 行由 rows 決定，最多 60vh 由 CSS 限制）；使用者手動拉高的不會被縮回去。
  const promptRef = useRef<HTMLTextAreaElement>(null)
  const [showPreview, setShowPreview] = useState(false)
  useLayoutEffect(() => {
    const el = promptRef.current
    if (!el || el.scrollHeight <= el.clientHeight) return
    el.style.height = `${el.scrollHeight + (el.offsetHeight - el.clientHeight)}px`
  }, [form.prompt])

  const botLabel = (id: string) => {
    const b = bots.find((x) => x.id === id)
    if (!b) return t('（找不到這顆 Bot）')
    return `${b.name} · ${b.kind}${b.model ? ` · ${b.model}` : ''}${b.effort ? ` · ${b.effort}` : ''}`
  }

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
      if (form.slots.some((s) => [...s.label.trim()].length > MAX_SLOT_LABEL)) return setError(t('時段標籤不能超過 {n} 字', { n: MAX_SLOT_LABEL }))
    }
    const retrySettings = { retry_count: form.retryCount, retry_interval_mins: form.retryInterval, retry_deadline_mins: form.retryDeadline }
    const retryChanged = form.retryCount !== (detail?.retry_count ?? 0) || form.retryInterval !== (detail?.retry_interval_mins ?? 3) || form.retryDeadline !== (detail?.retry_deadline_mins ?? 15)
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
        // 只有排程時間真的變了才送 spec：後端每收到一次 spec 就重新錨定間隔、升 generation（#1807）。
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
        // 儲存回來的是 Schedule（不帶 recent）：合進既有詳情，不取代成缺欄位的物件。
        setDetail((prev) => (prev ? { ...prev, ...d, recent: prev.recent } : prev))
        setNotice(t('已儲存。已排的未送出執行會被撤回，已送出的照跑。'))
      }
    } catch (e) {
      setError(errText(e))
      // 409 ＝同一個冪等 key 已用在別的內容上：換新 key 再送（同內容的重送本來就會回原筆）。
      if (!scheduleId && e instanceof ApiError && e.status === 409) setCreateKey(newClientRequestId())
    } finally {
      setBusy(false)
    }
  }

  // 手動執行的冪等 key 走共用的 createRequestId（存在 sessionStorage，重整、關掉再開也沿用）：
  // 回應遺失、網路錯誤後再按，送的是同一個 key，伺服器回原來那一筆，不會再跑一次。
  // 伺服器收下（回應成功）才 settle，下一次明確的手動執行才換新的；明確的 4xx 拒絕也 settle（不能讓舊 key 重放舊結果）。
  const runKey = scheduleId ? `schedule-run:${scheduleId}` : null
  const runNow = async (dryRun = false) => {
    const key = dryRun && scheduleId ? `schedule-dry-run:${scheduleId}` : runKey
    if (!scheduleId || !key || unsaved || botUnavailable) return
    setBusy(true)
    setError(null)
    // 伺服器收下（POST 成功）之後，key 仍保留到詳情也重抓成功才 settle：詳情 503 時再按，送的還是同一個 key，
    // 伺服器回同一筆 occurrence。只有 POST 本身的 4xx 拒絕才 settle（那次根本沒被收下）。
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
  // 同一顆 Bot、同一時刻或相差 < 10 分：已有 next_due 就比絕對時間；新排程用表單的每天時刻比（週次只比時刻，較粗）。
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
          <p>{t('已儲存的 Bot：{bot}', { bot: botLabel(detail.bot_id) })}</p>
          <p className="schedule-save-state" role="status">{unsaved ? t('有未儲存的變更') : t('設定已儲存')}</p>
          {randomSummary(detail.spec) ? <p className="schedule-random-summary">{randomSummary(detail.spec)}</p> : null}
          <p>{t('下次：{time}', { time: formatDue(detail.next_due, detail.tz, randomSummary(detail.spec) !== '') })}</p>
          <p>{t('狀態：{status}', { status: detail.enabled ? (openForDisplay ? occurrenceLine(openForDisplay, liveStatus) : t('啟用中')) : t('已停用') })}</p>
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
              // 到 Bot 對話看執行與額度、離線原因；不另建 API。
              selectBot(effectiveBotId)
              onClose()
            }}
          >
            {t('開啟 Bot 對話')}
          </button>
        ) : null}
      </div>

      {conflictNote}
      <div className="schedule-time-grid">
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
          <fieldset className="field schedule-slots">
            <legend>{t('時間（所選時區）')}</legend>
            {form.slots.map((slot, i) => (
              <div key={i} className="schedule-slot-row">
                <input type="time" aria-label={t('第 {n} 個時段的時間', { n: i + 1 })} value={slot.time} disabled={loading} onChange={(e) => setSlot(i, { time: e.target.value })} />
                <input type="text" maxLength={MAX_SLOT_LABEL} placeholder={t('標籤（選填）')} aria-label={t('第 {n} 個時段的標籤', { n: i + 1 })} value={slot.label} disabled={loading} onChange={(e) => setSlot(i, { label: e.target.value })} />
                <button type="button" className="schedule-link" disabled={loading || form.slots.length <= 1} onClick={() => set('slots', form.slots.filter((_, j) => j !== i))}>{t('刪除時段')}</button>
                <div className="schedule-slot-range">
                  <label className="schedule-slot-random">
                    <input type="checkbox" role="switch" aria-label={t('第 {n} 個時段的隨機區間', { n: i + 1 })} checked={slot.until != null} disabled={loading} onChange={(e) => setSlot(i, { until: e.target.checked ? defaultUntil(slot.time) : null })} />
                    <span>{t('隨機區間')}</span>
                  </label>
                  {slot.until != null ? (
                    <label className="schedule-slot-until">
                      <span>{t('結束時間')}</span>
                      <input type="time" aria-label={t('第 {n} 個時段的結束時間', { n: i + 1 })} value={slot.until} disabled={loading} onChange={(e) => setSlot(i, { until: e.target.value })} />
                    </label>
                  ) : null}
                  {slot.until != null && rangeError(slot) ? <p className="hint schedule-error" role="alert">{rangeError(slot)}</p> : null}
                  {slot.until != null && !rangeError(slot) ? <p className="hint">{t('{range} 隨機', { range: `${slot.time}–${slot.until}` })}</p> : null}
                </div>
              </div>
            ))}
            <button type="button" className="btn" disabled={loading || form.slots.length >= MAX_SLOTS} onClick={() => set('slots', [...form.slots, { time: nextFreeTime(form.slots), label: '', until: null }])}>{t('新增時段')}</button>
          </fieldset>
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

      <label className="field">
        <span>{t('通知')}</span>
        <select value={form.notify} disabled={loading} onChange={(e) => set('notify', e.target.value as ScheduleNotify)}>
          <option value="failure">{t('失敗、拒絕或結果不明時通知')}</option>
          <option value="all">{t('每次執行都通知')}</option>
          <option value="none">{t('不通知')}</option>
        </select>
      </label>

      <fieldset className="schedule-retry">
        <legend>{t('失敗重試')}</legend>
        <div className="schedule-time-grid">
          <label className="field"><span>{t('重試次數（0 表示不重試）')}</span><input type="number" min={0} max={5} value={form.retryCount} disabled={loading} onChange={(e) => set('retryCount', Number(e.target.value))} /></label>
          <label className="field"><span>{t('重試間隔（分鐘）')}</span><input type="number" min={1} max={1440} value={form.retryInterval} disabled={loading} onChange={(e) => set('retryInterval', Number(e.target.value))} /></label>
          <label className="field"><span>{t('距排定時間的截止（分鐘）')}</span><input type="number" min={1} max={10080} value={form.retryDeadline} disabled={loading} onChange={(e) => set('retryDeadline', Number(e.target.value))} /></label>
        </div>
        <p className="hint">{t('只有明確失敗才重試；拒絕與結果不明不會重試。重試可能重複外部操作，請確認任務可安全重做。')}</p>
      </fieldset>

      <label className="inline-check schedule-clear-after">
        <input type="checkbox" checked={form.clearAfter} disabled={loading} onChange={(e) => set('clearAfter', e.target.checked)} />
        {t('跑完就清空對話（不留記憶、不佔 context）')}
      </label>
      <p className="hint">{t('每次跑完會讓這顆 Bot 開新對話；它跟你手動聊的內容也會一起清掉，建議搭配專用 Bot。')}</p>

      <label className="inline-check schedule-from-base">
        <input type="checkbox" checked={form.fromBase} disabled={loading || (!form.fromBase && !baseLocked)} onChange={(e) => set('fromBase', e.target.checked)} />
        {t('每次從基準快照開始')}
      </label>
      <p className="hint">{baseLocked ? t('每次觸發前，Bot 會先重啟成「基準快照」的一份新對話再執行；基準本身不會被改動，基準不見了這次就會失敗。') : t('只有已鎖定基準快照的 codex Bot 才能開啟：先到該 Bot 的設定按「鎖定目前 context 為基準」。')}</p>

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
            // 跟「儲存」按鈕同一支函式；輸入法選字中的 Enter 不算。
            if (busy || loading || e.key !== 'Enter' || !(e.ctrlKey || e.metaKey) || isImeEnter(e.nativeEvent)) return
            e.preventDefault()
            void save()
          }}
        />
      </label>
      <div className="schedule-vars">
        <span className="hint">{t('變數（點了插入）')}</span>
        <div className="schedule-var-chips">
          {PROMPT_VARS.map((v) => (
            <button type="button" key={v} className="schedule-var-chip" disabled={loading} onClick={() => insertVar(v)}>{`{{${v}}}`}</button>
          ))}
        </div>
        {unknownPromptVars(form.prompt).length > 0 ? (
          <p className="hint schedule-var-warn" role="alert">
            {t('不認得的變數 {vars} 會原樣保留', { vars: unknownPromptVars(form.prompt).map((v) => `{{${v}}}`).join('、') })}
          </p>
        ) : null}
        <button type="button" className="schedule-link" aria-expanded={showPreview} onClick={() => setShowPreview((v) => !v)}>{t('預覽展開結果')}</button>
        {showPreview ? (
          <p className="schedule-preview" aria-label={t('預覽展開結果')}>
            {expandPrompt(form.prompt, contextAt(form.tz, new Date(), form.name, form.slots[0]?.label.trim() ?? ''))}
          </p>
        ) : null}
        {showPreview ? <p className="hint">{t('預覽用現在的時間與第一個時段的標籤；實際送出時用該次觸發的時間。')}</p> : null}
      </div>


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
            <span className="hint">{t('試跑會要求 Bot 在實際操作前停下，但不能保證不會產生實際效果。')}</span>
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
          <summary>{t('最近執行（{count}）', { count: (detail.recent ?? []).length })}</summary>
          {(detail.recent ?? []).length > 0 ? (
            <ul className="schedule-history" aria-label={t('最近執行')}>
              {(detail.recent ?? []).map((o) => (
                <li key={o.id}>
                  {o.source === 'dry_run' ? t('試跑') : o.source === 'manual' ? t('手動') : t('排程')} · {formatOccurrenceTime(o.scheduled_for, detail.tz)}{o.slot_label ? ` · ${o.slot_label}` : ''} · {OCCURRENCE_LABEL[o.status]}
                  <ScheduleResult occurrence={o} onConversation={() => { selectBot(o.bot_id); onClose() }} />
                  {o.error ? t('（{error}）', { error: o.error }) : ''}
                  {o.merged_count > 0 ? t(' · 合併 {count} 次', { count: o.merged_count }) : ''}
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

  // 掛到 body：側欄是 transform 容器，裡面的 fixed 遮罩會被它限在側欄寬度內（手機上彈窗被裁）。只這裡處理，不動全站 Modal。
  return createPortal(
    <>
      <Modal open width={760} title={scheduleId ? t('排程') : copyOf ? t('複製排程') : t('新增排程')} onClose={onClose}>
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

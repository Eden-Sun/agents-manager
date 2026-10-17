import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { Modal } from '../components/Modal'
import { NewBotForm } from '../components/Sidebar'
import {
  createSchedule,
  deleteSchedule,
  getSchedule,
  patchSchedule,
  runScheduleNow,
  type Schedule,
  type ScheduleDetail,
  type ScheduleMode,
  type ScheduleNotify,
  type ScheduleSpecIn,
} from '../api/schedules'
import { useStore } from '../store/store'
import { createRequestId, settleCreateRequest } from '../lib/createRequestId'
import { newClientRequestId } from '../api'
import { ApiError } from '../api/types'
import { isImeEnter } from '../lib/ime'
import { defaultTimeZone, formatDue, formatOccurrenceTime, OCCURRENCE_LABEL, occurrenceLine, timeZoneOptions } from './format'
import { useLiveTurnStatus, useOnTurnChange } from './liveTurn'
import './schedules.css'
import './scheduleEditor.css'

const WEEKDAYS = ['週一', '週二', '週三', '週四', '週五', '週六', '週日']

interface FormState {
  name: string
  prompt: string
  botId: string
  tz: string
  notify: ScheduleNotify
  mode: ScheduleMode
  at: string
  /** 間隔的秒數：保留 API 收的精度（例如 61 秒）；分鐘欄只是顯示與改分鐘用，沒動就沿用原值。 */
  everySecs: number
  time: string
  weekdays: number[]
  clearAfter: boolean
}

function emptyForm(botId: string): FormState {
  return { name: '', prompt: '', botId, tz: defaultTimeZone(), notify: 'failure', mode: 'daily', at: '', everySecs: 3600, time: '09:00', weekdays: [0], clearAfter: false }
}

function fromDetail(d: ScheduleDetail): FormState {
  const spec = (d.spec ?? {}) as { at?: string; every_secs?: number; time?: string; weekdays?: number[] }
  return {
    name: d.name,
    prompt: d.prompt,
    botId: d.bot_id,
    tz: d.tz,
    notify: d.notify,
    mode: d.mode,
    at: spec.at ?? '',
    everySecs: spec.every_secs || 3600,
    time: spec.time ?? '09:00',
    weekdays: spec.weekdays ?? [0],
    clearAfter: d.clear_after === true,
  }
}

function specOf(f: FormState): ScheduleSpecIn {
  switch (f.mode) {
    case 'once':
      return { mode: 'once', at: f.at }
    case 'interval':
      return { mode: 'interval', every_secs: f.everySecs }
    case 'daily':
      return { mode: 'daily', time: f.time }
    case 'weekly':
      return { mode: 'weekly', time: f.time, weekdays: [...f.weekdays].sort((a, b) => a - b) }
  }
}

function errText(e: unknown): string {
  return e instanceof Error ? e.message : '操作失敗'
}

/** 新增或編輯排程。建立後只存排程，不立即執行；要執行請按「立即執行」。 */
export function ScheduleEditor({
  projectId,
  scheduleId,
  onClose,
  onSelect,
}: {
  projectId: string
  scheduleId: string | null
  onClose: () => void
  onSelect: (id: string) => void
}) {
  const bots = useStore((s) => s.bots)
  const selectBot = useStore((s) => s.selectBot)
  const [detail, setDetail] = useState<ScheduleDetail | null>(null)
  // 綁定 turn 的即時狀態（queued→in_flight→終態）：變了就重抓詳情；排程自己的事件也重抓。
  const liveStatus = useLiveTurnStatus(detail?.open?.bot_id, detail?.open?.turn_id)
  const reload = useCallback(() => {
    if (!scheduleId) return
    getSchedule(scheduleId)
      .then((d) => setDetail(d))
      .catch(() => undefined)
  }, [scheduleId])
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
  const [form, setForm] = useState<FormState>(() => emptyForm(''))
  // 載入當下的排程（id）、時區與時間（`specOf`）：存檔時跟表單比，沒變就不送 spec（#1807）。
  // 帶 id：換了排程、還沒載入完成時對不上，保守送 spec（#1812）。
  const [baseline, setBaseline] = useState<{ id: string; tz: string; spec: ScheduleSpecIn } | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [botSheet, setBotSheet] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [loadError, setLoadError] = useState<string | null>(null)
  // 建立的冪等 key：重送同一張表單用同一個；後端說 key 已用在別的內容上才換新的。
  const [createKey, setCreateKey] = useState(() => newClientRequestId())

  // 只列能接收排程的 Bot：同專案、一般 Bot、非 child／分享／受限（daemon 也會再驗一次）。
  const eligible = useMemo(
    () => bots.filter((b) => b.project_id === projectId && b.managed_by === 'user' && !b.parent_bot_id && !b.share_profile),
    [bots, projectId],
  )

  useEffect(() => {
    if (!scheduleId) return
    const id = scheduleId
    let alive = true
    getSchedule(id)
      .then((d) => {
        if (!alive) return
        setDetail(d)
        const loaded = fromDetail(d)
        setForm(loaded)
        setBaseline({ id, tz: loaded.tz, spec: specOf(loaded) })
        setLoadError(null)
      })
      .catch((e: unknown) => alive && setLoadError(errText(e)))
    return () => {
      alive = false
    }
  }, [scheduleId])

  // 建立時預設第一顆可用 Bot（之後使用者可改）：推導出來，不用 effect 寫回表單。
  const effectiveBotId = form.botId || (scheduleId ? '' : (eligible[0]?.id ?? ''))

  const set = <K extends keyof FormState>(k: K, v: FormState[K]) => setForm((f) => ({ ...f, [k]: v }))

  // 任務內容框只在內容超出目前高度時長高（最少 8 行由 rows 決定，最多 60vh 由 CSS 限制）；使用者手動拉高的不會被縮回去。
  const promptRef = useRef<HTMLTextAreaElement>(null)
  useLayoutEffect(() => {
    const el = promptRef.current
    if (!el || el.scrollHeight <= el.clientHeight) return
    el.style.height = `${el.scrollHeight + (el.offsetHeight - el.clientHeight)}px`
  }, [form.prompt])

  const botLabel = (id: string) => {
    const b = bots.find((x) => x.id === id)
    if (!b) return '（找不到這顆 Bot）'
    return `${b.name} · ${b.kind}${b.model ? ` · ${b.model}` : ''}${b.effort ? ` · ${b.effort}` : ''}`
  }

  const save = async () => {
    setError(null)
    setNotice(null)
    if (!form.name.trim()) return setError('請填排程名稱')
    if (!form.prompt.trim()) return setError('請填任務內容')
    if (!effectiveBotId) return setError('請選執行的 Bot（或建立專用 Bot）')
    if (form.mode === 'once' && !form.at) return setError('單次需要日期與時間')
    if (form.mode === 'weekly' && form.weekdays.length === 0) return setError('每週至少選一天')
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
        })
        setNotice('已建立。排程不會立即執行，到時間才會排進 Bot。')
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
        })
        if (timingChanged) setBaseline({ id: scheduleId, ...timing })
        // 儲存回來的是 Schedule（不帶 recent）：合進既有詳情，不取代成缺欄位的物件。
        setDetail((prev) => (prev ? { ...prev, ...d, recent: prev.recent } : prev))
        setNotice('已儲存。已排的未送出執行會被撤回，已送出的照跑。')
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
  const runNow = async () => {
    if (!scheduleId || !runKey) return
    setBusy(true)
    setError(null)
    // 伺服器收下（POST 成功）之後，key 仍保留到詳情也重抓成功才 settle：詳情 503 時再按，送的還是同一個 key，
    // 伺服器回同一筆 occurrence。只有 POST 本身的 4xx 拒絕才 settle（那次根本沒被收下）。
    let accepted = false
    try {
      const r = await runScheduleNow(scheduleId, createRequestId(runKey))
      accepted = true
      setNotice(r.occurrence ? `已排進 Bot：${occurrenceLine(r.occurrence)}` : '已有未結束的執行，沿用它')
      setDetail(await getSchedule(scheduleId))
      settleCreateRequest(runKey)
    } catch (e) {
      if (!accepted && e instanceof ApiError && e.status >= 400 && e.status < 500) settleCreateRequest(runKey)
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
      setDetail(await getSchedule(scheduleId))
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

  const body = (
    <div className="form schedule-editor">
      {loadError ? <p className="hint schedule-error">讀取失敗：{loadError}</p> : null}
      {detail ? (
        <section className="schedule-status" aria-label="排程狀態">
          <p>下次：{formatDue(detail.next_due, detail.tz)}</p>
          <p>狀態：{detail.enabled ? (detail.open ? occurrenceLine(detail.open, liveStatus) : '啟用中') : '已停用'}</p>
          {detail.open?.waiting_reason ? <p className="hint">{occurrenceLine(detail.open, liveStatus)}</p> : null}
        </section>
      ) : null}

      <label className="field">
        <span>名稱</span>
        <input value={form.name} maxLength={80} onChange={(e) => set('name', e.target.value)} />
      </label>

      <label className="field">
        <span>
          任務內容<small className="hint schedule-prompt-hint">⌘/Ctrl+Enter 儲存</small>
        </span>
        <textarea
          ref={promptRef}
          className="schedule-prompt"
          rows={8}
          value={form.prompt}
          onChange={(e) => set('prompt', e.target.value)}
          onKeyDown={(e) => {
            // 跟「儲存」按鈕同一支函式；輸入法選字中的 Enter 不算。
            if (busy || e.key !== 'Enter' || !(e.ctrlKey || e.metaKey) || isImeEnter(e.nativeEvent)) return
            e.preventDefault()
            void save()
          }}
        />
      </label>

      <label className="field">
        <span>執行的 Bot</span>
        <select value={effectiveBotId} onChange={(e) => set('botId', e.target.value)}>
          {effectiveBotId && !eligible.some((b) => b.id === effectiveBotId) ? <option value={effectiveBotId}>{botLabel(effectiveBotId)}</option> : null}
          {eligible.length === 0 ? <option value="">（這個專案沒有可用的 Bot）</option> : null}
          {eligible.map((b) => (
            <option key={b.id} value={b.id}>
              {botLabel(b.id)}
            </option>
          ))}
        </select>
      </label>
      <p className="hint">
        排程會排進這顆 Bot 的對話與佇列，跟你手動送的訊息共用。沿用這顆 Bot 的模型；需要指定模型可建立專用 Bot。
      </p>
      <button type="button" className="btn" onClick={() => setBotSheet(true)}>
        建立專用 Bot…
      </button>
      {effectiveBotId ? (
        <button
          type="button"
          className="btn"
          onClick={() => {
            // 到 Bot 對話看執行與額度、離線原因；不另建 API。
            selectBot(effectiveBotId)
            onClose()
          }}
        >
          開啟 Bot 對話
        </button>
      ) : null}

      <label className="field">
        <span>模式</span>
        <select value={form.mode} onChange={(e) => set('mode', e.target.value as ScheduleMode)}>
          <option value="once">單次</option>
          <option value="interval">固定間隔</option>
          <option value="daily">每天</option>
          <option value="weekly">每週</option>
        </select>
      </label>
      {form.mode === 'once' ? (
        <label className="field">
          <span>時間（本地）</span>
          <input type="datetime-local" value={form.at} onChange={(e) => set('at', e.target.value)} />
        </label>
      ) : null}
      {form.mode === 'interval' ? (
        <label className="field">
          <span>每幾分鐘（以建立時為起點，不隨任務耗時漂移）</span>
          <input type="number" min={1} max={525600} value={Math.round(form.everySecs / 60)} onChange={(e) => set('everySecs', Number(e.target.value) * 60)} />
        </label>
      ) : null}
      {form.mode === 'daily' || form.mode === 'weekly' ? (
        <label className="field">
          <span>時間（本地）</span>
          <input type="time" value={form.time} onChange={(e) => set('time', e.target.value)} />
        </label>
      ) : null}
      {form.mode === 'weekly' ? (
        <fieldset className="field">
          <legend>星期</legend>
          {WEEKDAYS.map((label, i) => (
            <label key={label} className="inline-check">
              <input
                type="checkbox"
                checked={form.weekdays.includes(i)}
                onChange={(e) => set('weekdays', e.target.checked ? [...form.weekdays, i] : form.weekdays.filter((d) => d !== i))}
              />
              {label}
            </label>
          ))}
        </fieldset>
      ) : null}

      <label className="field">
        <span>時區</span>
        <select value={form.tz} onChange={(e) => set('tz', e.target.value)}>
          {timeZoneOptions(form.tz).map((z) => (
            <option key={z} value={z}>
              {z}
            </option>
          ))}
        </select>
      </label>

      <label className="field">
        <span>通知</span>
        <select value={form.notify} onChange={(e) => set('notify', e.target.value as ScheduleNotify)}>
          <option value="failure">只有失敗時通知</option>
          <option value="all">每次執行都通知</option>
          <option value="none">不通知</option>
        </select>
      </label>

      <label className="inline-check schedule-clear-after">
        <input type="checkbox" checked={form.clearAfter} onChange={(e) => set('clearAfter', e.target.checked)} />
        跑完就清空對話（不留記憶、不佔 context）
      </label>
      <p className="hint">每次跑完會讓這顆 Bot 開新對話；它跟你手動聊的內容也會一起清掉，建議搭配專用 Bot。</p>

      {error ? <p className="hint schedule-error" role="alert">{error}</p> : null}
      {notice ? <p className="hint" role="status">{notice}</p> : null}

      <div className="form-actions">
        <button type="button" className="btn primary" disabled={busy} onClick={() => void save()}>
          {scheduleId ? '儲存' : '建立排程'}
        </button>
        {detail ? (
          <>
            <button type="button" className="btn" disabled={busy || !detail.enabled} onClick={() => void runNow()}>
              立即執行
            </button>
            <button type="button" className="btn" disabled={busy} onClick={() => void toggle()}>
              {detail.enabled ? '停用' : '啟用'}
            </button>
            <button type="button" className="btn danger" disabled={busy} onClick={() => setConfirmDelete(true)}>
              刪除
            </button>
          </>
        ) : null}
      </div>

      {detail ? (
        <details className="schedule-recent">
          <summary>最近執行（{(detail.recent ?? []).length}）</summary>
          {(detail.recent ?? []).length > 0 ? (
            <ul className="schedule-history" aria-label="最近執行">
              {(detail.recent ?? []).map((o) => (
                <li key={o.id}>
                  {o.source === 'manual' ? '手動' : '排程'} · {formatOccurrenceTime(o.scheduled_for, detail.tz)} · {OCCURRENCE_LABEL[o.status]}
                  {o.error ? `（${o.error}）` : ''}
                  {o.merged_count > 0 ? ` · 合併 ${o.merged_count} 次` : ''}
                </li>
              ))}
            </ul>
          ) : (
            <p className="hint">還沒有執行紀錄。</p>
          )}
        </details>
      ) : null}
    </div>
  )

  // 掛到 body：側欄是 transform 容器，裡面的 fixed 遮罩會被它限在側欄寬度內（手機上彈窗被裁）。只這裡處理，不動全站 Modal。
  return createPortal(
    <>
      <Modal open width={760} title={scheduleId ? '排程' : '新增排程'} onClose={onClose}>
        {body}
      </Modal>
      <Modal open={botSheet} title="新增專用 Bot" onClose={() => setBotSheet(false)}>
        <NewBotForm
          key={projectId}
          initialProjectId={projectId}
          onDone={() => setBotSheet(false)}
          onCreated={(id) => setForm((f) => ({ ...f, botId: id }))}
        />
      </Modal>
      <ConfirmDialog
        open={confirmDelete}
        title="刪除排程"
        body={<>刪除後不會再排新的執行。已經排進 Bot 還沒送出的會被撤回；已經送出的回合照跑。</>}
        confirmLabel="刪除"
        danger
        onConfirm={() => void remove()}
        onCancel={() => setConfirmDelete(false)}
      />
    </>,
    document.body,
  )
}

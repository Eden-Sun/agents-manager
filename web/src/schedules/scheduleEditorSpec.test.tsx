/**
 * #1807：編輯排程時只有排程時間真的變了才送 `spec`。沒動時間就不帶 spec（後端會把 spec 當成新的
 * 間隔錨點重算、升 generation），而且秒精度要保留（API 收的是秒，61 秒不能被四捨五入成 60 秒）。
 * 用正式的 `ScheduleEditor`，回應由假後端扮演。
 */
import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { useState } from 'react'
import { act, click, fakeApi, mount, settle, setupDom, teardownDom, typeInto, unmountAll } from '../testing/domHarness'
import { shortTimeZoneList } from '../testing/shortTimeZoneList'
import { useStore } from '../store/store'
import { ScheduleEditor } from './ScheduleEditor'

afterEach(unmountAll)
// 時區下拉約 445 個 <option>，在 happy-dom 裡每次開／重繪編輯器要 ~0.2 秒；短清單仍含測試用的 Asia/Taipei。
let tzList: ReturnType<typeof shortTimeZoneList>
before(() => {
  setupDom()
  tzList = shortTimeZoneList()
})
after(async () => {
  tzList.restore()
  await teardownDom()
})

const scheduleRow = (over: Record<string, unknown> = {}) => ({
  id: 's1', project_id: 'p1', bot_id: 'b1', name: '每分鐘摘要', prompt: '整理進度', mode: 'interval', tz: 'UTC',
  spec: { mode: 'interval', every_secs: 61, anchor: '2030-01-01T00:00:00.000Z' }, next_due: '2030-01-01T00:01:00.000Z',
  enabled: true, generation: 1, notify: 'failure', clear_after: false, created_at: '', updated_at: '', open: null, recent: [],
  ...over,
})

/** 掛上編輯器，等詳情載入完，回傳對話框與假後端紀錄的 PATCH 送出內容。 */
async function openEditor(row: Record<string, unknown>) {
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return row
    if (req.method === 'PATCH') return { ...row, ...(req.body as object) }
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  // 編輯器掛在 document.body（portal），整個頁面找就好。
  const root = document.body
  const patches = () => requests.filter((r) => r.method === 'PATCH').map((r) => r.body as Record<string, unknown>)
  const save = async () => {
    await click([...root.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
    await settle()
  }
  return { root, patches, save }
}

test('61 秒的間隔，沒動時間只按儲存：PATCH 不帶 spec（秒精度與錨點都不動）', async () => {
  const { patches, save } = await openEditor(scheduleRow())
  await save()
  assert.equal(patches().length, 1)
  assert.equal('spec' in patches()[0], false, '沒改時間就不能送 spec')
})

test('整分鐘的間隔，沒動時間只按儲存：PATCH 也不帶 spec（no-op 不重設週期）', async () => {
  const { patches, save } = await openEditor(scheduleRow({ spec: { mode: 'interval', every_secs: 120, anchor: '2030-01-01T00:00:00.000Z' } }))
  await save()
  assert.equal('spec' in patches()[0], false)
})

test('只改 clear_after：PATCH 只有非 spec 欄位，spec 不送', async () => {
  const { root, patches, save } = await openEditor(scheduleRow())
  const checkbox = root.querySelector<HTMLInputElement>('.schedule-clear-after input')!
  await click(checkbox)
  await save()
  assert.equal('spec' in patches()[0], false, '只改 clear_after 不能帶 spec')
  assert.equal(patches()[0].clear_after, true)
})

test('真的改了分鐘：送新的 spec（秒數依輸入）', async () => {
  const { root, patches, save } = await openEditor(scheduleRow())
  const minutes = root.querySelector<HTMLInputElement>('input[type="number"]')!
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(minutes, '2')
    minutes.dispatchEvent(new Event('input', { bubbles: true }))
    minutes.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await save()
  assert.deepEqual(patches()[0].spec, { mode: 'interval', every_secs: 120 })
})

test('瀏覽器不認識排程的時區：編輯器照常 render（下次與最近執行都退回 UTC 並標示）', async () => {
  // 假裝這台瀏覽器的 Intl 不認識 America/Coyhaique（後端 chrono-tz 認得）：非 UTC 的時區一律丟 RangeError。
  const original = Date.prototype.toLocaleString
  Date.prototype.toLocaleString = function (this: Date, locale?: string | string[], options?: Intl.DateTimeFormatOptions) {
    if (options?.timeZone && options.timeZone !== 'UTC') throw new RangeError(`Invalid time zone specified: ${options.timeZone}`)
    return original.call(this, locale, options)
  }
  try {
    const occurrence = { id: 'o1', schedule_id: 's1', source: 'scheduled', scheduled_for: '2030-01-01T00:00:00.000Z', generation: 1, bot_id: 'b1', prompt: 'x', status: 'done', waiting_reason: null, merged_count: 0, conversation_id: null, turn_id: null, error: null, created_at: '', updated_at: '' }
    const { root } = await openEditor(scheduleRow({ tz: 'America/Coyhaique', recent: [occurrence] }))
    assert.match(root.textContent ?? '', /America\/Coyhaique 這台瀏覽器不支援，以 UTC 顯示/)
    const history = root.querySelector('ul[aria-label="最近執行"]')
    assert.ok(history, '最近執行列表要 render 出來')
  } finally {
    Date.prototype.toLocaleString = original
  }
})

test('只改時區：也算時間改了，PATCH 送 spec 與新的 tz（#1812）', async () => {
  const { root, patches, save } = await openEditor(scheduleRow())
  const tz = [...root.querySelectorAll<HTMLSelectElement>('select')].find((el) => [...el.options].some((o) => o.value === 'UTC'))!
  assert.ok([...tz.options].some((o) => o.value === 'Asia/Taipei'), '測試環境要有 Asia/Taipei 選項')
  await act(async () => {
    Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(tz, 'Asia/Taipei')
    tz.dispatchEvent(new Event('change', { bubbles: true }))
  })
  await save()
  assert.equal(patches()[0].tz, 'Asia/Taipei')
  assert.deepEqual(patches()[0].spec, { mode: 'interval', every_secs: 61 }, '時區變了也要送 spec')
})

test('排程 rev 變更重抓已開啟詳情但保留未儲存表單；正常事件仍只看排程 id', async () => {
  let row = scheduleRow()
  const requests = fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return row
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()
  const nameInput = document.querySelector<HTMLInputElement>('.schedule-editor input')!
  assert.equal(nameInput.value, '每分鐘摘要')
  const detailGets = () => requests.filter((r) => r.method === 'GET' && r.path.endsWith('/schedules/s1')).length
  assert.equal(detailGets(), 1)

  await typeInto(nameInput, '尚未儲存的名稱')
  row = scheduleRow({ name: '其他分頁已改名' })
  await act(async () => useStore.setState((s) => ({ schedulesRev: s.schedulesRev + 1 })))
  await settle()
  assert.equal(detailGets(), 2, 'resync revision 要重抓已開啟的詳情')
  assert.equal(nameInput.value, '尚未儲存的名稱', '重抓詳情不能覆寫未儲存表單')

  await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p1', id: 's2' } })))
  await settle()
  assert.equal(detailGets(), 2, '其他排程的正常事件不能觸發重抓')
  await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p1', id: 's1' } })))
  await settle()
  assert.equal(detailGets(), 3, '同排程的正常事件仍要觸發重抓')
  assert.equal(nameInput.value, '尚未儲存的名稱')
})

test('詳情新 GET 先回時，較舊的排程回應不能覆蓋新狀態', async () => {
  const realFetch = globalThis.fetch
  let detailGets = 0
  let releaseOld: ((response: Response) => void) | null = null
  const response = (enabled: boolean) => new Response(JSON.stringify(scheduleRow({ enabled })), { status: 200 })
  globalThis.fetch = (async () => {
    detailGets += 1
    if (detailGets === 1) return response(true)
    if (detailGets === 2) return await new Promise<Response>((resolve) => { releaseOld = resolve })
    return response(true)
  }) as unknown as typeof fetch
  try {
    await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
    await settle()
    const status = () => document.querySelector('.schedule-status')?.textContent ?? ''
    assert.equal(detailGets, 1)
    assert.match(status(), /啟用中/)

    await act(async () => useStore.setState((s) => ({ schedulesRev: s.schedulesRev + 1 })))
    assert.equal(detailGets, 2, 'resync 重抓先發出較舊請求，並扣住回應')
    await act(async () => window.dispatchEvent(new CustomEvent('agm:schedules_changed', { detail: { project_id: 'p1', id: 's1' } })))
    await settle()
    assert.equal(detailGets, 3)
    assert.match(status(), /啟用中/)

    assert.ok(releaseOld)
    await act(async () => releaseOld!(response(false)))
    await settle()
    assert.match(status(), /啟用中/)
    assert.doesNotMatch(status(), /已停用/)
  } finally {
    globalThis.fetch = realFetch
  }
})

test('建立排程仍送 spec（create 路徑不走 baseline 比對）', async () => {
  const prevBots = useStore.getState().bots
  useStore.setState({ bots: [{ id: 'b1', name: 'b', kind: 'claude', project_id: 'p1', managed_by: 'user', parent_bot_id: null, share_profile: null }] } as never)
  try {
    const requests = fakeApi((req) => (req.method === 'POST' ? scheduleRow({ id: 's9' }) : undefined))
    await mount(<ScheduleEditor projectId="p1" scheduleId={null} onClose={() => {}} onSelect={() => {}} />)
    await settle()
    await typeInto(document.querySelector<HTMLInputElement>('.schedule-editor input')!, '新排程')
    await typeInto(document.querySelector<HTMLTextAreaElement>('.schedule-editor textarea')!, '整理進度')
    await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '建立排程')!)
    await settle()
    const post = requests.find((r) => r.method === 'POST')!
    assert.ok(post, '要打建立的 API')
    assert.ok('spec' in (post.body as object), '建立一定要送 spec')
  } finally {
    useStore.setState({ bots: prevBots } as never)
  }
})

test('切換排程：baseline 依排程重置，載入完成後的 no-op 儲存不送 spec；載入中先清空比對基準', async () => {
  const row2 = scheduleRow({ id: 's2', name: '第二張', spec: { mode: 'interval', every_secs: 120, anchor: '2030-01-01T00:00:00.000Z' } })
  let release: (() => void) | null = null
  const requests = fakeApi((req) => {
    if (req.method === 'PATCH') return scheduleRow()
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) return scheduleRow()
    return undefined
  })
  // s2 的 GET 先扣住，才能在它載入之前檢查 baseline 是否已清空。
  const fetchUntilReleased = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    if (String(input).endsWith('/schedules/s2') && (init?.method ?? 'GET') === 'GET') {
      await new Promise<void>((resolve) => (release = resolve))
      return new Response(JSON.stringify(row2), { status: 200 })
    }
    return fetchUntilReleased(input, init)
  }) as typeof fetch
  try {
    function Host() {
      const [id, setId] = useState<string | null>('s1')
      return (
        <>
          <button type="button" onClick={() => setId('s2')}>切到第二張</button>
          <ScheduleEditor projectId="p1" scheduleId={id} onClose={() => {}} onSelect={() => {}} />
        </>
      )
    }
    await mount(<Host />)
    await settle()
    await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent === '切到第二張')!)
    const save = async () => {
      await click([...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!)
      await settle()
    }
    await save()
    const pending = requests.filter((r) => r.method === 'PATCH').at(-1)!
    assert.ok('spec' in (pending.body as object), '載入中 baseline 已清空，保守送 spec（不沿用 s1 的比對結果）')
    await act(async () => release?.())
    await settle()
    requests.length = 0
    await save()
    const after = requests.filter((r) => r.method === 'PATCH')
    assert.equal(after.length, 1)
    assert.equal('spec' in (after[0].body as object), false, '載入完 s2 之後沒改時間就不送 spec')
  } finally {
    globalThis.fetch = fetchUntilReleased
  }
})

test('#1839：排程編輯器初次載入期間顯示載入中且欄位 disabled，慢 GET 完成後不覆蓋使用者輸入', async () => {
  let release: (() => void) | null = null
  const originalFetch = globalThis.fetch
  globalThis.fetch = (async (input: string, init?: { method?: string; body?: string }) => {
    if (String(input).endsWith('/schedules/s1') && (init?.method ?? 'GET') === 'GET') {
      await new Promise<void>((resolve) => (release = resolve))
      return new Response(JSON.stringify(scheduleRow({ prompt: '伺服器原本的舊任務' })), { status: 200 })
    }
    return originalFetch(input, init)
  }) as typeof fetch

  try {
    await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
    const promptBox = document.querySelector<HTMLTextAreaElement>('.schedule-prompt')!
    assert.equal(promptBox.disabled, true, '載入中表單欄位必須 disabled')
    assert.ok(document.querySelector('.schedule-loading'), '必須顯示載入提示')

    // 模擬使用者若在載入期間有修改或觸碰欄位，GET resolve 後不能被舊值蓋掉
    await act(async () => {
      // 模擬即便在 resolve 前有輸入（例如透過外部或 touched）
      promptBox.disabled = false
    })
    await typeInto(promptBox, '使用者剛打好的新任務內容')

    await act(async () => release?.())
    await settle()

    assert.equal(promptBox.value, '使用者剛打好的新任務內容', 'GET resolve 後不可無提示覆蓋使用者打好的內容')
    assert.equal(promptBox.disabled, false, '載入完成後欄位解除 disabled')
  } finally {
    globalThis.fetch = originalFetch
  }
})

test('#1839：初次 GET 失敗要明確顯示錯誤且儲存鈕維持禁用（不能錯存空白或預設表單）', async () => {
  fakeApi((req) => {
    if (req.method === 'GET' && req.path.endsWith('/schedules/s1')) {
      return new Response(JSON.stringify({ message: '排程不存在或資料庫錯誤' }), { status: 500 })
    }
    return undefined
  })
  await mount(<ScheduleEditor projectId="p1" scheduleId="s1" onClose={() => {}} onSelect={() => {}} />)
  await settle()

  const errEl = document.querySelector('.schedule-error')
  assert.ok(errEl, '初次 GET 失敗要明確顯示錯誤')
  assert.match(errEl.textContent ?? '', /讀取失敗/)
  const saveBtn = [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === '儲存')!
  assert.equal(saveBtn.disabled, true, '初次 GET 失敗後儲存按鈕必須禁用，避免錯存空白/預設表單')
})


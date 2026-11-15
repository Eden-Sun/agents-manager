import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, act, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ApiTurnTimeline } from './ApiTurnTimeline'
import type { ApiTurnTimelineData } from '../api/apiBotUsage'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const mockTimelineData: ApiTurnTimelineData = {
  bot_id: 'bot-123',
  turn_id: 'turn-abc',
  status: 'completed',
  halt_reason: 'end_turn',
  total_cost_usd: 0.0585,
  duration_ms: 2450,
  created_at: '2026-11-15T02:00:00.000Z',
  completed_at: '2026-11-15T02:00:02.450Z',
  summary: {
    total_cost_usd: 0.0585,
    breakdown: {
      input: { tokens: 50, cost_usd: 0.0002, percentage: 0.3 },
      cache_read: { tokens: 20000, cost_usd: 0.0040, percentage: 6.8 },
      cache_write: { tokens: 10000, cost_usd: 0.0500, percentage: 85.5 },
      output: { tokens: 210, cost_usd: 0.0042, percentage: 7.2 },
      compaction: { tokens: 0, cost_usd: 0.0, percentage: 0.0 },
      server_tool: { tokens: 0, cost_usd: 0.0, percentage: 0.0 },
    },
    largest_item: {
      category: 'cache_write',
      label: '快取寫入',
      cost_usd: 0.0500,
      percentage: 85.5,
    },
    suggestions: [
      '快取冷掉重寫花費 $0.050；若在快取保留期（5 分鐘）內送出，快取讀取僅需 $0.002，可省約 $0.048（省 96%）。',
    ],
  },
  timeline: [
    {
      type: 'request',
      ordinal: 0,
      timestamp: '2026-11-15T02:00:00.000Z',
      iteration_type: 'message',
      input_tokens: 50,
      cache_read_tokens: 20000,
      cache_write_tokens: 10000,
      output_tokens: 120,
      cost_usd: 0.0560,
      stop_reason: 'tool_use',
      duration_ms: 1800,
      speed: 'standard',
    },
    {
      type: 'tool',
      name: 'read_file',
      duration_ms: 45,
      is_error: false,
    },
    {
      type: 'request',
      ordinal: 1,
      timestamp: '2026-11-15T02:00:01.900Z',
      iteration_type: 'message',
      input_tokens: 0,
      cache_read_tokens: 30120,
      cache_write_tokens: 0,
      output_tokens: 90,
      cost_usd: 0.0025,
      stop_reason: 'end_turn',
      duration_ms: 500,
      speed: 'standard',
    },
    {
      type: 'turn_end',
      timestamp: '2026-11-15T02:00:02.450Z',
      status: 'completed',
      halt_reason: 'end_turn',
    },
  ],
}

test('ApiTurnTimeline 渲染「這回合為什麼花了 $X」標題、一句話結論與堆疊長條', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const text = root.textContent ?? ''

  assert.match(text, /這回合為什麼花了 \$0\.0585/)
  assert.match(text, /2\.45s/)
  // 一句話結論與最大項占比
  assert.match(text, /這回合 \$0\.0585，85\.5% 是快取寫入/)
  assert.match(text, /最大項：快取寫入（佔 85\.5% · \$0\.0500）/)
  assert.ok(root.querySelector('.api-cost-bar'), '有堆疊條')
})

test('ApiTurnTimeline 渲染對齊的費用拆分表格（等寬右對齊，0 元淡列在後）', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const table = root.querySelector('.api-breakdown-table')
  assert.ok(table, '必須有拆分表格')

  // 表頭欄位
  const ths = [...table.querySelectorAll('th')].map((th) => th.textContent?.trim())
  assert.deepEqual(ths, ['類別', '用量', '金額', '占比'])

  // 6 大類別皆存在
  const text = table.textContent ?? ''
  assert.match(text, /快取寫入/)
  assert.match(text, /一般輸入/)
  assert.match(text, /輸出 \(含思考\)/)
  assert.match(text, /快取讀取/)
  assert.match(text, /對話壓縮/)
  assert.match(text, /伺服器工具/)

  // 排序檢查：有花費的列在前，0 元的排在最後且帶 .is-zero
  const rows = [...table.querySelectorAll('tbody tr')]
  assert.equal(rows.length, 6)
  // 第一列應為金額最大的「快取寫入」
  assert.match(rows[0].textContent ?? '', /快取寫入/)
  // 最後兩列應為 0 元的「對話壓縮」與「伺服器工具」
  assert.ok(rows[4].classList.contains('is-zero'), '第 5 列為 0 元淡列')
  assert.ok(rows[5].classList.contains('is-zero'), '第 6 列為 0 元淡列')

  // 金額與占比為右對齊等寬樣式
  assert.ok(rows[0].querySelector('.td-cost')?.classList.contains('text-right'))
  assert.ok(rows[0].querySelector('.td-pct')?.classList.contains('text-right'))
})

test('ApiTurnTimeline 可省建議放在結論旁，沒有建議時不顯示框', async () => {
  // 1. 有建議時：渲染建議區塊
  const rootWith = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const tipsWith = rootWith.querySelector('.api-conclusion-suggestions')
  assert.ok(tipsWith, '有建議時必須顯示建議框')
  assert.match(tipsWith.textContent ?? '', /快取冷掉重寫花費 \$0\.050/)

  await unmountAll()

  // 2. 沒有建議時：不顯示框
  const dataWithoutSuggestions: ApiTurnTimelineData = {
    ...mockTimelineData,
    summary: {
      ...mockTimelineData.summary,
      suggestions: [],
    },
  }
  const rootWithout = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={dataWithoutSuggestions} />)
  const tipsWithout = rootWithout.querySelector('.api-conclusion-suggestions')
  assert.equal(tipsWithout, null, '沒有建議時不顯示框')
})

test('ApiTurnTimeline 時間軸可折疊收合，每步時間與花費對齊', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)

  // 預設展開
  assert.ok(root.querySelector('.timeline-list'), '預設時間軸展開')
  const reqMetrics = root.querySelector('.timeline-request .step-metrics')
  assert.ok(reqMetrics, '時間軸請求列有對齊的指標欄')
  assert.match(reqMetrics.textContent ?? '', /1\.80s/)
  assert.match(reqMetrics.textContent ?? '', /\$0\.0560/)

  // 點擊折疊按鈕收合
  const toggleBtn = root.querySelector('.timeline-toggle-btn') as HTMLButtonElement | null
  assert.ok(toggleBtn, '有折疊按鈕')
  await act(() => {
    toggleBtn.click()
  })
  assert.equal(root.querySelector('.timeline-list'), null, '點擊後收合時間軸')

  // 再次點擊展開
  await act(() => {
    toggleBtn.click()
  })
  assert.ok(root.querySelector('.timeline-list'), '再次點擊展開時間軸')
})

test('ApiTurnTimeline 點擊關閉按鈕觸發 onClose 回呼', async () => {
  let closed = false
  const handleClose = () => {
    closed = true
  }
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} onClose={handleClose} />)

  const closeBtn = root.querySelector('.api-timeline-close-btn') as HTMLButtonElement | null
  assert.ok(closeBtn, '必須有關閉按鈕')
  await act(() => {
    closeBtn.click()
  })
  assert.equal(closed, true)
})

test('ApiTurnTimeline 正文標題與類別不用 opacity 淡化，確保高對比度可讀性', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)

  const title = root.querySelector('.api-timeline-title') as HTMLElement | null
  const conclusion = root.querySelector('.api-conclusion-text') as HTMLElement | null
  const heading = root.querySelector('.timeline-heading') as HTMLElement | null
  const labels = root.querySelectorAll('.card-label')
  const largest = root.querySelector('.api-largest-chip') as HTMLElement | null

  assert.ok(title, '有標題')
  assert.ok(conclusion, '有結論文字')
  assert.ok(heading, '有時間軸標題')
  assert.equal(labels.length, 6, '有 6 大類別名稱')
  assert.ok(largest, '有最大項晶片文字')

  // 正文元素不用 opacity 淡化
  const checkNoOpacity = (el: HTMLElement, name: string) => {
    const inlineOpacity = el.style.opacity
    assert.ok(!inlineOpacity || inlineOpacity === '1', `${name} 不可設定 opacity 淡化`)
    const computed = window.getComputedStyle(el)
    assert.ok(computed.opacity === '' || computed.opacity === '1', `${name} computed opacity 不可被淡化`)
  }

  checkNoOpacity(title, '標題')
  checkNoOpacity(conclusion, '結論文字')
  checkNoOpacity(heading, '時間軸標題')
  checkNoOpacity(largest, '最大項文字')
  for (const label of labels) {
    checkNoOpacity(label as HTMLElement, '類別名稱')
  }
})

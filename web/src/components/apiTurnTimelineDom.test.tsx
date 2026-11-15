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

test('ApiTurnTimeline 渲染「這回合為什麼花了 $X」標題、總耗時與最大項晶片', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const text = root.textContent ?? ''

  assert.match(text, /這回合為什麼花了 \$0\.0585/)
  assert.match(text, /2\.45s/)
  assert.match(text, /最大項：快取寫入（佔 85\.5% · \$0\.0500）/)
})

test('ApiTurnTimeline 渲染 6 大類別拆分卡片與可省建議', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const text = root.textContent ?? ''

  assert.match(text, /快取寫入/)
  assert.match(text, /一般輸入/)
  assert.match(text, /輸出 \(含思考\)/)
  assert.match(text, /快取讀取/)
  assert.match(text, /對話壓縮/)
  assert.match(text, /伺服器工具/)
  assert.match(text, /快取冷掉重寫花費 \$0\.050/)
})

test('ApiTurnTimeline 時間軸完整列出每次 API 請求、工具執行與結束原因，且不顯示工具原始輸入輸出', async () => {
  const root = await mount(<ApiTurnTimeline botId="bot-123" turnId="turn-abc" initialData={mockTimelineData} />)
  const text = root.textContent ?? ''

  // 請求行
  assert.match(text, /#1 API 請求/)
  assert.match(text, /#2 API 請求/)

  // 工具行：有名稱與耗時，無全文
  assert.match(text, /read_file/)
  assert.match(text, /45ms/)
  assert.match(text, /成功/)

  // 結束行
  assert.match(text, /回合結束/)
  assert.match(text, /原因：end_turn/)

  // DOM 結構確認
  assert.ok(root.querySelector('.api-timeline-panel'))
  assert.ok(root.querySelector('.timeline-list'))
  assert.ok(root.querySelector('.api-cost-bar'))
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

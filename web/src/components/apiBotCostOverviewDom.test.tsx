import test, { after, afterEach, before } from 'node:test'
import assert from 'node:assert/strict'
import { mount, setupDom, teardownDom, unmountAll } from '../testing/domHarness'
import { ApiBotCostOverview, type ApiBotCostOverviewData } from './ApiBotCostOverview'

before(setupDom)
afterEach(async () => {
  await unmountAll()
})
after(async () => {
  await teardownDom()
})

const mockOverviewData: ApiBotCostOverviewData = {
  bot_id: 'bot-claude-api',
  today_cost_usd: 1.25,
  total_cost_usd: 5.0104,
  conclusion: '總花費 $5.01，82% 是快取寫入',
  suggestions: [
    '快取冷掉重寫花費 $4.108；若在快取保留期（5 分鐘）內送出，快取讀取僅需 $0.016，可省約 $4.092（省 100%）。',
  ],
  categories: [
    {
      key: 'cache_write',
      label: '快取寫入',
      tokens: 821650,
      cost_usd: 4.1083,
      percentage: 82.0,
      colorVar: '#d97706',
    },
    {
      key: 'input',
      label: '一般輸入',
      tokens: 75000,
      cost_usd: 0.3000,
      percentage: 6.0,
      colorVar: '#2563eb',
    },
    {
      key: 'output',
      label: '輸出(含思考)',
      tokens: 25000,
      cost_usd: 0.5000,
      percentage: 10.0,
      colorVar: '#7c3aed',
    },
    {
      key: 'cache_read',
      label: '快取讀取',
      tokens: 510000,
      cost_usd: 0.1021,
      percentage: 2.0,
      colorVar: '#16a34a',
    },
    {
      key: 'compaction',
      label: '對話壓縮',
      tokens: 0,
      cost_usd: 0,
      percentage: 0,
      colorVar: '#64748b',
    },
    {
      key: 'server_tool',
      label: '伺服器工具',
      tokens: 0,
      cost_usd: 0,
      percentage: 0,
      colorVar: '#64748b',
    },
  ],
  top_turns: [
    {
      turn_id: 'turn-expensive-1',
      started_at: '2026-11-15T14:32:00Z',
      requests: 3,
      cost_usd: 2.5052,
      input_tokens: 400000,
      output_tokens: 15000,
    },
    {
      turn_id: 'turn-expensive-2',
      started_at: '2026-11-15T14:10:00Z',
      requests: 2,
      cost_usd: 1.2500,
      input_tokens: 200000,
      output_tokens: 8000,
    },
  ],
}

test('ApiBotCostOverview 渲染標題、今日花費、總計與關閉按鈕', async () => {
  let closed = false
  const root = await mount(
    <ApiBotCostOverview
      botId="bot-claude-api"
      initialData={mockOverviewData}
      onClose={() => {
        closed = true
      }}
    />,
  )

  const title = root.querySelector('.api-overview-title')
  assert.ok(title, '標題存在')
  assert.match(title.textContent ?? '', /Claude API 花費概覽/)

  const todayChip = root.querySelector('.api-overview-chip.today')
  assert.ok(todayChip, '今日花費 chip 存在')
  assert.match(todayChip.textContent ?? '', /\$1\.25/)

  const totalChip = root.querySelector('.api-overview-chip.total')
  assert.ok(totalChip, '總計 chip 存在')
  assert.match(totalChip.textContent ?? '', /\$5\.01/)

  const closeBtn = root.querySelector<HTMLButtonElement>('.api-overview-close')
  assert.ok(closeBtn, '關閉按鈕存在')
  closeBtn.click()
  assert.equal(closed, true, '點擊關閉按鈕觸發 onClose')
})

test('ApiBotCostOverview 渲染一句話結論與堆疊百分比長條', async () => {
  const root = await mount(
    <ApiBotCostOverview botId="bot-claude-api" initialData={mockOverviewData} />,
  )

  const conclusion = root.querySelector('.api-overview-conclusion')
  assert.ok(conclusion, '結論區塊存在')
  assert.match(conclusion.textContent ?? '', /總花費 \$5\.01，82% 是快取寫入/)

  const bar = root.querySelector('.api-overview-bar')
  assert.ok(bar, '堆疊長條存在')
  const segs = bar.querySelectorAll('.api-overview-bar-seg')
  assert.ok(segs.length >= 4, `應至少有 4 個大於 0.5% 的分類片段，實際 ${segs.length}`)

  const firstSeg = segs[0] as HTMLElement
  assert.match(firstSeg.style.width, /82%/)
})

test('ApiBotCostOverview 可省建議放在結論旁，沒有建議時不顯示框', async () => {
  // 1. 有建議時：顯示建議框且有內容
  const rootWith = await mount(
    <ApiBotCostOverview botId="bot-claude-api" initialData={mockOverviewData} />,
  )
  const suggWith = rootWith.querySelector('.api-overview-suggestion')
  assert.ok(suggWith, '有建議時應渲染建議框')
  assert.match(suggWith.textContent ?? '', /快取冷掉重寫花費 \$4\.108/)

  // 2. 沒有建議時：完全不顯示建議框（不留空框）
  const dataWithoutSuggestions: ApiBotCostOverviewData = {
    ...mockOverviewData,
    suggestions: [],
  }
  const rootWithout = await mount(
    <ApiBotCostOverview botId="bot-claude-api" initialData={dataWithoutSuggestions} />,
  )
  const suggWithout = rootWithout.querySelector('.api-overview-suggestion')
  assert.equal(suggWithout, null, '沒有建議時不應渲染建議框（避免留空框）')
})

test('ApiBotCostOverview 渲染 4 欄對齊表格（等寬右對齊，0 元淡列在後）', async () => {
  const root = await mount(
    <ApiBotCostOverview botId="bot-claude-api" initialData={mockOverviewData} />,
  )

  const ths = root.querySelectorAll('.api-overview-table th')
  assert.equal(ths.length, 4, '應有 4 欄表頭')
  assert.match(ths[0].textContent ?? '', /類別/)
  assert.match(ths[1].textContent ?? '', /用量/)
  assert.match(ths[2].textContent ?? '', /金額/)
  assert.match(ths[3].textContent ?? '', /占比/)

  // 檢查金額與占比在 th-num 且為右對齊 class
  assert.ok(ths[1].classList.contains('th-num'), '用量欄標頭為數值右對齊 class')
  assert.ok(ths[2].classList.contains('th-num'), '金額欄標頭為數值右對齊 class')
  assert.ok(ths[3].classList.contains('th-num'), '占比欄標頭為數值右對齊 class')

  const rows = root.querySelectorAll('.api-overview-table tbody tr')
  assert.equal(rows.length, 6, '應有 6 種類別列')

  // 第一列為金額最大項快取寫入
  assert.match(rows[0].textContent ?? '', /快取寫入/)
  assert.match(rows[0].textContent ?? '', /\$4\.1083/)
  assert.match(rows[0].textContent ?? '', /82(\.0)?%/)

  // 最後兩列為 0 元（對話壓縮、伺服器工具），應排在最後且標註 is-zero
  const rowCompaction = rows[4]
  const rowServerTool = rows[5]
  assert.ok(rowCompaction.classList.contains('is-zero'), '0 元壓縮列排在後且帶 is-zero')
  assert.ok(rowServerTool.classList.contains('is-zero'), '0 元伺服器工具列排在後且帶 is-zero')
})

test('ApiBotCostOverview 渲染最貴回合清單，點擊跳轉觸發 onJumpToTurn', async () => {
  let jumpedTurnId: string | null = null
  const root = await mount(
    <ApiBotCostOverview
      botId="bot-claude-api"
      initialData={mockOverviewData}
      onJumpToTurn={(id) => {
        jumpedTurnId = id
      }}
    />,
  )

  const turnsSection = root.querySelector('.api-overview-top-turns')
  assert.ok(turnsSection, '最貴回合區塊存在')

  const turnRows = root.querySelectorAll('.api-overview-turn-row')
  assert.equal(turnRows.length, 2, '應列出 2 筆最貴回合')

  const firstTurn = turnRows[0]
  assert.match(firstTurn.textContent ?? '', /#1/)
  assert.match(firstTurn.textContent ?? '', /\$2\.5052/)

  const jumpBtn = firstTurn.querySelector<HTMLButtonElement>('.api-overview-jump-btn')
  assert.ok(jumpBtn, '跳轉按鈕存在')
  jumpBtn.click()

  assert.equal(jumpedTurnId, 'turn-expensive-1', '點擊跳至該回合應觸發 onJumpToTurn 並帶 turn_id')
})

test('ApiBotCostOverview 確保正文與標題高對比度可讀性（不使用 opacity 淡化正文）', async () => {
  const root = await mount(
    <ApiBotCostOverview botId="bot-claude-api" initialData={mockOverviewData} />,
  )

  const title = root.querySelector('.api-overview-title') as HTMLElement
  assert.ok(title, '標題存在')
  assert.notEqual(title.style.opacity, '0.5', '標題不得淡化')

  const conclusion = root.querySelector('.api-overview-conclusion-text') as HTMLElement
  assert.ok(conclusion, '結論文字存在')
  assert.notEqual(conclusion.style.opacity, '0.4', '結論文字不得淡化')
})

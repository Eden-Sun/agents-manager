import test from 'node:test'
import assert from 'node:assert/strict'
import { keysToSelect, parseChoiceMenu, sameChoices } from './tuiChoices.ts'

/** 2026-09-12 真機快照（bot `carbis`，185 欄）。分隔線夾在 5 與 6 之間是原樣。 */
const CARBIS = [
  '     9 OPEN [astro] .env.example 與實際用到的環境變數不同步',
  '────────────────────────────────────────────────────────────',
  ' ☐ 下一步',
  '',
  'robinstech-carbis-web 要先推進哪一塊？',
  '',
  '❯ 1. 先把 WIP commit 掉（建議）',
  '     57 個檔案未 commit，包含 #21 的評價 demo、icon 重構、legal，加上我剛做的 carbis 改名。我會拆成幾個主題 commit，push 到',
  '     feat/astro-migration。',
  '  2. 接 console 的回饋 API（#22）',
  '     把 api/feedback.ts 改成轉送 console + 瀏覽器產 feedback_id。',
  '  3. 清小 issue（#19/#16/#8/#9）',
  '     nginx server_name、佔位電話 0900000000、電話硬寫 20 處改用單一出處、.env.example 同步。',
  '  4. 全面換 carbis 品牌',
  '     客服信箱換 @carbis.com.tw、Vercel SITE_URL 與網域綁定、docker/CI 名稱、handoff source 值。',
  '  5. Type something.',
  '────────────────────────────────────────────────────────────',
  '  6. Chat about this',
  '',
  'Enter to select · ↑/↓ to navigate · Esc to cancel',
  '',
].join('\n')

test('讀得出真機那份選單：問題、六個選項、游標在第一項', () => {
  const menu = parseChoiceMenu(CARBIS)
  assert.ok(menu)
  assert.equal(menu.question, 'robinstech-carbis-web 要先推進哪一塊？')
  assert.equal(menu.choices.length, 6)
  assert.equal(menu.cursor, 0)
  assert.equal(menu.footer, true)
  assert.equal(menu.choices[0].title, '先把 WIP commit 掉（建議）')
  assert.equal(menu.choices[5].title, 'Chat about this')
  assert.equal(menu.choices[5].number, 6)
})

test('說明行被終端折掉的地方接回同一段（中文之間不補空白）', () => {
  const menu = parseChoiceMenu(CARBIS)
  assert.ok(menu)
  assert.ok(menu.choices[0].detail.includes('主題 commit，push 到feat/astro-migration。'))
  assert.equal(menu.choices[5].detail, '')
})

test('分隔線夾在選項中間不算選單結束（真機 5 與 6 之間就有一條）', () => {
  const menu = parseChoiceMenu(CARBIS)
  assert.deepEqual(
    menu?.choices.map((c) => c.number),
    [1, 2, 3, 4, 5, 6],
  )
})

test('claude 的權限框：外框線去掉、沒有腳註也認得', () => {
  const screen = [
    '╭───────────────────────────────────────────────╮',
    '│ Do you want to make this edit to api.rs?      │',
    '│ ❯ 1. Yes                                      │',
    '│   2. Yes, allow all edits this session        │',
    '│   3. No, and tell Claude what to do (esc)     │',
    '╰───────────────────────────────────────────────╯',
  ].join('\n')
  const menu = parseChoiceMenu(screen)
  assert.equal(menu?.question, 'Do you want to make this edit to api.rs?')
  assert.equal(menu?.choices.length, 3)
  assert.equal(menu?.footer, false)
  assert.equal(menu?.cursor, 0)
})

test('游標不在第一項時算得出來', () => {
  const menu = parseChoiceMenu(['選哪個？', '  1. A', '❯ 2. B', '  3. C', 'Enter to select'].join('\n'))
  assert.equal(menu?.cursor, 1)
  assert.deepEqual(keysToSelect(menu, 2), ['down', 'enter'])
  assert.deepEqual(keysToSelect(menu, 0), ['up', 'enter'])
  assert.deepEqual(keysToSelect(menu, 1), ['enter'])
})

test('十項以上照樣算得出按幾次', () => {
  const lines = ['很多選項', ...Array.from({ length: 12 }, (_, i) => `${i === 0 ? '❯' : ' '} ${i + 1}. 第 ${i + 1} 項`)]
  const menu = parseChoiceMenu(lines.join('\n'))
  assert.equal(menu?.choices.length, 12)
  assert.equal(keysToSelect(menu, 11).length, 12)
})

test('認不出來的一律回 null：沒有游標、沒有連號、正文裡的清單、離畫面底太遠', () => {
  // 游標捲出畫面 → 算不出 ↓ 幾次
  assert.equal(parseChoiceMenu(['  1. A', '  2. B', 'Enter to select'].join('\n')), null)
  // 兩個 `>` 是引用不是選單
  assert.equal(parseChoiceMenu(['> 1. A', '> 2. B'].join('\n')), null)
  // 不從 1 開始
  assert.equal(parseChoiceMenu(['❯ 2. A', '  3. B'].join('\n')), null)
  // 只有一項
  assert.equal(parseChoiceMenu(['❯ 1. A'].join('\n')), null)
  // 選單在畫面上方，底下還有一整頁輸出 → 那是正文，不是現在在等的東西
  assert.equal(
    parseChoiceMenu(['❯ 1. A', '  2. B', ...Array.from({ length: 12 }, (_, i) => `輸出第 ${i} 行`)].join('\n')),
    null,
  )
  assert.equal(parseChoiceMenu(''), null)
  assert.equal(parseChoiceMenu(null), null)
})

test('問題往上只收到空白行／正文符號為止，不會把工具輸出也吃進來', () => {
  const menu = parseChoiceMenu(
    ['⏺ Bash(ls)', '  ⎿  a.txt', '要不要繼續？', '❯ 1. 好', '  2. 不要', 'Enter to select'].join('\n'),
  )
  assert.equal(menu?.question, '要不要繼續？')
})

test('sameChoices 認的是「同一份選單還在嗎」，游標移動不算換', () => {
  const a = parseChoiceMenu(['問？', '❯ 1. A', '  2. B', 'Enter to select'].join('\n'))
  const b = parseChoiceMenu(['問？', '  1. A', '❯ 2. B', 'Enter to select'].join('\n'))
  const c = parseChoiceMenu(['問？', '❯ 1. A', '  2. 換了', 'Enter to select'].join('\n'))
  assert.ok(a && b && c)
  assert.equal(sameChoices(a, b), true)
  assert.equal(sameChoices(a, c), false)
})

test('codex 的 `> 1.` 也認得（游標記號只有一個）', () => {
  const menu = parseChoiceMenu(
    ['✨ Update available! 0.153.4 -> 0.154.0', '> 1. Update now', '  2. Skip', '  3. Skip until next version'].join(
      '\n',
    ),
  )
  assert.equal(menu?.choices.length, 3)
  assert.equal(menu?.cursor, 0)
})

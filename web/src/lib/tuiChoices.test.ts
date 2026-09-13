import test from 'node:test'
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import {
  customAnswerShown,
  isActionChoice,
  isTypeSomething,
  keysToMove,
  keysToSelect,
  parseChoiceMenu,
  sameChoices,
} from './tuiChoices.ts'

/**
 * 2026-09-12 第三輪真機快照（bot `carbis`，185×54）：claude 的**多分頁 ＋ 多選**
 * AskUserQuestion，使用者回報「第二個分頁的問題沒有辦法正確解析」——整塊選單長不出來。
 */
const MULTI = readFileSync(new URL('./__fixtures__/carbis-multiselect.txt', import.meta.url), 'utf8')

/**
 * 2026-09-12 第七輪真機快照：同一份問卷走到最後的 review／confirm 頁。使用者回報「怎麼不能
 * 點其他分頁」——分頁列跟選項之間隔著一整段 Review，原本只看問題正上方那一行就找不到它。
 */
const REVIEW = readFileSync(new URL('./__fixtures__/carbis-review.txt', import.meta.url), 'utf8')

/** 2026-09-13 真機：多分頁**單選**（沒有 `[ ]`）。草稿若把 `checked===null` 當 disabled 就勾不起來。 */
const OPU_RADIO = readFileSync(new URL('./__fixtures__/opu-radio.txt', import.meta.url), 'utf8')
const TYPE_IDLE = readFileSync(new URL('./__fixtures__/type-something-idle.txt', import.meta.url), 'utf8')
const TYPE_TYPED = readFileSync(new URL('./__fixtures__/type-something-typed.txt', import.meta.url), 'utf8')
const TYPE_REVIEW = readFileSync(new URL('./__fixtures__/type-something-review.txt', import.meta.url), 'utf8')

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


test('多選：分頁列讀得出每一題答過沒有（☒／☐／✔ Submit）', () => {
  const menu = parseChoiceMenu(MULTI)
  assert.ok(menu)
  assert.deepEqual(menu.tabs, [
    { label: '編輯方式', done: true, submit: false },
    { label: '功能', done: false, submit: false },
    { label: 'Submit', done: false, submit: true },
  ])
})

test('多選：`[ ]` 讀成 checked、本文不留方括號，整份是 multi', () => {
  const menu = parseChoiceMenu(MULTI)
  assert.ok(menu)
  assert.equal(menu.multi, true)
  assert.equal(menu.choices.length, 6)
  assert.equal(menu.choices[0].title, '站內搜尋、分類、標籤')
  assert.equal(menu.choices[0].checked, false)
  assert.equal(menu.choices[0].current, true)
  // `Chat about this` 沒有方框，不是可勾選的選項
  assert.equal(menu.choices[5].checked, null)
  assert.equal(menu.question, '除了看文章，還需要哪些功能？（這些是決定能不能純静態的關鍵）')
})

test('多選：說明只縮排到編號那一欄（2）也算說明，不會把選單切斷', () => {
  const menu = parseChoiceMenu(MULTI)
  assert.equal(menu?.choices[1].detail, '需要伺服器端收件與儲存（現有 serverless function + console 可承接）。')
  assert.deepEqual(
    menu?.choices.map((c) => c.number),
    [1, 2, 3, 4, 5, 6],
  )
})

test('多選：沒有編號的 `Submit` 列算進游標要走的格數，不然點第 6 項會停在它上面', () => {
  const menu = parseChoiceMenu(MULTI)
  assert.ok(menu)
  assert.deepEqual(menu.submit, { after: 4, current: false })
  // 游標在第 1 項：第 5 項 4 格、Submit 5 格、第 6 項 6 格（中間多一列 Submit）
  assert.equal(keysToMove(menu, 4)?.length, 4)
  assert.equal(keysToMove(menu, 'submit')?.length, 5)
  assert.equal(keysToMove(menu, 5)?.length, 6)
})

test('多選：游標停在 Submit 那一列時仍算得出來（cursor 為 -1 但不是解析失敗）', () => {
  const menu = parseChoiceMenu(MULTI.replace('     Submit', '❯    Submit').replace('❯ 1. [ ]', '  1. [ ]'))
  assert.ok(menu)
  assert.equal(menu.cursor, -1)
  assert.equal(menu.submit?.current, true)
  assert.deepEqual(keysToMove(menu, 5), ['down'])
  assert.deepEqual(keysToMove(menu, 4), ['up'])
})

test('多選那份的腳註是 Tab/Arrow keys，照樣算腳註', () => {
  assert.equal(parseChoiceMenu(MULTI)?.footer, true)
})

test('上一輪的單選那份沒有退步：沒有分頁、沒有方框、沒有 Submit 列', () => {
  const menu = parseChoiceMenu(CARBIS)
  assert.ok(menu)
  assert.equal(menu.multi, false)
  assert.deepEqual(menu.tabs, [])
  assert.equal(menu.submit, null)
  assert.deepEqual(keysToSelect(menu, 3), ['down', 'down', 'down', 'enter'])
})


test('review 頁：分頁列跟選項之間隔著一整段 Review，照樣找得到（第七輪的根因）', () => {
  const menu = parseChoiceMenu(REVIEW)
  assert.ok(menu)
  assert.deepEqual(menu.tabs, [
    { label: '編輯方式', done: true, submit: false },
    { label: '功能', done: true, submit: false },
    { label: 'Submit', done: false, submit: true },
  ])
  // 全部答完了 → 現在這一格應該是送出頁
  assert.equal(menu.tabAt, 2)
})

test('review 頁：每題的目前答案讀得出來，選項是 Submit answers / Cancel', () => {
  const menu = parseChoiceMenu(REVIEW)
  assert.ok(menu)
  assert.deepEqual(menu.review, [
    { question: '文章是誰寫、怎麼上稿？', answer: '非工程師要能自己發' },
    { question: '除了看文章，還需要哪些功能？（這些是決定能不能純静態的關鍵）', answer: 'SEO 是主要目的' },
  ])
  assert.deepEqual(
    menu.choices.map((c) => c.title),
    ['Submit answers', 'Cancel'],
  )
  assert.equal(menu.question, 'Ready to submit your answers?')
  assert.equal(menu.cursor, 0)
  // review 那段不能被當成問題的一部分吃進來
  assert.equal(menu.multi, false)
})

test('一般選項頁沒有 review 那段，tabAt 是第一個還沒答的', () => {
  const menu = parseChoiceMenu(MULTI)
  assert.ok(menu)
  assert.deepEqual(menu.review, [])
  assert.equal(menu.tabAt, 1)
})

test('沒有分頁的單選：tabAt 是 null、review 空的（沒有退步）', () => {
  const menu = parseChoiceMenu(CARBIS)
  assert.ok(menu)
  assert.equal(menu.tabAt, null)
  assert.deepEqual(menu.review, [])
  assert.deepEqual(menu.tabs, [])
})

test('多分頁單選（opu 真畫面）：五個分頁、沒有核取方塊、Type something 與 Chat 都是編號列', () => {
  const menu = parseChoiceMenu(OPU_RADIO)
  assert.ok(menu)
  assert.equal(menu.multi, false)
  assert.equal(menu.tabs.length, 5)
  assert.deepEqual(
    menu.tabs.map((t) => t.label),
    ['換身分門檻', 'Fable 用完', '執行者 kind', '推 main 失敗', 'Submit'],
  )
  assert.equal(menu.choices.length, 5)
  assert.equal(menu.choices.every((c) => c.checked === null), true)
  assert.equal(menu.cursor, 0)
  assert.equal(isActionChoice(menu.choices[3]), true)
  assert.equal(isActionChoice(menu.choices[4]), true)
  assert.ok(menu.question?.includes('cc2 撞到哪一種才換下一個身分'))
  assert.ok(menu.choices[0].detail.includes('reviewer 建議'))
})

test('Type something 游標停上去：標題還是那句、腳註多了 ctrl+g', () => {
  const menu = parseChoiceMenu(TYPE_IDLE)
  assert.ok(menu)
  assert.equal(menu.cursor, 3)
  assert.equal(isTypeSomething(menu.choices[3]), true)
  assert.equal(menu.choices[3].title, 'Type something.')
})

test('貼上之後標題被取代、換行進說明；customAnswerShown 認得出來', () => {
  const menu = parseChoiceMenu(TYPE_TYPED)
  assert.ok(menu)
  assert.equal(isTypeSomething(menu.choices[2] ?? { title: '' }), false)
  assert.equal(menu.choices[2].title, '第一行')
  assert.ok(menu.choices[2].detail.includes('第二行中文'))
  assert.equal(customAnswerShown(menu.choices[2], '第一行\n第二行中文'), true)
})

test('答完進 review：自訂答案（含換行）讀得出來', () => {
  const menu = parseChoiceMenu(TYPE_REVIEW)
  assert.ok(menu)
  assert.ok(menu.review.length >= 2)
  assert.ok(menu.review[0].answer.includes('測試中文'))
  assert.ok(menu.review[1].answer.includes('第一行'))
  assert.ok(menu.review[1].answer.includes('第二行中文'))
})

// 標題列底下那三列（額度、未讀、context/git）的版面約束，量給人看的數字。
//
// 2026-09-12 的 goal（docs/goals/header-chrome-2026-09-12.md）定了三條，這支腳本就是它們的
// 回歸測試——單看截圖分不出「被捲軸剪掉」跟「被上面那列蓋住」，得看數字：
//
//   1. 手機額度每一格**只寫一個**窗口，不折行（`.quota-compact-win` 一格一顆）。
//   2. 未讀列最多**一列**高，放不下往右捲（`.unread-bar` 的高度 ≈ 一顆晶片 + 內距）。
//   3. `.context-bar` 不用 `position` 蓋住 `.msg-list`（兩者 static，訊息從 context bar 的
//      下緣才開始）。
//   4. 390px 沒有整頁橫向捲動（`documentElement.scrollWidth === innerWidth`）。
//
// 跑法（要有真 daemon 的 dev server，見 CLAUDE.md）：
//   cd web && npx vite            # 5173
//   node scripts/verify-header-chrome.mjs
//   OUT=docs/screenshots/header-chrome node scripts/verify-header-chrome.mjs   # 順便存圖
import { spawn } from 'node:child_process'
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'

const TOKEN = process.env.AM_TOKEN ?? readFileSync(`${process.env.HOME}/.config/agents-manager/ui-token`, 'utf8').trim()
const URL_BASE = `${process.env.BASE ?? 'http://127.0.0.1:5173'}/?token=${TOKEN}`
const OUT = process.env.OUT ?? null
const PORT = Number(process.env.CDP_PORT ?? 9414)
if (OUT) mkdirSync(OUT, { recursive: true })

const chrome = spawn('/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', [
  '--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run',
  `--user-data-dir=/tmp/am-header-chrome-${PORT}`, '--window-size=1440,900', URL_BASE,
], { stdio: 'ignore' })

const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws
let id = 0
const pending = new Map()
for (let i = 0; i < 100; i += 1) {
  try {
    const pages = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json()
    const page = pages.find((t) => t.type === 'page' && t.url.startsWith('http'))
    if (page) { ws = new WebSocket(page.webSocketDebuggerUrl); break }
  } catch {}
  await sleep(250)
}
if (!ws) throw new Error(`Chrome did not expose CDP on ${PORT}`)
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.reject(new Error(JSON.stringify(m.error))) : p.resolve(m.result) }
}
await new Promise((r) => (ws.onopen = r))
const send = (method, params = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method, params })); return new Promise((resolve, reject) => pending.set(i, { resolve, reject })) }
await send('Runtime.enable')
await send('Page.enable')
const ev = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description ?? 'eval failed')
  return r.result?.value
}
const shot = async (name) => {
  if (!OUT) return
  await sleep(350)
  const { data } = await send('Page.captureScreenshot', { format: 'png' })
  writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64'))
  console.log(`saved ${name}.png`)
}
const size = async (width, height, mobile = false) => {
  await send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: mobile ? 2 : 1, mobile })
  // 額度列的收合是掛在 resize 上的，headless 改 metrics 不會自己派事件。
  await ev("window.dispatchEvent(new Event('resize'))")
  await sleep(500)
}

const PROBE = `(() => {
  const box = (sel) => { const el = document.querySelector(sel); if (!el) return null
    const r = el.getBoundingClientRect()
    return { top: Math.round(r.top), bottom: Math.round(r.bottom), h: Math.round(r.height), position: getComputedStyle(el).position } }
  const bar = document.querySelector('.unread-bar')
  const chip = bar?.querySelector('.unread-chip')
  return {
    innerWidth: window.innerWidth,
    docScrollWidth: document.documentElement.scrollWidth,
    unreadBar: box('.unread-bar'),
    unreadChipH: chip ? Math.round(chip.getBoundingClientRect().height) : null,
    unreadChips: bar ? bar.querySelectorAll('.unread-chip').length : 0,
    unreadScrolls: bar ? bar.scrollWidth > bar.clientWidth : false,
    contextBar: box('.context-bar'),
    msgList: box('.msg-list'),
    quotaCells: [...document.querySelectorAll('.quota-hp')].map((c) => ({
      wins: c.querySelectorAll('.quota-compact-win').length,
      h: Math.round(c.getBoundingClientRect().height),
    })),
  }
})()`

const fails = []
const check = (ok, line) => { console.log(`${ok ? 'PASS' : 'FAIL'}  ${line}`); if (!ok) fails.push(line) }

await send('Page.navigate', { url: URL_BASE })
await sleep(4000)
await ev("(() => { const rows = [...document.querySelectorAll('.bot-row')]; rows[0]?.click() })()")
await sleep(1800)

for (const width of [1380, 1500]) {
  await size(width, 900)
  const m = await ev(PROBE)
  const oneRow = m.unreadBar && m.unreadChipH && m.unreadBar.h <= m.unreadChipH + 14
  check(oneRow, `${width}px 未讀列一列高：${m.unreadBar?.h}px（晶片 ${m.unreadChipH}px，${m.unreadChips} 顆，${m.unreadScrolls ? '橫捲' : '排得下'}）`)
  const stacked = m.contextBar && m.msgList && m.contextBar.position === 'static' && m.msgList.top >= m.contextBar.bottom
  check(stacked, `${width}px context 列不蓋訊息：context ${m.contextBar?.top}–${m.contextBar?.bottom}（${m.contextBar?.position}）、msg-list 從 ${m.msgList?.top} 起`)
  await shot(`desktop-chat-${width}`)
}

await size(390, 844, true)
const m = await ev(PROBE)
check(m.docScrollWidth === m.innerWidth, `390px 無整頁橫向捲動：scrollWidth ${m.docScrollWidth} / innerWidth ${m.innerWidth}`)
check(
  m.unreadBar && m.unreadChipH && m.unreadBar.h <= m.unreadChipH + 14,
  `390px 未讀列一列高：${m.unreadBar?.h}px（晶片 ${m.unreadChipH}px，${m.unreadChips} 顆，${m.unreadScrolls ? '橫捲' : '排得下'}）`,
)
check(
  m.quotaCells.length > 0 && m.quotaCells.every((c) => c.wins === 1),
  `390px 額度每格一個窗口：${m.quotaCells.map((c) => `${c.wins}@${c.h}px`).join(' ')}`,
)
await shot('mobile-chat-390')

console.log(fails.length === 0 ? '\nALL PASS' : `\n${fails.length} FAILED`)
ws.close()
chrome.kill()
process.exit(fails.length === 0 ? 0 : 1)

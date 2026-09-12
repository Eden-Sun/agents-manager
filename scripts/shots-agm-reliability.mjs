// AGM 總管面板的畫面證據（驗收狀態、系統故障、遠端入口能力限制）。
//
// **走 mock，不碰正式 daemon 也不碰 5173**：`VITE_MOCK=1`，自己起一個 vite（預設 5199）。
// 這份改動會改交辦狀態與遠端入口的說法，用真 session 驗等於燒使用者的額度。
//
//   cd web && VITE_MOCK=1 bunx vite --port 5199 &
//   OUT=docs/screenshots/agm-reliability node scripts/shots-agm-reliability.mjs
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'

const PORT = process.env.UI_PORT ?? 5199
const URL_BASE = `http://127.0.0.1:${PORT}/`
const OUT = process.env.OUT ?? '/tmp/am-agm-shots'
mkdirSync(OUT, { recursive: true })
const CDP = 9391
const chrome = spawn(
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ['--headless=new', `--remote-debugging-port=${CDP}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run',
   '--user-data-dir=/tmp/am-agm-shots-profile', '--window-size=1440,900', URL_BASE],
  { stdio: 'ignore' },
)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0
const pending = new Map()
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) {
  try {
    const l = await (await fetch(`http://127.0.0.1:${CDP}/json/list`)).json()
    const p = l.find((t) => t.type === 'page' && t.url.startsWith('http'))
    if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break }
  } catch {}
  await sleep(250)
}
ws.onmessage = (e) => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } }
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable')
const ev = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })
  return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value
}
const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('saved', name) }

await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)

const openPanel = async () => {
  await ev(`[...document.querySelectorAll('button')].find(b=>b.textContent.includes('AGM 總管'))?.click()`)
  await sleep(900)
}
await openPanel()
await shot('1-before-setup')

// 建立 + 啟動：mock 的啟動會給一筆 delivered 與一筆 awaiting_review 的交辦。
await ev(`[...document.querySelectorAll('.agm-panel button')].find(b=>b.textContent.includes('建立 AGM 環境'))?.click()`)
await sleep(700)
await ev(`[...document.querySelectorAll('.agm-panel button')].find(b=>b.textContent.trim().startsWith('啟動'))?.click()`)
await sleep(900)
await shot('2-awaiting-review')

// 面板往下捲，看交辦清單與系統故障那一段。
await ev(`document.querySelector('.agm-panel')?.scrollIntoView(false); document.querySelector('.modal-body,.agm-panel')?.scrollTo(0, 9999)`)
await shot('3-assignments-and-incidents')

// 檢查文案：不能出現「已連線」，等驗收那筆不能被畫成已完成。
const text = await ev(`document.querySelector('.agm-panel')?.innerText ?? ''`)
const checks = [
  ['遠端入口不宣稱已連線', !text.includes('已連線')],
  ['遠端入口說明能力限制', text.includes('沒有可靠的 Remote Control 觀測來源')],
  ['等驗收的交辦標成「等驗收」', text.includes('等驗收')],
  ['等驗收的交辦沒有被說成已完成', !text.includes('已完成')],
  ['系統故障有自己的區塊', text.includes('系統故障')],
]
let bad = 0
for (const [name, ok] of checks) { console.log(ok ? 'ok   -' : 'FAIL -', name); if (!ok) bad++ }

await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true })
await sleep(600)
await shot('4-mobile-390')

chrome.kill()
process.exit(bad === 0 ? 0 : 1)

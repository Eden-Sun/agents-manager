// 群組任務「完成後追問／追加修改」的畫面證據。
//
// **走 mock、走自己的 port**（AGM 核准），不碰正式 daemon 也不碰 5173：
//   cd web && VITE_MOCK=1 bunx vite --port 5207 &
//   OUT=docs/screenshots/mission-followup node scripts/shots-mission-followup.mjs
//
// 桌機一張、手機 390px 兩張（收合／展開追問），最後檢查文案：追問不能講得像會改東西，
// 已完成的成果不能因為被追問就變成進行中。
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'

const PORT = process.env.UI_PORT ?? 5207
const URL_BASE = `http://127.0.0.1:${PORT}/`
const OUT = process.env.OUT ?? '/tmp/am-mission-followup'
mkdirSync(OUT, { recursive: true })
const CDP = 9393
const chrome = spawn(
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ['--headless=new', `--remote-debugging-port=${CDP}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run',
   '--user-data-dir=/tmp/am-mission-followup-profile', '--window-size=1440,900', URL_BASE],
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
const click = (text) => ev(`[...document.querySelectorAll('button')].find(b=>b.textContent.trim()===${JSON.stringify(text)})?.click(); true`)

const openDoneCard = async () => {
  // 群組任務在 project 的群組頁；先進群組，再把「已完成」那段展開。
  await ev(`document.querySelector('.project-head')?.click(); true`)
  await sleep(900)
  await ev(`[...document.querySelectorAll('button')].find(b=>/已完成/.test(b.textContent))?.click(); true`)
  await sleep(500)
  await ev(`[...document.querySelectorAll('.mission-done-row-head')][0]?.click(); true`)
  await sleep(900)
}

await send('Emulation.setDeviceMetricsOverride', { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false })
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)
await openDoneCard()
await shot('1-done-card-desktop')
// 入口的文案要在「還沒打開表單」時看——表單一開，按鈕就變成取消／送出了。
const entryText = await ev(`document.querySelector('.mission-followup')?.innerText ?? ''`)
const doneWhenBefore = await ev(`document.querySelector('.mission-done-row .mission-done-when')?.innerText ?? ''`)

// 追問：送出後 mock 的 AGM 會回一句，畫面要串成一問一答。
await click('追問')
await sleep(300)
await ev(`(() => { const t = document.querySelector('.mission-followup-input'); if (!t) return false;
  const set = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype,'value').set;
  set.call(t, '這個改動會影響登入嗎？'); t.dispatchEvent(new Event('input', { bubbles: true })); return true })()`)
await sleep(200)
await click('送出追問')
await sleep(1800)
await shot('2-qna-thread-desktop')
const qnaText = await ev(`document.querySelector('.mission-qna')?.innerText ?? ''`)
const doneMark = await ev(`document.querySelector('.mission-done-row .mission-done-mark')?.innerText?.trim() ?? ''`)
const doneWhen = await ev(`document.querySelector('.mission-done-row .mission-done-when')?.innerText ?? ''`)

// 手機 390：收合與展開都要點得到。
await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true })
await sleep(700)
await shot('3-mobile-390')
await click('追加修改')
await sleep(500)
await shot('4-mobile-revise-390')

const text = entryText + '\n' + qnaText + '\n' + (await ev(`document.body.innerText`))
const box = await ev(`(() => { const b = [...document.querySelectorAll('.mission-followup-actions .btn')][0];
  if (!b) return null; const r = b.getBoundingClientRect(); return { w: Math.round(r.width), h: Math.round(r.height) } })()`)
const checks = [
  ['成果卡有追問入口', text.includes('追問')],
  ['成果卡有追加修改入口', text.includes('追加修改')],
  ['追問串起了 AGM 的回覆', text.includes('不影響登入流程')],
  // 「追問不會動到成果」要對著那張卡本身驗，不是對整頁文字——整頁本來就有別的進行中任務。
  ['追問後那張成果卡還是已完成', doneMark === '✔'],
  ['追問後完成時間沒有變', doneWhen === doneWhenBefore],
  ['390px 按鈕高度 >= 44px', Boolean(box && box.h >= 44)],
]
let bad = 0
for (const [name, ok] of checks) { console.log(ok ? 'ok   -' : 'FAIL -', name); if (!ok) bad++ }
if (box) console.log(`   （按鈕 ${box.w}x${box.h}）`)

chrome.kill()
process.exit(bad === 0 ? 0 : 1)

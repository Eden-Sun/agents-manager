// Screenshots of the Herdr 版本 card in 環境設定, against a **mock** vite (no live daemon, no live data):
//   cd web && VITE_MOCK=1 bunx vite --port 5219
//   OUT=docs/screenshots/herdr-updates PORT=5219 node scripts/shots-herdr-updates.mjs
// Mobile 390px is the one that matters (the card lives in a modal that has to stay tappable there).
import { spawn } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
const VITE = process.env.PORT ?? '5219'
const URL_BASE = `http://127.0.0.1:${VITE}/`
const OUT = process.env.OUT ?? '/tmp/am-herdr-updates'
mkdirSync(OUT, { recursive: true })
const CDP = 9391
const chrome = spawn(
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
  ['--headless=new', `--remote-debugging-port=${CDP}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-herdr-updates-profile', '--window-size=1440,900', URL_BASE],
  { stdio: 'ignore' },
)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws
let id = 0
const pending = new Map()
const send = (m, p = {}) => {
  const i = ++id
  ws.send(JSON.stringify({ id: i, method: m, params: p }))
  return new Promise((res, rej) => pending.set(i, { res, rej }))
}
for (let i = 0; i < 80; i++) {
  try {
    const l = await (await fetch(`http://127.0.0.1:${CDP}/json/list`)).json()
    const p = l.find((t) => t.type === 'page' && t.url.startsWith('http'))
    if (p) {
      ws = new WebSocket(p.webSocketDebuggerUrl)
      break
    }
  } catch {}
  await sleep(250)
}
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (m.id && pending.has(m.id)) {
    const p = pending.get(m.id)
    pending.delete(m.id)
    m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result)
  }
}
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable')
await send('Page.enable')
const ev = async (expression) => {
  const r = await send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true })
  return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value
}
const shot = async (name) => {
  await sleep(400)
  const { data } = await send('Page.captureScreenshot', { format: 'png' })
  writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64'))
  console.log('saved', name)
}
const dark = (on) => send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: on ? 'dark' : 'light' }] })
const clickText = (sel, text) => ev(`[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.includes(${JSON.stringify(text)}))?.click(), true`)
/** Open the sidebar drawer when it is a drawer (mobile), then 環境設定. */
const openEnv = async () => {
  await ev(`document.querySelector('button[aria-label="開啟側邊欄"]')?.click(), true`)
  await sleep(600)
  await clickText('button.disclosure', '環境設定')
  await sleep(900)
  // Scroll the card into view inside the modal.
  await ev(`[...document.querySelectorAll('.env-sec h3')].find(h=>h.textContent.includes('Herdr'))?.scrollIntoView({block:'start'}), true`)
  await sleep(300)
}

for (const [w, h, mobile, tag] of [
  [390, 844, true, '390'],
  [1440, 900, false, '1440'],
]) {
  for (const isDark of [true, false]) {
    await send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: mobile ? 2 : 1, mobile })
    await dark(isDark)
    await send('Page.navigate', { url: URL_BASE })
    await sleep(2500)
    await openEnv()
    await shot(`card-${tag}-${isDark ? 'dark' : 'light'}`)
    // Expanded release notes: the cross-version list is the part that can overflow.
    await clickText('.hu-notes button', '看更新內容')
    await sleep(500)
    await ev(`document.querySelector('.hu-notes-body')?.scrollIntoView({block:'center'}), true`)
    await shot(`notes-${tag}-${isDark ? 'dark' : 'light'}`)
  }
}
// The unread hint on the 環境設定 row (must not need the modal to be visible).
await send('Emulation.setDeviceMetricsOverride', { width: 390, height: 844, deviceScaleFactor: 2, mobile: true })
await dark(true)
await send('Page.navigate', { url: URL_BASE })
await sleep(2500)
await ev(`document.querySelector('button[aria-label="開啟側邊欄"]')?.click(), true`)
await sleep(700)
await ev(`document.querySelector('.hu-dot')?.scrollIntoView({block:'center'}), true`)
await shot('unread-dot-390-dark')
console.log(await ev(`JSON.stringify({dot: document.querySelector('.hu-dot')?.textContent ?? null})`))
chrome.kill()
process.exit(0)

/**
 * Bot 設定面板 — 真後端驗收（agents-managerd serve @ 127.0.0.1:7788）。
 * 只動本機的 `am-codex`：改模型 → gpt-5.5 → 儲存（needs_restart）→ 立即重啟。
 *
 * 先在 web/ 內啟動：npx vite --port 5184
 * 再執行：node scripts/demo-botsettings-real.mjs
 */
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'

const URL_BASE = 'http://127.0.0.1:5184/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const TARGET = 'am-codex'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(
  CHROME,
  ['--headless=new', '--remote-debugging-port=9344', '--disable-gpu', '--hide-scrollbars', '--no-first-run',
   '--user-data-dir=/tmp/am-cdp-botset-real', '--window-size=1280,900', URL_BASE],
  { stdio: 'ignore' },
)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0
const pending = new Map()
const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) {
  try {
    const l = await (await fetch('http://127.0.0.1:9344/json/list')).json()
    const p = l.find((t) => t.type === 'page' && t.url.startsWith('http'))
    if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break }
  } catch {}
  await sleep(250)
}
ws.onmessage = (e) => {
  const m = JSON.parse(e.data)
  if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) }
  else events.push(m)
}
await new Promise((r) => (ws.onopen = r))
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(3000)

const ev = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })
  return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value
}
const shot = async (name) => { await sleep(400); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const setVal = (sel, v) => ev(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});if(!el)return 'missing';const proto=el.tagName==='SELECT'?HTMLSelectElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,${JSON.stringify(v)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return 'ok'})()`)
const clickText = (sel, text) => ev(`(()=>{const b=[...document.querySelectorAll(${JSON.stringify(sel)})].find(x=>x.textContent.trim().includes(${JSON.stringify(text)}));if(!b)return 'missing';if(b.disabled)return 'disabled';b.click();return 'clicked'})()`)
const lampOf = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent===${JSON.stringify(name)});return r?[...r.querySelector('.lamp').classList].join(' ')+' | '+r.querySelector('.bot-sub').textContent:'no row'})()`)

console.log('rows:', await ev(`[...document.querySelectorAll('.bot-row .bot-name')].map(e=>e.textContent).join(', ')`))
console.log(`${TARGET} before:`, await lampOf(TARGET))

console.log('gear:', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent===${JSON.stringify(TARGET)});if(!r)return 'no row';r.querySelector('.icon-btn.gear').click();return 'opened'})()`))
await sleep(700)
console.log('panel:', await ev(`document.querySelector('.bs-head')?.textContent`))
console.log('name disabled:', await ev(`document.querySelector('.bs-body input[type=text]')?.disabled`))
console.log('model options:', await ev(`[...document.querySelectorAll('.bs-body select')[0].options].map(o=>o.textContent).join(' | ')`))
await shot('112-real-bot-settings')

console.log('set model gpt-5.5:', await setVal('.bs-body select', 'gpt-5.5'))
await sleep(200)
console.log('changed:', await ev(`document.querySelector('.bs-actions .hint').textContent`))
console.log('save:', await clickText('.bs-actions button', '儲存'))
await sleep(1500)
console.log('banner:', await ev(`document.querySelector('.bs-banner')?.textContent`))
await shot('113-real-needs-restart')

console.log('restart:', await clickText('.bs-banner button', '立即重啟'))
await sleep(1500)
console.log('lamp during restart:', await lampOf(TARGET))
await shot('114-real-restarting')
for (let i = 0; i < 40; i++) {
  const l = await lampOf(TARGET)
  if (l.includes('lamp-idle') || l.includes('lamp-working')) break
  await sleep(1000)
}
console.log('lamp after restart:', await lampOf(TARGET))
console.log('banner gone:', await ev(`document.querySelector('.bs-banner')===null`))
console.log('model tag in head:', await ev(`document.querySelector('.main-title .model-tag')?.textContent`))
await shot('115-real-restarted')

for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log('EXC', JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)

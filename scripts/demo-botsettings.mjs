/**
 * Bot 設定面板（API.md v3.3）驗收：mock 模式走一遍
 *   開設定 → 改模型 opus / args → 儲存（needs_restart）→ 立即重啟 → starting→idle
 *   → 停止並改名 → 刪除 bot → 列表消失
 *
 * 先在 web/ 內啟動：VITE_MOCK=1 npx vite --port 5183
 * 再執行：node scripts/demo-botsettings.mjs
 */
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'

const URL_BASE = 'http://127.0.0.1:5183/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(
  CHROME,
  ['--headless=new', '--remote-debugging-port=9343', '--disable-gpu', '--hide-scrollbars', '--no-first-run',
   '--user-data-dir=/tmp/am-cdp-botset', '--window-size=1280,900', URL_BASE],
  { stdio: 'ignore' },
)
const sleep = (ms) => new Promise((r) => setTimeout(r, ms))
let ws, id = 0
const pending = new Map()
const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) {
  try {
    const l = await (await fetch('http://127.0.0.1:9343/json/list')).json()
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
const setViewport = (w, h) => send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 2, mobile: false })
await setViewport(1280, 900)
await send('Page.navigate', { url: URL_BASE }); await sleep(2200)

const ev = async (expr) => {
  const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true })
  return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value
}
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const setVal = (sel, v) => ev(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});if(!el)return 'missing '+${JSON.stringify(sel)};const proto=el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,${JSON.stringify(v)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return 'ok'})()`)
const clickText = (sel, text) => ev(`(()=>{const b=[...document.querySelectorAll(${JSON.stringify(sel)})].find(x=>x.textContent.trim().includes(${JSON.stringify(text)}));if(!b)return 'missing ${text}';if(b.disabled)return 'disabled ${text}';b.click();return 'clicked ${text}'})()`)
const lampOf = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent===${JSON.stringify(name)});return r?[...r.querySelector('.lamp').classList].join(' ')+' | '+r.querySelector('.bot-sub').textContent:'no row'})()`)
const rows = () => ev(`[...document.querySelectorAll('.bot-row .bot-name')].map(e=>e.textContent).join(', ')`)

console.log('rows:', await rows())

// 0) 先啟動 am-claude，這樣才有 active Run 可測 needs_restart / 改名鎖定
console.log('start am-claude:', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent==='am-claude');r.click();const b=[...r.querySelectorAll('.mini-btn')].find(b=>b.textContent.trim()==='啟動');b.click();return 'started'})()`))
await sleep(2200)
console.log('am-claude lamp:', await lampOf('am-claude'))

// 1) 由 bot 列的齒輪開啟設定面板
console.log('gear:', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent==='am-claude');const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened'})()`))
await sleep(500)
console.log('panel:', await ev(`document.querySelector('.bs-head')?.textContent`))
console.log('name disabled (active run):', await ev(`document.querySelector('.bs-body input[type=text]').disabled`))
console.log('model options:', await ev(`[...document.querySelectorAll('.bs-body select')[0].options].map(o=>o.textContent).join(' | ')`))
await shot('100-bot-settings')

// 2) 改模型 → opus，改 args
console.log('set model:', await setVal('.bs-body select', 'opus'))
console.log('set args:', await ev(`(()=>{const ins=[...document.querySelectorAll('.bs-body input[type=text]')];const el=ins[ins.length-1];Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(el,'--search --verbose');el.dispatchEvent(new Event('input',{bubbles:true}));return 'args='+el.value})()`))
await sleep(200)
console.log('changed:', await ev(`document.querySelector('.bs-actions .hint').textContent`))
await shot('101-bot-settings-model-changed')

// 3) 儲存 → needs_restart 提示
console.log('save:', await clickText('.bs-actions button', '儲存'))
await sleep(900)
console.log('banner:', await ev(`document.querySelector('.bs-banner')?.textContent`))
await shot('102-needs-restart')

// 4) 立即重啟 → starting → idle
console.log('restart:', await clickText('.bs-banner button', '立即重啟'))
await sleep(500)
console.log('lamp during restart:', await lampOf('am-claude'))
await shot('103-restarting')
await sleep(2000)
console.log('lamp after restart:', await lampOf('am-claude'))
console.log('banner gone:', await ev(`document.querySelector('.bs-banner')===null`))
await shot('104-restarted-idle')

// 5) 停止並改名
console.log('stop+rename:', await clickText('.bs-inline-note button', '停止並改名'))
await sleep(2200)
console.log('name enabled now:', await ev(`document.querySelector('.bs-body input[type=text]').disabled===false`))
console.log('set name:', await setVal('.bs-body input[type=text]', 'am-claude-fast'))
await sleep(150)
console.log('changed:', await ev(`document.querySelector('.bs-actions .hint').textContent`))
await shot('105-rename-after-stop')
console.log('save:', await clickText('.bs-actions button', '儲存'))
await sleep(900)
console.log('rows:', await rows())
console.log('banner:', await ev(`document.querySelector('.bs-banner')?.textContent`))
await shot('106-renamed')

// 6) 深色 + 900 寬
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'dark' }] })
await shot('107-dark')
await send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: 'light' }] })
await setViewport(900, 900); await sleep(400)
await shot('108-narrow-900')
await setViewport(1280, 900); await sleep(300)

// 7) 新增 Bot 表單的模型欄位
console.log('close settings:', await ev(`(()=>{document.querySelector('.bs-head .icon-btn').click();return 'closed'})()`))
await sleep(300)
console.log('open new bot:', await ev(`(()=>{const b=[...document.querySelectorAll('.sidebar-foot .disclosure')].find(b=>b.textContent.includes('新增 Bot'));b.click();return 'opened'})()`))
await sleep(400)
console.log('model field hint:', await ev(`[...document.querySelectorAll('.sidebar-foot .field')].map(f=>f.querySelector('span')?.textContent).filter(t=>t&&t.includes('模型')).join(' / ')`))
console.log('hint text:', await ev(`[...document.querySelectorAll('.sidebar-foot .hint')].map(e=>e.textContent).join(' | ')`))
await shot('109-new-bot-model-field')
console.log('close:', await clickText('.sidebar-foot .disclosure', '新增 Bot'))
await sleep(300)

// 8) 刪除 bot（confirm 自動接受）
await ev(`window.confirm = () => true`)
console.log('gear again:', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>r.querySelector('.bot-name').textContent==='am-claude-fast');r.querySelector('.icon-btn.gear').click();return 'opened'})()`))
await sleep(400)
await shot('110-delete-section')
console.log('delete:', await clickText('.bs-danger button', '刪除 Bot'))
await sleep(1200)
console.log('rows after delete:', await rows())
console.log('selected bot:', await ev(`document.querySelector('.main-title strong')?.textContent`))
await shot('111-after-delete')

for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log('EXC', JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)

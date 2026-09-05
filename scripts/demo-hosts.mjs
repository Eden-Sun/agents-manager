// SPEC §11.6 remote-host UI walkthrough against the mock backend (VITE_MOCK=1 npm run dev).
// Usage: node scripts/demo-hosts.mjs [http://127.0.0.1:5199/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5199/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9341
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-hosts', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
const metrics = (w, h) => send('Emulation.setDeviceMetricsOverride', { width: w, height: h, deviceScaleFactor: 2, mobile: false })
const dark = (on) => send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: on ? 'dark' : 'light' }] })
await metrics(1280, 900)
await dark(false)
await send('Page.navigate', { url: URL_BASE }); await sleep(2200)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const clickText = (t, sel = 'button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'MISSING '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
// React tracks input value on the DOM node; set it through the native setter so onChange fires.
const type = (sel, value, nth = 0) => ev(`(()=>{const el=document.querySelectorAll(${JSON.stringify(sel)})[${nth}];if(!el)return 'MISSING ${sel}';const d=Object.getOwnPropertyDescriptor(el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype,'value');d.set.call(el,${JSON.stringify(value)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return el.value})()`)
const clickInBotRow = (bot, t) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.bot-name')?.textContent.trim()===${JSON.stringify(bot)});if(!r)return 'MISSING row '+${JSON.stringify(bot)};const b=[...r.querySelectorAll('button')].find(x=>x.textContent.trim().includes(${JSON.stringify(t)}));if(!b)return 'MISSING btn '+${JSON.stringify(t)};b.click();return 'clicked '+${JSON.stringify(bot)}+' '+${JSON.stringify(t)}})()`)
const lamps = () => ev(`JSON.stringify([...document.querySelectorAll('.bot-row')].map(r=>[r.querySelector('.bot-name')?.textContent, [...r.querySelector('.lamp').classList].find(c=>c.startsWith('lamp-'))]))`)

console.log('== 1. 開啟主機面板並新增 m4p ==')
console.log(' ', await clickText('主機', '.sidebar-foot .disclosure'))
await sleep(300)
await shot('60-hosts-empty')
console.log('  name:', await type('.hosts-panel input[type=text]', 'm4p', 0))
console.log('  ssh :', await type('.hosts-panel input[type=text]', 'm4p@100.112.229.82', 1))
console.log(' ', await clickText('進階', '.hosts-panel .disclosure'))
await sleep(250)
await shot('61-new-host-form')
console.log(' ', await clickText('新增並連線', '.hosts-panel button'))
await sleep(1600)
console.log('  host rows:', await ev(`document.querySelectorAll('.host-row').length`), '| result:', await ev(`document.querySelector('.host-result')?.textContent`))
await shot('62-host-connected')

console.log('== 2. 新增遠端 Project（選擇器走遠端目錄樹）==')
console.log(' ', await clickText('新增 Project', '.sidebar-foot .disclosure'))
await sleep(350)
console.log('  host select:', await type('.form select', 'm4p'))
await sleep(250)
console.log(' ', await clickText('瀏覽', '.form button'))
await sleep(1200)
console.log('  picker host:', await ev(`document.querySelector('.dirpicker-host')?.textContent`), '| path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`), '| rows:', await ev(`document.querySelectorAll('.dirpicker-row').length`))
await shot('63-remote-dirpicker')
console.log(' ', await clickText('work', '.dirpicker-row'))
await sleep(900)
console.log('  picker path:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`), '| rows:', await ev(`document.querySelectorAll('.dirpicker-row').length`))
await shot('64-remote-dirpicker-work')
console.log(' ', await clickText('api-server', '.dirpicker-row'))
await sleep(900)
console.log(' ', await clickText('選擇此目錄'))
await sleep(500)
console.log('  form path:', await ev(`document.querySelectorAll('.form input[type=text]')[0]?.value`), '| label:', await ev(`document.querySelectorAll('.form input[type=text]')[1]?.value`))
console.log(' ', await clickText('新增', '.form-actions button'))
await sleep(900)
console.log('  projects:', await ev(`JSON.stringify([...document.querySelectorAll('.project-head')].map(h=>h.textContent))`))
await shot('65-remote-project')

console.log('== 3. 新增遠端 bot ==')
console.log(' ', await clickText('新增 Bot', '.sidebar-foot .disclosure'))
await sleep(350)
console.log('  project select:', await type('.form select', await ev(`[...document.querySelectorAll('.form select option')].find(o=>o.textContent.includes('api-server'))?.value`)))
await sleep(200)
console.log('  name:', await type('.form input[type=text]', 'api-claude', 0))
console.log(' ', await clickText('新增', '.form-actions button'))
await sleep(900)
console.log('  lamps:', await lamps())
console.log(' ', await clickInBotRow('api-claude', '啟動'))
await sleep(2200)
console.log('  lamps:', await lamps())
await shot('66-remote-bot-running')

console.log('== 4. 主機斷線 ==')
console.log(' ', await ev(`__amMock.hostDown('m4p'), 'hostDown m4p'`))
await sleep(700)
console.log('  lamps:', await lamps())
console.log('  composer lock:', await ev(`document.querySelector('.composer-lock')?.textContent`))
console.log('  badge class:', await ev(`document.querySelector('.main-title .host-badge')?.className`))
await shot('67-host-down')
console.log(' ', await clickText('主機', '.sidebar-foot .disclosure'))
await sleep(300)
console.log('  host err:', await ev(`document.querySelector('.host-err')?.textContent`))
await shot('68-host-down-panel')

console.log('== 5. 重連 ==')
console.log(' ', await clickText('重連', '.host-row .mini-btn'))
await sleep(1600)
console.log('  lamps:', await lamps())
console.log('  composer lock:', await ev(`document.querySelector('.composer-lock')?.textContent ?? '(none)'`))
await shot('69-host-reconnected')

console.log('== 6. 深色 / 900 寬 ==')
await dark(true); await sleep(400)
await shot('6a-dark')
await dark(false)
await metrics(900, 900); await sleep(500)
await shot('6b-narrow-900')
await metrics(1280, 900)

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)

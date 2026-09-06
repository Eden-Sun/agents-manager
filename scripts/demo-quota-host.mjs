// SPEC §12.7：header 的額度列跟著「現在在看哪一台主機」走。
// 走 mock backend（`VITE_MOCK=1 npx vite --port 5311`）：先拍本機 header，再建一台 m4p、
// 在上面開 project + bot，選進去之後 header 應該換成 m4p 的數字並掛上主機名牌。
// Usage: node scripts/demo-quota-host.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9344
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-quota-host', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
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
// 只截標題列：這個功能的全部變化都在那一條上，整頁截圖只會把它縮成一行。
const shot = async (name, h = 120) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1280, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const clickText = (t, sel = 'button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'MISSING '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const type = (sel, value, nth = 0) => ev(`(()=>{const el=document.querySelectorAll(${JSON.stringify(sel)})[${nth}];if(!el)return 'MISSING ${sel}';const d=Object.getOwnPropertyDescriptor(el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype,'value');d.set.call(el,${JSON.stringify(value)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return el.value})()`)
// `.bot-name` 裡除了名字還有 persona 記號與 agent 標題，所以用開頭比對而不是完全相等。
const selectBot = (name) => ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>(x.querySelector('.bot-name')?.textContent||'').trim().startsWith(${JSON.stringify(name)}));if(!r)return 'MISSING '+${JSON.stringify(name)};r.click();return 'selected '+${JSON.stringify(name)}})()`)
// 額度列現在讀的是哪一台、每個 gauge 的標籤與剩餘 %：截圖之外也留一份可 diff 的文字。
const strip = () => ev(`(()=>{const s=document.querySelector('.quota-strip');if(!s)return 'no strip';const host=s.querySelector('.quota-host')?.textContent ?? '(本機)';const gs=[...s.querySelectorAll('.quota-hp')].map(g=>(g.querySelector('.quota-identity')?.textContent||g.className.split(' ')[1])+' '+[...g.querySelectorAll('.quota-window')].map(w=>w.querySelector('.quota-window-name').textContent+':'+Math.round(parseFloat(w.querySelector('.quota-bar-fill').style.width))+'%').join('/'));return host+' → '+gs.join(' | ')})()`)

const submitForm = () => ev(`(()=>{const b=document.querySelector('.form .form-actions button[type=submit]');if(!b)return 'MISSING submit';if(b.disabled)return 'submit disabled';b.click();return 'submitted'})()`)
const clickProject = (label) => ev(`(()=>{const h=[...document.querySelectorAll('.project-head')].find(x=>x.textContent.includes(${JSON.stringify(label)}));if(!h)return 'MISSING project '+${JSON.stringify(label)};h.click();return 'selected project '+${JSON.stringify(label)}})()`)

// mock 一開機就有一個本機 project（agents-manager）和它的 bot，直接用。
console.log('== 1. 本機：預設就是 local，額度列不掛主機名牌 ==')
console.log(' ', await selectBot('am-claude'))
await sleep(600)
console.log('  strip:', await strip())
await shot('340-quota-host-local')

console.log('== 2. 新增遠端主機 m4p（環境設定 → 主機）==')
console.log(' ', await clickText('環境設定', '.sidebar-foot .disclosure'))
await sleep(400)
console.log('  name:', await type('.hosts-panel input[type=text]', 'm4p', 0))
console.log('  ssh :', await type('.hosts-panel input[type=text]', 'm4p@100.112.229.82', 1))
console.log(' ', await clickText('新增並連線', '.hosts-panel button'))
await sleep(2000)
console.log('  host rows:', await ev(`document.querySelectorAll('.host-row').length`), '| result:', await ev(`document.querySelector('.host-result')?.textContent`))
console.log(' ', await ev(`(()=>{const b=document.querySelector('.modal-close, .modal button[aria-label]');if(b){b.click();return 'closed modal'}document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));return 'esc'})()`))
await sleep(400)

console.log('== 3. 遠端 project + bot ==')
console.log(' ', await clickText('新增 Project', '.sidebar-foot-actions button'))
await sleep(400)
console.log('  host select:', await type('.form select', 'm4p'))
await sleep(300)
console.log('  path:', await type('.form input[type=text]', '/Users/m4p/work/api-server', 0))
console.log(' ', await submitForm())
await sleep(900)
// 先選起遠端 project：sidebar 的「新增 Bot」會把目前選到的 project 直接帶進表單。
console.log(' ', await clickProject('api-server'))
await sleep(400)
console.log(' ', await clickText('新增 Bot', '.sidebar-foot-actions button'))
await sleep(400)
console.log('  form project:', await ev(`document.querySelector('.modal-sub')?.textContent ?? document.querySelector('.modal h2, .modal-title')?.textContent`))
console.log('  name:', await type('.form input[type=text]', 'api-claude', 0))
console.log(' ', await submitForm())
await sleep(1300)

console.log('== 4. 點進遠端 bot：額度換成 m4p 的 ==')
console.log(' ', await selectBot('api-claude'))
await sleep(700)
console.log('  strip:', await strip())
console.log('  host badge:', await ev(`document.querySelector('.main-title .host-badge')?.textContent`))
await shot('341-quota-host-remote')

console.log('== 5. 展開額度 popover（標題寫哪一台）==')
console.log(' ', await clickText('', '.quota-open'))
await sleep(400)
console.log('  pop title:', await ev(`document.querySelector('.quota-pop-title')?.textContent`))
console.log('  pop rows:', await ev(`[...document.querySelectorAll('.quota-pop-row')].map(r=>r.querySelector('.quota-name')?.textContent).join(' | ')`))
await shot('342-quota-host-remote-pop', 420)
console.log(' ', await clickText('', '.quota-open'))
await sleep(300)

console.log('== 6. 切回本機 bot：數字換回來 ==')
console.log(' ', await selectBot('am-claude'))
await sleep(600)
console.log('  strip:', await strip())
await shot('343-quota-host-back-to-local')

console.log('== 7. 深色 / 窄視窗（收合成單一窗口時主機名牌還在）==')
console.log(' ', await selectBot('api-claude'))
await sleep(500)
await dark(true); await sleep(400)
await shot('344-quota-host-remote-dark')
await dark(false)
await metrics(1040, 900); await sleep(600)
console.log('  strip:', await strip())
await shot('345-quota-host-remote-narrow')
await metrics(1280, 900)

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)

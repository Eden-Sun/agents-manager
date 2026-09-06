// SPEC §15：cc0～cc6 由各主機自己的登入 shell（zshrc 的 alias）認出來，不必寫 config.toml。
// 走 mock backend（`VITE_MOCK=1 npx vite --port 5311`）：身份面板要分得出 config 與 shell 兩種
// 來源，Bot 設定的身份選項要選得到 shell 來的那個，而且同一個名字在遠端指到的是遠端的目錄。
// Usage: node scripts/demo-identity-shell.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9346
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-ident-shell', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2200)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png' }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
const clickText = (t, sel = 'button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'MISSING '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const type = (sel, value, nth = 0) => ev(`(()=>{const el=document.querySelectorAll(${JSON.stringify(sel)})[${nth}];if(!el)return 'MISSING ${sel}';const d=Object.getOwnPropertyDescriptor(el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype,'value');d.set.call(el,${JSON.stringify(value)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return el.value})()`)
const esc = () => ev(`(()=>{document.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));const b=document.querySelector('.modal button[aria-label]');if(b)b.click();return 'closed'})()`)
// 身份區塊在「環境設定」的下半段，截圖前先捲到它，不然只拍得到主機那一半。
const scrollToIdentities = () => ev(`(()=>{const e=document.querySelector('.identity-shell-block')??document.querySelector('.identities-panel');if(!e)return 'MISSING identities';e.scrollIntoView({block:'center'});return 'scrolled'})()`)
// 身份面板上看得到什麼：config 的列（可刪）與 shell 認來的列（唯讀）。
const identRows = () => ev(`JSON.stringify({
  config: [...document.querySelectorAll('.identities-panel .identity-row:not(.is-shell)')].map(r=>r.querySelector('.identity-name')?.textContent.trim()),
  shell: [...document.querySelectorAll('.identities-panel .identity-row.is-shell')].map(r=>r.querySelector('.identity-name')?.textContent.trim()+' → '+r.querySelector('.identity-detail')?.textContent.trim()),
})`)

console.log('== 1. 環境設定 → 身份：config 與 shell 兩區 ==')
console.log(' ', await clickText('環境設定', '.sidebar-foot .disclosure'))
await sleep(500)
console.log('  rows:', await identRows())
console.log('  foot note:', await ev(`document.querySelector('.sidebar-foot .disclosure-note')?.textContent`))
console.log(' ', await scrollToIdentities())
await shot('350-identities-shell-local')

console.log('== 2. 加一台 m4p：同一個 cc2 在那台指到別的目錄 ==')
console.log('  name:', await type('.hosts-panel input[type=text]', 'm4p', 0))
console.log('  ssh :', await type('.hosts-panel input[type=text]', 'm4p@100.112.229.82', 1))
console.log(' ', await clickText('新增並連線', '.hosts-panel button'))
await sleep(2000)
console.log('  rows:', await identRows())
console.log(' ', await scrollToIdentities())
await shot('351-identities-shell-two-hosts')
console.log(' ', await esc())
await sleep(400)

console.log('== 3. Bot 設定的身份選項：shell 來的也選得到 ==')
console.log(' ', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(r=>(r.querySelector('.bot-name')?.textContent||'').trim().startsWith('am-claude'));if(!r)return 'MISSING am-claude';r.click();const g=r.querySelector('.icon-btn.gear');if(!g)return 'no gear';g.click();return 'opened settings'})()`))
await sleep(700)
console.log('  identity options:', await ev(`[...document.querySelectorAll('.bs-body .opt-group[aria-label=identity] .opt')].map(b=>b.textContent.trim()).join(' | ')`))
console.log('  cc2 title:', await ev(`[...document.querySelectorAll('.bs-body .opt-group[aria-label=identity] .opt')].find(b=>b.textContent.trim()==='cc2')?.title`))
await shot('352-bot-settings-identity-options')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 400))
ws.close(); chrome.kill(); process.exit(0)

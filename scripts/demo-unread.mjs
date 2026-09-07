// 「已完成（未讀）」：回合跑完時人不在看（選的是別的 bot / 分頁在背景 / 視窗沒 focus），
// 側欄那一列就掛一顆 `!N`，分頁標題掛 `(N)`；點進去（且視窗在前景）才清掉，重整不會消失。
// Prereq: `cd web && VITE_MOCK=1 npx vite --port 5311 --strictPort`
// Usage: node scripts/demo-unread.mjs [http://127.0.0.1:5311/]
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.argv[2] ?? 'http://127.0.0.1:5311/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots/unread'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const PORT = 9357
const chrome = spawn(CHROME, ['--headless=new', `--remote-debugging-port=${PORT}`, '--disable-gpu', '--hide-scrollbars', '--no-first-run', '--user-data-dir=/tmp/am-cdp-unread', '--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p = {}) => { const i = ++id; ws.send(JSON.stringify({ id: i, method: m, params: p })); return new Promise((res, rej) => pending.set(i, { res, rej })) }
for (let i = 0; i < 80; i++) { try { const l = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json(); const p = l.find(t => t.type === 'page' && t.url.startsWith('http')); if (p) { ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 2, mobile: false })
await send('Page.navigate', { url: URL_BASE }); await sleep(2400)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name, h = 900) => { await sleep(350); const { data } = await send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 0, width: 1280, height: h, scale: 2 } }); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data, 'base64')); console.log('  saved', name) }
/** 側欄每一列的名字 + 未讀徽章。 */
const rows = () => ev(`JSON.stringify([...document.querySelectorAll('.bot-row')].map(r=>({name:r.querySelector('.bot-name-text,.bot-name,.name')?.textContent?.trim()??r.textContent.trim().slice(0,14),badge:r.querySelector('.unread-turns')?.textContent??null})))`)
const title = () => ev('document.title')
const store = () => ev(`JSON.stringify({marks:JSON.parse(localStorage.getItem('am.readMarks')||'{}'),unread:JSON.parse(localStorage.getItem('am.unread')||'{}')})`)

console.log('  document.hasFocus():', await ev('document.hasFocus()'), ' visibility:', await ev('document.visibilityState'))
console.log('== 選 bot #1、送一則訊息，回合還在跑的時候切到 bot #2 ==')
await ev(`document.querySelectorAll('.bot-row')[0].click()`)
await sleep(500)
// mock 的 bot 開機時是「離線」，離線的不會跑回合——先啟動它。
await ev(`[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === '啟動')?.click()`)
await sleep(2500)
console.log(' ', await ev(`(()=>{const t=document.querySelector('.composer textarea');if(!t||t.disabled)return 'composer disabled';const s=Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype,'value').set;s.call(t,'請寫一段長一點的回覆');t.dispatchEvent(new Event('input',{bubbles:true}));t.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));return 'sent'})()`))
await sleep(400)
await ev(`document.querySelectorAll('.bot-row')[1].click()`)
console.log('  切走了，等回合完成…')
await sleep(7000)
console.log('  rows:', await rows())
console.log('  title:', await title())
await shot('440-unread-badge')

console.log('== 收合專案：底下的未讀加總掛回標題 ==')
console.log(' ', await ev(`(()=>{const b=document.querySelector('.project-fold');if(!b)return 'MISSING .project-fold';b.click();return 'folded'})()`))
await sleep(500)
console.log('  專案標題:', await ev(`(()=>{const t=document.querySelector('.project-label-btn');return JSON.stringify({label:t?.querySelector('.project-label')?.textContent,badge:t?.querySelector('.unread-turns')?.textContent??null})})()`))
await shot('441-unread-project-folded')
console.log(' ', await ev(`(()=>{const b=document.querySelector('.project-fold')??document.querySelector('.project-folded');if(!b)return 'MISSING unfold';b.click();return 'unfolded'})()`))
await sleep(500)

console.log('== localStorage 帳本（跨重整就靠這個；mock 每次重整都換一組 bot id，')
console.log('   真正的重整驗證見 scripts/demo-unread-persist.mjs @ 5173） ==')
console.log(' ', await store())

console.log('== 點進那個 bot：清成已讀 ==')
console.log(' ', await ev(`(()=>{const r=[...document.querySelectorAll('.bot-row')].find(x=>x.querySelector('.unread-turns'));if(!r)return 'no unread row';r.click();return 'clicked '+r.textContent.trim().slice(0,12)})()`))
await sleep(900)
console.log('  rows:', await rows())
console.log('  title:', await title())
console.log('  storage:', await store())
await shot('443-unread-cleared')

console.log('--- console errors ---')
for (const e of events) if (e.method === 'Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0, 300))
ws.close(); chrome.kill(); process.exit(0)

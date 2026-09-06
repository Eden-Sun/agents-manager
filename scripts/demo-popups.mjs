// 新增 Project / 新增 Bot / 環境設定 as popups (mock: `VITE_MOCK=1 npx vite --port 5307`).
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.AM_URL ?? 'http://localhost:5307/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9343','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-popups','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9343/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const clickText = (t, sel='button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'missing '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const key = (k) => send('Input.dispatchKeyEvent',{type:'keyDown',key:k,code:k,windowsVirtualKeyCode:k==='Escape'?27:0}).then(()=>send('Input.dispatchKeyEvent',{type:'keyUp',key:k,code:k,windowsVirtualKeyCode:k==='Escape'?27:0}))
const modal = async (tag) => console.log(tag, '| title:', await ev(`document.querySelector('.modal-head strong')?.textContent ?? '(none)'`), '| sub:', await ev(`document.querySelector('.modal-sub')?.textContent ?? '-'`), '| sidebar list behind:', await ev(`!!document.querySelector('.sidebar-scroll')`), '| focus:', await ev(`document.activeElement?.tagName + '.' + (document.activeElement?.className||'')`))

console.log(await clickText('新增 Project')); await sleep(400)
await modal('project')
await shot('320-popup-project')

console.log(await clickText('瀏覽')); await sleep(1000)
console.log('picker in modal, list height:', await ev(`Math.round(document.querySelector('.dirpicker-list').getBoundingClientRect().height)`), '| footer visible:', await ev(`(()=>{const b=document.querySelector('.dirpicker-actions .primary').getBoundingClientRect();return b.bottom<=innerHeight&&b.top>=0})()`))
await shot('321-popup-project-picker')

// Esc inside the picker goes back to the form, not out of the popup
await key('Escape'); await sleep(300)
console.log('after Esc in picker | picker gone:', await ev(`!document.querySelector('.dirpicker')`), '| modal still open:', await ev(`!!document.querySelector('.modal')`))
// Esc again closes the popup
await key('Escape'); await sleep(300)
console.log('after Esc in form   | modal closed:', await ev(`!document.querySelector('.modal')`))

console.log(await clickText('新增 Bot')); await sleep(500)
await modal('bot')
await shot('322-popup-bot')
console.log(await ev(`document.querySelector('.modal-head .icon-btn').click() ?? 'closed via ✕'`)); await sleep(300)
console.log('modal closed:', await ev(`!document.querySelector('.modal')`))

console.log(await clickText('環境設定')); await sleep(500)
await modal('env')
console.log('panels:', await ev(`[...document.querySelectorAll('.modal-body .disclosure')].map(b=>b.textContent.trim().slice(0,12)).join(' | ')`))
await shot('323-popup-env')
// backdrop click closes
console.log(await ev(`document.querySelector('.modal-backdrop').dispatchEvent(new MouseEvent('mousedown',{bubbles:true})) ?? 'backdrop mousedown'`)); await sleep(300)
console.log('modal closed:', await ev(`!document.querySelector('.modal')`))

await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'dark'}]})
console.log(await clickText('環境設定')); await sleep(500)
await shot('324-popup-env-dark')

console.log('--- errors ---')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)

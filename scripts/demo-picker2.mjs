// Drives the rebuilt DirPicker against the mock transport (`VITE_MOCK=1 vite --port 5307`).
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.AM_URL ?? 'http://localhost:5307/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9341','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-picker2','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9341/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:860,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
// Clip to the sidebar: the picker lives there and a full-page shot buries it.
let CLIPH = 860
const shot = async (name) => { await sleep(350); const { data } = await send('Page.captureScreenshot',{format:'png',clip:{x:0,y:0,width:460,height:CLIPH,scale:2}}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const clickText = (t, sel='button') => ev(`(()=>{const e=[...document.querySelectorAll(${JSON.stringify(sel)})].find(b=>b.textContent.trim().includes(${JSON.stringify(t)}));if(!e)return 'missing '+${JSON.stringify(t)};e.click();return 'clicked '+${JSON.stringify(t)}})()`)
const key = (k) => ev(`(()=>{const el=document.activeElement;el.dispatchEvent(new KeyboardEvent('keydown',{key:${JSON.stringify(k)},bubbles:true}));return el.className||el.tagName})()`)
const state = async (tag) => console.log(tag, '| crumbs:', await ev(`[...document.querySelectorAll('.dirpicker-crumbs .crumb')].map(b=>b.textContent).join('>')`), '| rows:', await ev(`document.querySelectorAll('.dirpicker-row').length`), '| sel:', await ev(`document.querySelector('.dirpicker-row.sel .name')?.textContent ?? '-'`), '| btn:', await ev(`document.querySelector('.dirpicker-actions .primary')?.textContent`), '| target:', await ev(`document.querySelector('.dirpicker-cur')?.textContent`))

console.log(await clickText('新增 Project')); await sleep(400)
console.log(await clickText('瀏覽')); await sleep(900)
await state('home')
await shot('310-picker2-home')

// one click highlights, the button follows the highlight
console.log(await clickText('project', '.dirpicker-pick')); await sleep(200)
await state('selected-row')
await shot('311-picker2-selected-row')

// the row chevron walks in
console.log(await ev(`document.querySelectorAll('.dirpicker-enter')[0]?.click() ?? 'clicked chevron'`)); await sleep(900)
await state('after-chevron')

// filter narrows the level
console.log(await ev(`(()=>{const i=document.querySelector('.dirpicker-filter');const s=Object.getOwnPropertyDescriptor(window.HTMLInputElement.prototype,'value').set;s.call(i,'age');i.dispatchEvent(new Event('input',{bubbles:true}));return 'typed'})()`)); await sleep(300)
await state('filtered')
await shot('312-picker2-filter')

// keyboard: ArrowDown then Enter walks deeper
console.log(await ev(`document.querySelector('.dirpicker-filter').focus() ?? 'focused'`))
console.log('key ArrowDown ->', await key('ArrowDown')); await sleep(150)
await state('kbd')
await shot('313-picker2-keyboard')

// hidden toggle
console.log(await clickText('⌂')); await sleep(800)
console.log(await ev(`document.querySelector('.dirpicker-hidden input').click() ?? 'toggled'`)); await sleep(900)
await state('hidden-on')
await shot('314-picker2-hidden')

// pick a highlighted folder without walking in
console.log(await clickText('project', '.dirpicker-pick')); await sleep(200)
console.log(await clickText('選擇', '.dirpicker-actions .primary')); await sleep(500)
console.log('form path:', await ev(`document.querySelector('.form input[type=text]')?.value`), '| label:', await ev(`document.querySelectorAll('.form input[type=text]')[1]?.value`))
await shot('315-picker2-picked')

// dark theme pass (SPEC §3.2 swaps on prefers-color-scheme)
await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'dark'}]})
console.log(await clickText('瀏覽')); await sleep(900)
console.log(await clickText('project', '.dirpicker-pick')); await sleep(200)
await state('dark')
await shot('316-picker2-dark')

// a long level in a short window: the list scrolls inside itself, the footer stays put
await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value:'light'}]})
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:560,deviceScaleFactor:2,mobile:false})
console.log(await clickText('瀏覽')); await sleep(900)
console.log(await clickText('⌂')); await sleep(800)
console.log(await clickText('Downloads', '.dirpicker-enter') === 'missing Downloads' ? await ev(`[...document.querySelectorAll('.dirpicker-row')].find(r=>r.textContent.includes('Downloads'))?.querySelector('.dirpicker-enter').click() ?? 'entered Downloads'` ) : 'entered'); await sleep(900)
await state('long-level-short-window')
console.log('list scrollable:', await ev(`(()=>{const l=document.querySelector('.dirpicker-list');return l.scrollHeight>l.clientHeight})()`),
  '| footer visible:', await ev(`(()=>{const b=document.querySelector('.dirpicker-actions .primary').getBoundingClientRect();return b.bottom<=window.innerHeight&&b.top>=0})()`),
  '| x-scroll:', await ev(`(()=>{const l=document.querySelector('.dirpicker-list');return l.scrollWidth>l.clientWidth})()`))
CLIPH = 560
await shot('317-picker2-long-short-window')

console.log('--- errors ---')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,400))
ws.close(); chrome.kill(); process.exit(0)

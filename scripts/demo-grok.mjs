// Headless-Chrome walkthrough of the grok kind in MOCK mode (SPEC §12).
//   cd web && VITE_MOCK=1 npx vite --port 5186 &   then   node scripts/demo-grok.mjs
// Screenshots: docs/screenshots/130-*.png
import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = process.env.AM_URL ?? 'http://127.0.0.1:5186/'
const OUT = new URL('../docs/screenshots', import.meta.url).pathname
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9341','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-grok','--window-size=1280,860', URL_BASE], { stdio: 'ignore' })
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
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const setVal = (sel, v) => ev(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});if(!el)return 'no '+${JSON.stringify(sel)};const proto=el.tagName==='SELECT'?HTMLSelectElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,${JSON.stringify(v)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return 'set '+el.tagName})()`)

// 1. sidebar shows the mock am-grok bot with its kind tag
console.log('bots:', await ev(`[...document.querySelectorAll('.bot-name')].map(e=>e.textContent).join(', ')`))
console.log('kind tags:', await ev(`[...document.querySelectorAll('.kind-tag')].map(e=>e.className+':'+e.textContent).join(', ')`))
await shot('130-grok-kind-tag')

// 2. open the "+ bot" inline form on the first project and pick kind = grok
console.log(await ev(`(()=>{const h=document.querySelectorAll('.project-head')[0];const b=h.querySelector('.icon-btn.add');if(!b)return 'no + btn';b.click();return 'clicked +'})()`)); await sleep(400)
console.log(await setVal('.inline-form input[type=text]', 'am-grok-2'))
// the kind select is the one whose options include `grok` (the first <select> is Project)
console.log('kind select ->', await ev(`(()=>{const s=[...document.querySelectorAll('.inline-form select')];const k=s.find(x=>[...x.options].some(o=>o.value==='grok'));if(!k)return 'no kind select';const set=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;set.call(k,'grok');k.dispatchEvent(new Event('change',{bubbles:true}));return k.value})()`)); await sleep(300)
console.log('model options:', await ev(`(()=>{const s=[...document.querySelectorAll('.inline-form select')];const m=s.find(x=>[...x.options].some(o=>o.value==='grok-4.6'));return m?[...m.options].map(o=>o.textContent).join(' | '):'no grok model select'})()`))
console.log('auto-approve text:', await ev(`(()=>{const l=[...document.querySelectorAll('.inline-form label')].find(l=>l.textContent.includes('自動核准'));return l?l.textContent.trim():'none'})()`))
await shot('130-new-bot-grok-form')

// 3. pick grok-4.5 and submit
console.log('model ->', await ev(`(()=>{const s=[...document.querySelectorAll('.inline-form select')];const m=s.find(x=>[...x.options].some(o=>o.value==='grok-4.5'));const set=Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set;set.call(m,'grok-4.5');m.dispatchEvent(new Event('change',{bubbles:true}));return m.value})()`)); await sleep(200)
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.inline-form button')].find(b=>b.textContent.trim()==='新增');if(!b||b.disabled)return 'submit disabled';b.click();return 'submitted'})()`)); await sleep(800)
console.log('bots now:', await ev(`[...document.querySelectorAll('.bot-name')].map(e=>e.textContent).join(', ')`))
await shot('131-grok-bot-added')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)

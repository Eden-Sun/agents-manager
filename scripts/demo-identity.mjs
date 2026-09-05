import { spawn } from 'node:child_process'
import { writeFileSync } from 'node:fs'
const URL_BASE = 'http://127.0.0.1:5182/'
const OUT = '/Users/m1pro/project/agents-manager/docs/screenshots'
const CHROME = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome'
const chrome = spawn(CHROME, ['--headless=new','--remote-debugging-port=9340','--disable-gpu','--hide-scrollbars','--no-first-run','--user-data-dir=/tmp/am-cdp-id','--window-size=1280,900', URL_BASE], { stdio: 'ignore' })
const sleep = (ms) => new Promise(r => setTimeout(r, ms))
let ws, id = 0; const pending = new Map(); const events = []
const send = (m, p={}) => { const i = ++id; ws.send(JSON.stringify({id:i,method:m,params:p})); return new Promise((res,rej)=>pending.set(i,{res,rej})) }
for (let i=0;i<80;i++){ try { const l = await (await fetch('http://127.0.0.1:9340/json/list')).json(); const p = l.find(t=>t.type==='page'&&t.url.startsWith('http')); if(p){ ws = new WebSocket(p.webSocketDebuggerUrl); break } } catch {} await sleep(250) }
ws.onmessage = e => { const m = JSON.parse(e.data); if (m.id && pending.has(m.id)) { const p = pending.get(m.id); pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result) } else events.push(m) }
await new Promise(r => ws.onopen = r)
await send('Runtime.enable'); await send('Page.enable')
await send('Emulation.setDeviceMetricsOverride',{width:1280,height:900,deviceScaleFactor:2,mobile:false})
await send('Page.navigate', { url: URL_BASE }); await sleep(2500)
const ev = async (expr) => { const r = await send('Runtime.evaluate', { expression: expr, awaitPromise: true, returnByValue: true }); return r.exceptionDetails ? 'EXC: ' + JSON.stringify(r.exceptionDetails.exception?.description) : r.result?.value }
const shot = async (name) => { await sleep(300); const { data } = await send('Page.captureScreenshot',{format:'png'}); writeFileSync(`${OUT}/${name}.png`, Buffer.from(data,'base64')); console.log('saved', name) }
const setVal = (sel, v) => ev(`(()=>{const el=document.querySelector(${JSON.stringify(sel)});if(!el)return 'missing '+${JSON.stringify(sel)};const proto=el.tagName==='SELECT'?HTMLSelectElement.prototype:el.tagName==='TEXTAREA'?HTMLTextAreaElement.prototype:HTMLInputElement.prototype;Object.getOwnPropertyDescriptor(proto,'value').set.call(el,${JSON.stringify(v)});el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return 'set'})()`)
// 1) identities panel
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.disclosure')].find(b=>b.textContent.includes('身份'));b.click();return 'opened identities'})()`)); await sleep(400)
console.log('identities listed:', await ev(`[...document.querySelectorAll('.identity-row .identity-badge')].map(e=>e.textContent).join(', ')`))
await shot('80-identities-panel')
// 2) + bot on project with identity cc1
console.log(await ev(`(()=>{document.querySelector('.project-head .icon-btn.add').click();return 'clicked +'})()`)); await sleep(400)
console.log('identity options:', await ev(`[...document.querySelectorAll('.inline-form select')[2].options].map(o=>o.textContent).join(' | ')`))
console.log(await setVal('.inline-form input[type=text]', 'company-bot'))
console.log(await setVal('.inline-form select:nth-of-type(1)', 'claude'))
console.log(await ev(`(()=>{const sels=document.querySelectorAll('.inline-form select');const s=sels[2];Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype,'value').set.call(s,'cc1');s.dispatchEvent(new Event('change',{bubbles:true}));return 'identity='+s.value})()`))
await shot('81-new-bot-with-identity')
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.inline-form button')].find(b=>b.textContent.trim()==='新增');if(b.disabled)return 'disabled';b.click();return 'submitted'})()`)); await sleep(800)
console.log('bot rows with badge:', await ev(`[...document.querySelectorAll('.bot-row')].filter(r=>r.querySelector('.identity-badge')).map(r=>r.querySelector('.bot-name').textContent+'['+r.querySelector('.identity-badge').textContent+']').join(', ')`))
// 3) add a new identity
console.log(await setVal('.identities-panel form input[type=text]', 'cc2'))
console.log(await setVal('.identities-panel form textarea', 'CLAUDE_CONFIG_DIR=$HOME/.claude-personal'))
console.log(await ev(`(()=>{const b=[...document.querySelectorAll('.identities-panel form button')].find(b=>b.textContent.includes('新增身份'));if(b.disabled)return 'disabled';b.click();return 'added identity'})()`)); await sleep(600)
console.log('identities now:', await ev(`[...document.querySelectorAll('.identity-row .identity-badge')].map(e=>e.textContent).join(', ')`))
await shot('82-identity-added-and-bot-badge')
for (const e of events) if (e.method==='Runtime.exceptionThrown') console.log(JSON.stringify(e.params).slice(0,300))
ws.close(); chrome.kill(); process.exit(0)

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, writeFileSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';

const script = readFileSync(new URL('./chatgpt-consult.mjs', import.meta.url), 'utf8');
const pid = '01M1Y7BNVP843V9MFEDJ2KW9NQ';
const key = '1234567890abcdef1234567890abcdef';
const url = 'https://chatgpt.com/c/project-a';

function run(options = {}) {
  const dir = mkdtempSync(join(tmpdir(), 'ob-browser-test-'));
  try {
    const journal = join(dir, 'journal.json');
    if (options.progress) writeFileSync(journal, JSON.stringify(options.progress));
    const args = {project_id: pid, project_label: 'AM', question: 'review this', request_key: key,
      url: options.newProject ? null : url, journal, collect: !!options.collect};
    const prelude = `
      const actions=[];
      const opts=${JSON.stringify(options)};
      let currentUrl=${JSON.stringify(url)};
      const marker=${JSON.stringify(`[OB request=${key}]`)};
      const nodes=[
        {getAttribute:()=> 'user', innerText:marker+' question'},
        {getAttribute:()=> 'assistant', innerText:'BOUND_ANSWER'},
        {getAttribute:()=> 'user', innerText:'unrelated later question'},
        {getAttribute:()=> 'assistant', innerText:'WRONG_LATER_ANSWER'}
      ];
      for(const n of nodes)n.closest=()=>({querySelector:()=>!opts.partial});
      globalThis.document={querySelectorAll:()=>nodes,querySelector:()=>null};
      const page={label:'p2',goto:async u=>{actions.push(['goto',u]);},
        waitForSelector:async()=>{},snapshot:async()=>'',
        click:async s=>{actions.push(['click',s]);if(opts.failSend && s.includes('send-button'))throw new Error('send interrupted');},
        keyboard:{insertText:async s=>actions.push(['insert',s])},
        evaluate:async(fn,arg)=>fn.toString().includes('busy:')?{count:0,busy:!!opts.busy,draft:opts.draft||''}:fn(arg),
        waitForFunction:async()=>{},url:async()=>currentUrl};
      globalThis.taskSpace=async name=>({
        tabs:async()=>[{url:${JSON.stringify(url)},label:'p2'},{url:'https://chatgpt.com/',label:'p3'}],
        page:label=>{actions.push(['reuse',label]);return page;},
        newPage:async()=>{actions.push(['new']);return page;}
      });
      globalThis.CONSULT_ARGS=${JSON.stringify(args)};
      process.on('exit',()=>console.log('ACTIONS '+JSON.stringify(actions)));
    `;
    const entry = join(dir, 'run.mjs');
    writeFileSync(entry, prelude + '\n' + script);
    const proc = spawnSync(process.execPath, [entry], {encoding:'utf8'});
    const line = proc.stdout.split('\n').find(l=>l.startsWith('ACTIONS '));
    return {code:proc.status,error:proc.stderr,actions:JSON.parse(line.slice(8)),
      journal:existsSync(journal)?JSON.parse(readFileSync(journal,'utf8')):null};
  } finally { rmSync(dir, {recursive:true,force:true}); }
}

test('known project reuses exact conversation and pairs answer to request, not last answer', () => {
  const r=run();
  assert.equal(r.code,0,r.error);
  assert.deepEqual(r.actions[0],['reuse','p2']);
  assert.equal(r.actions.filter(a=>a[0]==='new').length,0);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
  assert.match(r.actions.find(a=>a[0]==='insert')[1],new RegExp(key));
});
test('new project never adopts another project home tab', () => {
  const r=run({newProject:true});
  assert.equal(r.code,0,r.error);
  assert.deepEqual(r.actions[0],['new']);
  assert.match(r.actions.find(a=>a[0]==='insert')[1],new RegExp(pid));
});
test('busy conversation and existing draft are not overwritten', () => {
  for(const opts of [{busy:true},{draft:'human draft'}]) {
    const r=run(opts);
    assert.notEqual(r.code,0);
    assert.equal(r.actions.filter(a=>a[0]==='insert'||a[0]==='click').length,0);
  }
});
test('send failure leaves durable uncertainty before click, never blind retry', () => {
  const r=run({failSend:true});
  assert.notEqual(r.code,0);
  assert.equal(r.journal.phase,'dispatching');
  const retry=run({progress:r.journal});
  assert.notEqual(retry.code,0);
  assert.equal(retry.actions.length,0);
});
test('collect only retrieves the bound answer and never sends another question', () => {
  const r=run({collect:true,progress:{phase:'sent',project_id:pid,url,before:0}});
  assert.equal(r.code,0,r.error);
  assert.equal(r.actions.filter(a=>a[0]==='insert'||a[0]==='click').length,0);
  assert.equal(r.journal.answer,'BOUND_ANSWER');
});
test('completed browser receipt is replayed without any page operations', () => {
  const r=run({progress:{phase:'done',project_id:pid,url,answer:'saved'}});
  assert.equal(r.code,0,r.error);
  assert.equal(r.actions.length,0);
});
test('corrupt or wrong-project journal is not silently ignored', () => {
  const r=run({progress:{phase:'sent',project_id:'01M1Y75JS1G8PZ4EHF6Y98AFB1',url,before:0}});
  assert.notEqual(r.code,0);
  assert.equal(r.actions.length,0);
});
test('a partial response without its completed-turn controls is not accepted', () => {
  const r=run({partial:true});
  assert.notEqual(r.code,0);
  assert.notEqual(r.journal.phase,'done');
});

import test from 'node:test'
import assert from 'node:assert/strict'
import { checkEnvText, emptyEnvKeys, rustTrim, envDisplayText, parseEnvText } from './identityEnv'

test('一行打錯（少了 =、名字不合法、重複）不能靜靜被丟掉：丟掉的話身份的 env 變成空的＝悄悄用了預設帳號', () => {
  const r = checkEnvText('CLAUDE_CONFIG_DIR ~/.claude-cc1\nBAD NAME=1\n9LIVES=x\nA=1\nA=2\n=novalue')
  assert.deepEqual(r.env, { A: '2' }, '還是把合法的解出來（給預覽用）')
  assert.equal(r.errors.length, 5, r.errors.join(' | '))
  assert.ok(r.errors.some((e) => e.includes('第 1 行') && e.includes('=')), '少了 =：指出第幾行')
  assert.ok(r.errors.some((e) => e.includes('第 2 行') && e.includes('BAD NAME')), '名字不合法')
  assert.ok(r.errors.some((e) => e.includes('第 3 行') && e.includes('9LIVES')), '數字開頭')
  assert.ok(r.errors.some((e) => e.includes('第 5 行') && e.includes('A')), '重複的 key')
  assert.ok(r.errors.some((e) => e.includes('第 6 行')), '空的名字')
})

test('合法的 env 沒有任何錯誤；空行與 # 註解照舊略過；值裡的 = 保留', () => {
  const r = checkEnvText('# 註解\n\nCLAUDE_CONFIG_DIR=$HOME/.claude-cc1\nURL=https://x/?a=b')
  assert.deepEqual(r.errors, [])
  assert.deepEqual(r.env, { CLAUDE_CONFIG_DIR: '$HOME/.claude-cc1', URL: 'https://x/?a=b' })
  assert.deepEqual(parseEnvText('A=1'), { A: '1' }, '舊的 parseEnvText 行為不變')
})

test('畫面上顯示 env 時，key／token／secret／password 類的值遮起來（列表與 tooltip 都會被截圖、被旁邊的人看到）', () => {
  const shown = envDisplayText({
    CLAUDE_CONFIG_DIR: '/home/u/.claude-cc1',
    ANTHROPIC_API_KEY: 'sk-ant-api03-abcdefghijklmnop',
    GH_TOKEN: 'ghp_secretsecret',
    DB_PASSWORD: 'hunter2',
    CLIENT_SECRET: 's3cr3t',
  })
  assert.ok(shown.includes('CLAUDE_CONFIG_DIR=/home/u/.claude-cc1'), '一般的值照顯示')
  for (const secret of ['sk-ant-api03-abcdefghijklmnop', 'ghp_secretsecret', 'hunter2', 's3cr3t']) {
    assert.ok(!shown.includes(secret), `不能出現 ${secret}：${shown}`)
  }
  assert.ok(shown.includes('ANTHROPIC_API_KEY=') && shown.includes('••'), '還看得出有這個 key、而且被遮了')
})

test('key 跟 Object.prototype 上的名字同名（constructor、__proto__）：不能被誤判成重複，也不能靜靜消失', () => {
  const ctor = checkEnvText('constructor=1')
  assert.deepEqual(ctor.errors, [], '沒有前面那一行，就不是重複')
  assert.deepEqual(Object.entries(ctor.env), [['constructor', '1']])

  const proto = checkEnvText('__proto__=x')
  assert.deepEqual(proto.errors, [])
  assert.deepEqual(Object.entries(proto.env), [['__proto__', 'x']], '不能被 __proto__ 的 setter 吃掉')
})

test('值是空的（例如把 `$HOME/.claude-` 的尾巴刪光）不能靜靜存下來：身份的 CLAUDE_CONFIG_DIR 變空，就悄悄用了預設帳號', () => {
  const r = checkEnvText('CLAUDE_CONFIG_DIR=\nA=1\nB=   ')
  assert.ok(r.errors.some((e) => e.includes('第 1 行') && e.includes('CLAUDE_CONFIG_DIR')), r.errors.join(' | '))
  assert.ok(r.errors.some((e) => e.includes('第 3 行') && e.includes('B')), r.errors.join(' | '))
  assert.deepEqual(r.env, { A: '1' }, '空值的那幾行不進 env，也不會讓送出被靜靜放行')
})

test('設定裡既有的空值 env：空字串與全空白的 key 都點名，有值的與沒有 env 的身份不標', () => {
  assert.deepEqual(emptyEnvKeys({ CLAUDE_CONFIG_DIR: '', A: '1', B: '  ' }), ['CLAUDE_CONFIG_DIR', 'B'])
  assert.deepEqual(emptyEnvKeys({ A: '1' }), [])
  assert.deepEqual(emptyEnvKeys({}), [])
})

test('判空的空白集合跟 daemon（Rust char::is_whitespace）一致：U+0085、U+3000 算空白，U+FEFF 不算', () => {
  assert.deepEqual(emptyEnvKeys({ A: '\u0085', B: '\u3000', C: ' \t\u00a0\u2003 ', D: '\ufeff', E: 'x' }), ['A', 'B', 'C'])
  assert.equal(rustTrim('\ufeffx\u0085'), '\ufeffx')
  const r = checkEnvText('A=\u0085\nB=\u3000\nC=\ufeff')
  assert.equal(r.errors.length, 2, r.errors.join(' | '))
  assert.ok(r.errors[0].includes('第 1 行') && r.errors[1].includes('第 2 行'))
  assert.deepEqual(r.env, { C: '\ufeff' })
})

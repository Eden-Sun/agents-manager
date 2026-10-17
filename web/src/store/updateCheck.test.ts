import test from 'node:test'
import assert from 'node:assert/strict'
import type { HerdrVersion, ToolMap } from '../api/types.ts'
import type { IntegrationHost } from '../api/integrationUpdates.ts'
import type { UpstreamItem } from './upstreamUpdate.ts'
import { buildHostCheck, cleanVersion, summaryText, updatableCount, versionLess, type CheckRow, type CheckStatus, type HostCheck } from './updateCheck.ts'

const tool = (installed: boolean, version: string | null = null) => ({ installed, path: installed ? '/bin/x' : null, version, logged_in: null })
const tools = (over: Partial<Record<'claude' | 'codex' | 'grok' | 'agy', ReturnType<typeof tool>>> = {}) =>
  ({ claude: tool(true, '2.1.300 (Claude Code)'), codex: tool(true, 'codex-cli 0.157.0'), grok: tool(false), agy: tool(true, '1.2.0'), ...over }) as unknown as ToolMap
const herdr = (cli: string | null, server: string | null): HerdrVersion => ({ server_version: server, protocol: 22, protocol_supported: true, cli_version: cli, mismatch: cli !== server })
const item = (kind: string, latest: string | null, hosts: UpstreamItem['hosts'], target = latest): UpstreamItem => ({
  kind,
  latest,
  target,
  hasUpdate: hosts.some((h) => h.behind),
  hosts,
  notify: null,
  text: null,
})
const up = (installed: string | null, behind: boolean, error: string | null = null) => ({ host: 'local', installedVersion: installed, error, behind })
const integ = (items: IntegrationHost['items'], error: string | null = null): IntegrationHost => ({ host: 'local', checked_at: 't', error, items })
const irow = (name: string, state: IntegrationHost['items'][number]['state'], a: number | null, b: number | null) => ({
  name,
  state,
  installed_version: a,
  required_version: b,
  path: null,
  detail: state === 'unknown' ? 'wedged' : null,
})

test('cleanVersion／versionLess：從 CLI 原文取版本號、逐段比較', () => {
  assert.equal(cleanVersion('herdr 0.9.5'), '0.9.5')
  assert.equal(cleanVersion('2.1.300 (Claude Code)'), '2.1.300')
  assert.equal(cleanVersion('no version'), null)
  assert.equal(versionLess('2.1.9', '2.1.10'), true, '不是字串比較')
  assert.equal(versionLess('0.9.5', '0.9.5'), false)
  assert.equal(versionLess(null, '0.9.5'), null)
})

test('每台主機的列：herdr CLI／server、四家 CLI、四家 integration，狀態各自判斷', () => {
  const c = buildHostCheck({
    host: 'local',
    tools: tools(),
    herdr: herdr('herdr 0.9.4', '0.9.4'),
    upstream: {
      herdr: item('herdr', '0.9.5', [up('herdr 0.9.4', true)]),
      claude: item('claude', '2.1.300', [up('2.1.300', false)]),
      codex: item('codex', '0.158.0', [up('0.157.0', true)]),
    },
    integration: integ([
      irow('claude', 'outdated', 8, 11),
      irow('codex', 'current', 10, 10),
      irow('antigravity-cli', 'not_installed', null, null),
      irow('grok', 'unknown', null, null),
    ]),
    herdrBlocked: null,
  })
  const by = Object.fromEntries(c.rows.map((r) => [r.id, r]))
  assert.deepEqual(c.rows.map((r) => r.id), [
    'local:herdr-cli', 'local:herdr-server', 'local:cli-claude', 'local:cli-codex', 'local:cli-grok', 'local:cli-agy',
    'local:integration-claude', 'local:integration-codex', 'local:integration-antigravity-cli', 'local:integration-grok',
  ])
  assert.equal(by['local:herdr-cli'].status, 'update')
  assert.deepEqual(by['local:herdr-cli'].action, { kind: 'herdr', target: '0.9.5' })
  assert.equal(by['local:herdr-server'].status, 'update')
  assert.equal(by['local:herdr-server'].action, null, 'server 那列不另外給按鈕（更新 CLI 那列會一併重啟）')
  assert.equal(by['local:cli-claude'].status, 'latest')
  assert.equal(by['local:cli-claude'].current, '2.1.300')
  assert.deepEqual(by['local:cli-codex'].action, { kind: 'cli', cliKind: 'codex', target: '0.158.0' })
  assert.equal(by['local:cli-grok'].status, 'missing')
  assert.equal(by['local:cli-agy'].status, 'unknown', 'agy 沒有上游版本來源')
  assert.equal(by['local:cli-agy'].action, null)
  assert.deepEqual([by['local:integration-claude'].current, by['local:integration-claude'].latest, by['local:integration-claude'].status], ['v8', 'v11', 'update'])
  assert.deepEqual(by['local:integration-claude'].action, { kind: 'integration', name: 'claude' })
  assert.equal(by['local:integration-codex'].status, 'latest')
  assert.equal(by['local:integration-antigravity-cli'].status, 'missing')
  assert.equal(by['local:integration-antigravity-cli'].action, null, '沒裝的只顯示，不給安裝按鈕')
  assert.equal(by['local:integration-grok'].status, 'unknown')
  assert.equal(by['local:integration-grok'].note, 'wedged')
  assert.equal(updatableCount(c.rows), 4, 'herdr CLI、herdr server、codex CLI、claude integration')
})

test('遠端 herdr 不給按鈕、講原因；查不到上游、讀不到版本都是「查不到」而不是「最新」', () => {
  const c = buildHostCheck({
    host: 'm4p',
    tools: tools(),
    herdr: herdr('herdr 0.9.4', '0.9.4'),
    upstream: {
      herdr: item('herdr', '0.9.5', [{ ...up('herdr 0.9.4', true), host: 'm4p' }]),
      claude: item('claude', null, []),
      codex: item('codex', '0.158.0', [{ ...up(null, false, 'ssh timeout'), host: 'm4p' }]),
    },
    integration: null,
    herdrBlocked: '遠端主機：這版只支援更新本機的 herdr，請到那台手動更新',
  })
  const by = Object.fromEntries(c.rows.map((r) => [r.id, r]))
  assert.equal(by['m4p:herdr-cli'].status, 'update')
  assert.equal(by['m4p:herdr-cli'].action, null)
  assert.match(by['m4p:herdr-cli'].note ?? '', /遠端主機/)
  assert.equal(by['m4p:cli-claude'].status, 'unknown')
  assert.equal(by['m4p:cli-codex'].status, 'unknown')
  assert.match(by['m4p:cli-codex'].note ?? '', /ssh timeout/)
  assert.equal(c.integrationPending, true)
  assert.equal(c.rows.some((r) => r.group === 'integration'), false)
})

test('herdr 版本：讀到了但 CLI 版本是空的才是未安裝；沒讀到（undefined）是查不到；沒有上游快照時 CLI 也是查不到', () => {
  const empty = buildHostCheck({ host: 'x', tools: tools(), herdr: herdr(null, null), upstream: {}, integration: integ([]), herdrBlocked: null })
  assert.equal(empty.rows[0].status, 'missing')
  assert.equal(empty.rows[1].status, 'unknown')
  const notRead = buildHostCheck({ host: 'x', tools: tools(), herdr: undefined, upstream: {}, integration: integ([]), herdrBlocked: null })
  assert.equal(notRead.rows[0].status, 'unknown', '還沒讀到 herdr 不是未安裝')
  assert.equal(notRead.rows[0].note, '這台還沒讀到 herdr 版本')
  assert.ok(notRead.rows.filter((r) => r.group === 'herdr').every((r) => r.status === 'unknown'), 'herdr 兩列都是查不到')
  const noUp = buildHostCheck({ host: 'x', tools: tools(), herdr: herdr('herdr 0.9.5', '0.9.5'), upstream: {}, integration: integ([]), herdrBlocked: null })
  assert.equal(noUp.rows[0].status, 'unknown')
  assert.equal(noUp.rows[0].latest, null)
})

test('工具清單還沒讀到（tools undefined）：四家 CLI 都是查不到，不是未安裝', () => {
  const check = buildHostCheck({ host: 'x', tools: undefined, herdr: herdr('herdr 0.9.5', '0.9.5'), upstream: {}, integration: integ([]), herdrBlocked: null })
  for (const r of check.rows.filter((x) => x.group === 'cli')) {
    assert.equal(r.status, 'unknown', r.id)
    assert.equal(r.note, '這台還沒讀到工具清單')
  }
  const text = summaryText(check)
  assert.doesNotMatch(text, /未安裝/)
  assert.match(text, /查不到/)
})

test('工具清單讀到了、installed 為假：才是未安裝', () => {
  const check = buildHostCheck({ host: 'x', tools: tools({ grok: tool(false) }), herdr: herdr('herdr 0.9.5', '0.9.5'), upstream: {}, integration: integ([]), herdrBlocked: null })
  assert.equal(check.rows.find((r) => r.id === 'x:cli-grok')!.status, 'missing')
})

// #1813：表頭的一句話只有「真的全部確認是最新」才說「都是最新」；查不到、未安裝、integration 沒讀到都要寫出來。
const srow = (status: CheckStatus): CheckRow => ({ id: status, group: 'cli', label: status, current: null, latest: null, status, note: null, action: null })
const shost = (statuses: CheckStatus[], over: Partial<HostCheck> = {}): HostCheck => ({ host: 'local', integrationError: null, integrationPending: false, rows: statuses.map(srow), ...over })

test('summaryText：全部確認是最新 → 都是最新', () => {
  assert.equal(summaryText(shost(['latest', 'latest', 'latest'])), '都是最新')
})

test('summaryText：有可更新 → 寫出可更新數；其餘照數量寫，不把未確認的算成最新', () => {
  assert.equal(summaryText(shost(['update', 'latest', 'latest', 'latest'])), '1 項可更新、3 項最新')
  assert.equal(summaryText(shost(['update', 'update', 'missing'])), '2 項可更新、1 項未安裝')
})

test('summaryText：未安裝或查不到的不算最新（即使其餘都最新）', () => {
  assert.equal(summaryText(shost(['latest', 'latest', 'latest', 'missing', 'unknown'])), '3 項最新、1 項未安裝、1 項查不到')
  assert.equal(summaryText(shost(['missing', 'missing', 'missing', 'missing', 'missing', 'missing'])), '6 項未安裝')
  assert.equal(summaryText(shost(['unknown', 'unknown', 'unknown'])), '3 項查不到')
  assert.equal(summaryText(shost(['latest', 'unknown'])), '1 項最新、1 項查不到')
})

test('summaryText：loading（integration 還沒讀到）不說都是最新；讀失敗也要講', () => {
  assert.equal(summaryText(shost(['latest', 'latest'], { integrationPending: true })), '2 項最新、integration 還沒讀到')
  assert.equal(summaryText(shost(['latest', 'update'], { integrationPending: true })), '1 項可更新、1 項最新、integration 還沒讀到')
  assert.equal(summaryText(shost(['latest'], { integrationError: 'boom' })), '1 項最新、integration 讀取失敗')
})

test('summaryText 接在 buildHostCheck 後：沒有上游、讀不到版本的主機不會顯示都是最新', () => {
  const check = buildHostCheck({
    host: 'local',
    tools: tools({ claude: tool(false), codex: tool(false), grok: tool(false), agy: tool(false) }),
    herdr: undefined,
    upstream: {},
    integration: integ([irow('claude', 'unknown', null, null)]),
    herdrBlocked: null,
  })
  const text = summaryText(check)
  assert.doesNotMatch(text, /都是最新/)
  assert.match(text, /查不到/)
})

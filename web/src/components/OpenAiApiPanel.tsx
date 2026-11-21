import { useCallback, useEffect, useState } from 'react'
import { fetchClaudeApiLedger, type ClaudeApiLedger } from '../api/claudeApi'
import { useT } from '../lib/i18n'
import { EnvSubTabs, type EnvSubTabItem } from './EnvSubTabs'
import { OpenAiApiSettings } from './OpenAiApiSettings'
import { translationModelName } from './messageTranslationConstants'
import './claudeApiPanel.css'

const nf = new Intl.NumberFormat('en-US')

function OpenAiLedger() {
  const t = useT()
  const [ledger, setLedger] = useState<ClaudeApiLedger | null | undefined>(undefined)
  const load = useCallback(() => {
    void fetchClaudeApiLedger(8, 'openai')
      .then(setLedger)
      .catch(() => setLedger(null))
  }, [])
  useEffect(load, [load])
  return (
    <div className="claude-api-ledger" data-testid="openai-api-ledger">
      <h3 className="page-title">
        {t('OpenAI 用量帳本')}{' '}
        <button type="button" className="btn" onClick={load}>
          {t('重新整理')}
        </button>
      </h3>
      <p className="hint">{t('只列翻譯等 daemon 內部一次性工作的 OpenAI 呼叫（存於記憶體，daemon 重啟會清空）；OpenAI bot 的花費記在資料庫，累計見「設定」。')}</p>
      {ledger === undefined ? (
        <p className="hint">{t('讀取中…')}</p>
      ) : ledger === null ? (
        <p className="hint">{t('讀不到帳本。')}</p>
      ) : ledger.rows.length === 0 ? (
        <p className="hint" data-testid="openai-api-empty-ledger">
          {t('還沒有 OpenAI 的呼叫記錄。')}
        </p>
      ) : (
        <>
          <p className="hint" data-testid="openai-api-totals">
            {t('自 daemon 啟動起共 {calls} 次（成功 {ok}、失敗 {errors}），輸入 {input}、輸出 {output} tokens。', {
              calls: ledger.totals.calls,
              ok: ledger.totals.ok,
              errors: ledger.totals.errors,
              input: nf.format(ledger.totals.input_tokens),
              output: nf.format(ledger.totals.output_tokens),
            })}
          </p>
          <div className="claude-api-ledger-scroll">
            <table className="as-cards">
              <thead>
                <tr>
                  <th>{t('時間')}</th>
                  <th>{t('模型')}</th>
                  <th>{t('輸入')}</th>
                  <th>{t('輸出')}</th>
                  <th>{t('結果')}</th>
                </tr>
              </thead>
              <tbody>
                {ledger.rows.map((r, i) => (
                  <tr key={`${r.at}-${i}`}>
                    <td className="num" data-label={t('時間')}>{r.at.slice(5, 19).replace('T', ' ')}</td>
                    <td data-label={t('模型')} title={r.model}>{translationModelName(r.model)}</td>
                    <td className="num" data-label={t('輸入')}>{nf.format(r.input_tokens)}</td>
                    <td className="num" data-label={t('輸出')}>{nf.format(r.output_tokens)}</td>
                    <td data-label={t('結果')} className={r.ok ? 'ok' : 'err'}>{r.ok ? t('成功') : (r.error_kind ?? t('失敗'))}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </div>
  )
}

export function OpenAiApiPanel() {
  const t = useT()
  const [tab, setTab] = useState<'form' | 'book'>('form')
  const items: EnvSubTabItem<'form' | 'book'>[] = [
    { id: 'form', label: t('設定'), panelId: 'openai-form', content: <OpenAiApiSettings /> },
    { id: 'book', label: t('帳本'), panelId: 'openai-book', content: <OpenAiLedger /> },
  ]
  return <EnvSubTabs<'form' | 'book'> ariaLabel={t('OpenAI API 分類')} value={tab} onChange={setTab} items={items} />
}

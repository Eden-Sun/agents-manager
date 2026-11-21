import { useSyncExternalStore, type KeyboardEvent } from 'react'
import { getLangPref, msg, setLangPref, subscribeLang, useT, type LangPref } from '../lib/i18n'
import './kindTag.css'

const OPTIONS: { pref: LangPref; label: string }[] = [
  { pref: 'system', label: msg('跟隨瀏覽器') },
  { pref: 'zh-TW', label: '繁體中文' },
  { pref: 'en', label: 'English' },
]

export function LanguageToggle() {
  const t = useT()
  const pref = useSyncExternalStore(subscribeLang, getLangPref, getLangPref)
  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    const step = e.key === 'ArrowRight' || e.key === 'ArrowDown' ? 1 : e.key === 'ArrowLeft' || e.key === 'ArrowUp' ? -1 : 0
    if (!step) return
    e.preventDefault()
    const i = OPTIONS.findIndex((o) => o.pref === pref)
    const next = OPTIONS[(i + step + OPTIONS.length) % OPTIONS.length]
    setLangPref(next.pref)
    e.currentTarget.querySelector<HTMLElement>(`[data-pref="${next.pref}"]`)?.focus()
  }
  return (
    <div className="kind-display lang-toggle" role="radiogroup" aria-label={t('介面語言')} onKeyDown={onKeyDown}>
      <span className="kind-display-label">{t('語言')}</span>
      <div className="tabs small">
        {OPTIONS.map((o) => (
          <button key={o.pref} type="button" className="tab" role="radio" data-pref={o.pref} aria-checked={pref === o.pref} tabIndex={pref === o.pref ? 0 : -1} onClick={() => setLangPref(o.pref)}>
            {o.pref === 'system' ? t.tr(o.label) : o.label}
          </button>
        ))}
      </div>
    </div>
  )
}

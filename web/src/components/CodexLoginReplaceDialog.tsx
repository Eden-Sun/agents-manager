import { useStore } from '../store/store'
import { useT } from '../lib/i18n'
import { ConfirmDialog } from './ConfirmDialog'

export function CodexLoginReplaceDialog() {
  const t = useT()
  const ask = useStore((s) => s.codexLoginAsk)
  const answer = useStore((s) => s.answerCodexLoginAsk)
  return (
    <ConfirmDialog
      open={ask !== null}
      title={t('重新登入 codex？')}
      body={t('開始後這個帳號原本的登入會立即失效，用這個身份的 codex bot 要等你完成登入才能繼續。')}
      confirmLabel={t('開始登入')}
      cancelLabel={t('先不要')}
      danger
      onConfirm={() => answer(true)}
      onCancel={() => answer(false)}
    />
  )
}

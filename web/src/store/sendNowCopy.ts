import { t } from '../lib/i18n'

export interface SendNowButton {
  label: string
  title: string
}

export function sendNowButton(kind: string | null | undefined, pending: string): SendNowButton | null {
  const chars = [...pending]
  const preview = `${chars.slice(0, 40).join('')}${chars.length > 40 ? '…' : ''}`
  if (kind === 'claude') {
    return { label: t('插隊'), title: t('打斷目前這一輪，改送這一句（需要 claude 2.1.275 以上）：{preview}', { preview }) }
  }
  if (kind === 'codex') {
    return {
      label: t('插入'),
      title: t('把這一句插進正在跑的這一輪：不打斷、不開新回合，codex 邊做邊看到（需要 codex 0.159 以上，且 daemon 設定 [codex] instant_interrupt 與 codex 自己的 instant_interrupt 都開；不合資格時會告訴你原因）：{preview}', { preview }),
    }
  }
  return null
}

export interface SendNowNotice {
  level: 'info' | 'error'
  text: string
}

export function sendNowNotice(sendNow: string | null | undefined, kind: string | null | undefined): SendNowNotice | null {
  if (!sendNow || sendNow === 'interrupted' || sendNow === 'idle') return null
  if (sendNow === 'steered') {
    return {
      level: 'info',
      text: t('已插入進行中的回合：codex 沒有被打斷、也沒有開新回合，這一句併進它正在跑的這一輪（對話裡標「補充」）。'),
    }
  }
  const isCodex = kind === 'codex'
  switch (sendNow) {
    case 'send_now_unsupported_kind':
      return {
        level: 'info',
        text: isCodex
          ? t('沒有插隊：codex 的插入還沒開（daemon 設定 [codex] instant_interrupt 預設關），訊息照一般方式送出。')
          : t('沒有插隊：這種 bot 沒有插隊功能（只有 claude 與開了 instant_interrupt 的 codex），訊息照一般方式送出。'),
      }
    case 'send_now_cli_too_old':
      return { level: 'info', text: t('沒有插隊：這顆 bot 跑的 claude 比 2.1.275 舊，還沒有 send-now 鍵（重啟套用新版後才能插隊），訊息照一般方式送出。') }
    case 'send_now_version_unknown':
      return { level: 'info', text: t('沒有插隊：還不知道這顆 bot 跑的 claude 版本（statusLine 尚未回報），不賭那顆鍵，訊息照一般方式送出。') }
    case 'send_now_codex_too_old':
      return { level: 'info', text: t('沒有插入：這顆 bot 跑的 codex 比 0.159.0 舊，沒有 instant_interrupt（重啟套用新版後才能插入），訊息照一般方式送出。') }
    case 'send_now_codex_version_unknown':
      return { level: 'info', text: t('沒有插入：還不知道這顆 bot 跑的 codex 版本（畫面上的版本行尚未讀到），不賭 instant_interrupt，訊息照一般方式送出。') }
    default:
      return { level: 'info', text: t('沒有插隊（{reason}），訊息照一般方式送出。', { reason: sendNow }) }
  }
}

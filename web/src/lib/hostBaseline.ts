import type { HostBaseline } from '../api/types'

export type HostBaselineLevel = 'ok' | 'warn' | 'critical' | 'unknown'

export interface HostBaselineView {
  text: string
  level: HostBaselineLevel
  /** tooltip／說明；沒有就不顯示 */
  hint: string
  /** 嚴重的排前面；沒有就是空陣列 */
  items: { id: string; severity: 'critical' | 'warn'; message: string }[]
}

/**
 * hosts 面板的工作環境一致性行（SPEC §16.7，只讀）。
 * 沒量過、或探測沒跑完 → 「未知」，絕不當成全缺（一次 ssh 逾時不能在每台主機上喊一排缺漏）。
 */
export function hostBaselineView(b: HostBaseline | null): HostBaselineView {
  if (!b) return { text: '一致性：尚未檢查', level: 'unknown', hint: '還沒量過這台主機（剛連上、或 daemon 是舊版）', items: [] }
  if (b.issues === null) {
    return { text: '一致性：未知', level: 'unknown', hint: '上一趟探測沒跑完（逾時或被截斷），這不代表缺東西；下一次偵測會再量', items: [] }
  }
  if (b.issues.length === 0) return { text: '一致性：與基準一致', level: 'ok', hint: '', items: [] }
  const items = [...b.issues].sort((a, c) => Number(c.severity === 'critical') - Number(a.severity === 'critical'))
  const critical = items.filter((i) => i.severity === 'critical').length
  return {
    text: critical > 0 ? `一致性：${items.length} 項不一致（${critical} 項嚴重）` : `一致性：${items.length} 項提醒`,
    level: critical > 0 ? 'critical' : 'warn',
    hint: '',
    items,
  }
}

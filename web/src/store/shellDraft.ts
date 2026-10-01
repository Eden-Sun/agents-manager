import type { DraftKey } from './store'

/**
 * host shell 面板的指令草稿 key（#758）：跟對話輸入框同一套 daemon 同步草稿，換裝置／分頁看得到同一行。
 * 格式與 daemon `drafts::shell_key` 相同；pane id 只在單一主機內唯一，所以兩段都要帶。
 */
export const shellDraftKey = (host: string, paneId: string): DraftKey => `shell:${host}/${paneId}`

import type { DraftKey } from './store'

export const shellDraftKey = (host: string, paneId: string): DraftKey => `shell:${host}/${paneId}`

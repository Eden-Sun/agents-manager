import type { ShareClient } from '../share/shareApi'

export const shareFileStubs = {
  async upload() {
    return { id: 'a', name: 'a' }
  },
  async files() {
    return []
  },
  fileUrl: () => '/s/t/api/files/a',
  previewUrl: () => '/s/t/api/files/a?inline=1',
} satisfies Pick<ShareClient, 'upload' | 'files' | 'fileUrl' | 'previewUrl'>

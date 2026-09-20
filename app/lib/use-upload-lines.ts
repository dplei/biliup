import useSWR from 'swr'

import { fetcher } from './api-streamer'

/** `/v1/upload-lines`：B 站当前公布的 upcdn key；索引拉不到时 `degraded` 为 true，`lines` 是本地兜底。 */
export interface UploadLines {
  lines: string[]
  degraded: boolean
}

/** 已知线路的可读说明；索引里新出现、这里没收录的 key 只显示 key 本身。 */
const LINE_LABELS: Record<string, string> = {
  bldsa: 'B站自建',
  bda2: '百度云',
  tx: '腾讯云',
  estx: '第三方-tx',
  akbd: '第三方-bd',
  alia: '海外-阿里云',
  txa: '海外-腾讯云',
}

export const lineLabel = (key: string) => (LINE_LABELS[key] ? `${key}（${LINE_LABELS[key]}）` : key)

/** 可显式选择的线路列表，不含 auto；`current` 是已保存却不在索引里的值，保留为选项以免下拉显示空白。 */
export function useUploadLines(current?: string | null) {
  const { data } = useSWR<UploadLines>('/v1/upload-lines', fetcher, { revalidateOnFocus: false })
  // B 站每次返回的顺序都不同（负载均衡），按字母排序让下拉稳定。
  const lines = [...(data?.lines ?? [])].sort()
  const stale = current && current.toLowerCase() !== 'auto' && !lines.includes(current)
  return {
    lines: stale ? [...lines, current] : lines,
    degraded: data?.degraded ?? false,
    isLoading: !data,
  }
}

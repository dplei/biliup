'use client'
import { Button, Layout, Popconfirm, Select, Table, Tag, Toast, Typography } from '@douyinfe/semi-ui'
import { IconDeleteStroked, IconRefresh, IconSendStroked } from '@douyinfe/semi-icons'
import { useEffect, useMemo, useState } from 'react'
import useSWR from 'swr'
import { fetcher, requestDelete, sendRequest } from '../../lib/api-streamer'

interface MissingSegment {
  id: number
  live_streamer_id: number
  streamer_info_id: number
  upload_session_id: number | null
  aid: number | null
  file_path: string
  danmaku_file_path: string | null
  segment_order: number
  status: string
  attempts: number
  line_index: number
  next_retry_at: string
  last_error: string | null
  created_at: string
  updated_at: string
  total_bytes: number | null
  uploaded_bytes: number
  current_line: string | null
  upload_started_at: string | null
  last_progress_at: string | null
  // attempt 阶段：preprocessing（本地转码）/ queued（等全局上传许可）/ transferring（真在传）
  attempt_phase: string | null
  phase_started_at: string | null
  last_heartbeat_at: string | null
  line_source: string | null
  last_chunk_index: number | null
  last_chunk_started_at: string | null
  last_chunk_error: string | null
  // 所属会话的投稿结果（后端 JOIN upload_session 得到，真正的番号在这里）
  session_aid: number | null
  session_bvid: string | null
  session_status: string | null
  session_submit_state: string | null
  session_completeness: SessionCompleteness | null
  next_line: string
  line_skip_reason: string | null
  line_candidates: string[]
}

interface UploadLineHealth {
  line_key: string
  consecutive_failures: number
  cooldown_until: string | null
  last_failure_kind: string | null
  last_error: string | null
  updated_at: string
}

interface RecoveryAccepted {
  ok: boolean
  missing_id: number
  eligibility: string
  attempt_token: string | null
  line: string | null
  line_skip_reason: string | null
  status: string
}

const ELIGIBILITY_TEXT: Record<string, string> = {
  already_succeeded: '该分段已经补传成功',
  already_running: '已有一次补传在跑，本页会持续刷新它的进度',
  source_missing: '本地源文件不存在',
  finalized_rejected: '所属会话已投稿完成，不再接受新分段',
  legacy_finalized_edit: '已补进现有稿件；该编辑可能触发重新审核',
  invalid_media: '源文件不是有效录像',
  conflict: '状态已变化，请刷新后重试',
}

interface AttemptHistory {
  id: number
  missing_id: number
  line_key: string | null
  line_source: string | null
  started_at: string
  ended_at: string | null
  phase_reached: string | null
  outcome: string | null
  uploaded_bytes: number
  last_chunk_index: number | null
  error: string | null
}

interface SessionCompleteness {
  total_expected: number
  valid_videos: number
  pending: number
  uploading: number
  failed: number
  source_missing: number
  deleting: number
  succeeded: number
  unknown: number
  earliest_blocking_segment_id: number | null
  reasons: string[]
}

type PendingSubmitAction =
  | 'waiting_segments'
  | 'ready_to_submit'
  | 'submitting'
  | 'retry_scheduled'
  | 'manual_inspection'

interface PendingSubmitSession {
  id: number
  live_streamer_id: number
  streamer_info_id: number
  streamer_name: string
  stream_title: string
  stream_started_at: string
  submit_requested_at: string
  submit_state: string | null
  submit_attempts: number
  submit_retry_attempts: number
  last_submit_at: string | null
  last_submit_error: string | null
  next_submit_at: string | null
  submit_claimed: boolean
  action: PendingSubmitAction
  action_message: string
  completeness: SessionCompleteness
  aid: number | null
  bvid: string | null
  status: string
}

interface SessionRecoveryAccepted {
  upload_session_id: number
  segments_started: number[]
  segments_busy: boolean
  submission_queued: boolean
  blocking_summary: { code: string; message: string; segment_ids: number[] } | null
}

interface StreamerInfo {
  id: number
  name: string
  title: string
  date: number
}

interface RescanResult {
  upload_session_id: number
  scanned: number
  queued: number
  skipped_known: number
  skipped_invalid: number
  skipped_finalized: boolean
  /** 补扫自己新建了会话——正常情况下它应该挂进本场已有的会话。 */
  created_session: boolean
}

const STATUS_META: Record<string, { color: 'grey' | 'red' | 'orange' | 'green'; text: string }> = {
  pending: { color: 'grey', text: '待补传' },
  failed: { color: 'red', text: '失败' },
  uploading: { color: 'orange', text: '补传中' },
  succeeded: { color: 'green', text: '已完成' },
  source_missing: { color: 'grey', text: '源文件缺失' },
}

// 一次 attempt 依次经过这三个阶段，各自有各自的超时。区分它们是这轮修复的核心：
// 「本地转码 20 分钟」和「网络 20 分钟没动静」完全不是一回事。
const PHASE_META: Record<string, { text: string; hint: string }> = {
  preprocessing: { text: '本地转码中', hint: '音量标准化与时间戳修复，尚未开始上传' },
  queued: { text: '排队等待上传', hint: '全局同时只允许一个上传，正在等前一段传完' },
  transferring: { text: '传输中', hint: '正在向上传线路推送分块' },
}

const LINE_SOURCE_TEXT: Record<string, string> = {
  configured: '跟随配置',
  manual: '手动指定',
  fallback: '回退（配置线路冷却中）',
  auto_probe: 'auto 探测',
}

const OUTCOME_META: Record<string, { color: 'green' | 'red' | 'orange' | 'grey'; text: string }> = {
  succeeded: { color: 'green', text: '成功' },
  failed: { color: 'red', text: '失败' },
  cancelled: { color: 'orange', text: '已取消' },
  stale: { color: 'grey', text: '租约超时' },
}

const SUBMIT_ACTION_META: Record<PendingSubmitAction, { color: 'green' | 'red' | 'orange' | 'grey'; text: string }> = {
  waiting_segments: { color: 'orange', text: '等待分段' },
  ready_to_submit: { color: 'green', text: '待投稿' },
  submitting: { color: 'orange', text: '投稿中' },
  retry_scheduled: { color: 'orange', text: '退避重试' },
  manual_inspection: { color: 'red', text: '需人工核对' },
}

// 可手动指定的线路。空串表示「跟随配置」，也就是不传 line 参数。
const LINE_OPTIONS = [
  { value: '', label: '跟随配置' },
  { value: 'auto', label: 'auto（自动探测）' },
  { value: 'bda2', label: 'bda2' },
  { value: 'bda', label: 'bda' },
  { value: 'tx', label: 'tx' },
  { value: 'txa', label: 'txa' },
  { value: 'alia', label: 'alia' },
  { value: 'bldsa', label: 'bldsa' },
]

const fmtTime = (s?: string | null) => (s ? new Date(s).toLocaleString() : '—')
const fmtBytes = (bytes: number) => `${(bytes / 1024 / 1024).toFixed(1)} MiB`
const fmtDuration = (seconds: number) => {
  const total = Math.max(0, Math.floor(seconds))
  if (total < 60) return `${total}秒`
  const minutes = Math.floor(total / 60)
  if (minutes < 60) return `${minutes}分${total % 60}秒`
  return `${Math.floor(minutes / 60)}小时${minutes % 60}分`
}
const baseName = (p: string) => p.split(/[/\\]/).pop() || p
// Semi Select 的递归泛型在这页两组动态 option 并存时会触发 TS2589；运行时 props
// 仍由 Semi 校验，这里只截断无意义的类型展开。
const SimpleSelect = Select as any

export default function MissingRecovery() {
  const { Header, Content } = Layout
  const { Text } = Typography
  const [statusFilter, setStatusFilter] = useState<'active' | 'succeeded' | 'all'>('active')
  const {
    data: rows,
    isLoading,
    mutate,
  } = useSWR<MissingSegment[]>(`/v1/uploads/missing?status=${statusFilter}`, fetcher, {
    refreshInterval: 5000,
  })
  const { data: streamerInfos } = useSWR<StreamerInfo[]>('/v1/streamer-info', fetcher)
  const { data: lineHealth } = useSWR<UploadLineHealth[]>('/v1/health/upload-lines', fetcher, {
    refreshInterval: 15000,
  })
  const {
    data: pendingSessions,
    isLoading: pendingSessionsLoading,
    mutate: mutatePendingSessions,
  } = useSWR<PendingSubmitSession[]>('/v1/uploads/sessions/pending', fetcher, {
    refreshInterval: 5000,
  })
  const [recoveringId, setRecoveringId] = useState<number | null>(null)
  const [retryingId, setRetryingId] = useState<number | null>(null)
  const [stoppingId, setStoppingId] = useState<number | null>(null)
  const [deletingId, setDeletingId] = useState<number | null>(null)
  const [recoveringSessionId, setRecoveringSessionId] = useState<number | null>(null)
  // 每行各自的线路选择；空串（或缺省）表示「跟随配置」。
  const [lineChoice, setLineChoice] = useState<Record<number, string>>({})
  const [rescanStreamerInfoId, setRescanStreamerInfoId] = useState<number | null>(null)
  const [rescanning, setRescanning] = useState(false)
  const [now, setNow] = useState(Date.now())

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])

  const recentStreamerInfos = useMemo(
    () => [...(streamerInfos ?? [])].sort((a, b) => b.date - a.date).slice(0, 100),
    [streamerInfos],
  )
  // 冷却中的线路。bldsa 的证书熔断以前只在 /v1/health/upload-lines 看得到，
  // 补传页完全不可见——于是「为什么它不走我配的线」在这一页永远问不出答案。
  const coolingLines = useMemo(
    () => (lineHealth ?? []).filter((line) => line.cooldown_until != null && new Date(line.cooldown_until).getTime() > now),
    [lineHealth, now],
  )
  useEffect(() => {
    if (rescanStreamerInfoId == null && recentStreamerInfos.length > 0) {
      setRescanStreamerInfoId(recentStreamerInfos[0].id)
    }
  }, [recentStreamerInfos, rescanStreamerInfoId])

  const handleRescan = async () => {
    if (rescanStreamerInfoId == null) {
      Toast.warning('请先选择一场直播')
      return
    }
    setRescanning(true)
    try {
      const result = (await sendRequest('/v1/uploads/missing/rescan', {
        arg: { streamer_info_id: rescanStreamerInfoId },
      })) as RescanResult
      Toast.success(
        result.skipped_finalized
          ? `会话 #${result.upload_session_id} 已投稿完成，补扫未创建新的补传任务`
          : `补扫完成：${result.queued} 段已加入会话 #${result.upload_session_id}，` +
            `${result.skipped_known} 段已登记，${result.skipped_invalid} 段无效`,
      )
      if (result.created_session) {
        // 本场没有可挂接的会话时补扫才会新建。发生在录制中就意味着一场直播被拆成了两个稿件。
        Toast.warning(`会话 #${result.upload_session_id} 是本次补扫新建的，请确认它不该并入本场已有会话`)
      }
      setStatusFilter('active')
      await mutate()
    } catch (e: any) {
      Toast.error(`补扫失败：${e?.message ?? e}`)
    } finally {
      setRescanning(false)
    }
  }

  /** 该行当前选中的线路参数；跟随配置时不传，让后端按 config.lines 决策。 */
  const lineArg = (id: number) => {
    const chosen = lineChoice[id]
    return chosen ? { line: chosen } : {}
  }

  /** 补传/重投接口现在是「同步 claim、异步执行」，返回的是已受理，不是已完成。 */
  const describeAccepted = (result: RecoveryAccepted) => {
    if (!result.ok) return null
    const line = result.line ? `，线路 ${result.line}` : ''
    return `已在后台开始补传${line}；进度会在本页自动刷新`
  }

  const handleRecover = async (id: number) => {
    setRecoveringId(id)
    try {
      const result = (await sendRequest(`/v1/uploads/missing/${id}/recover`, {
        arg: lineArg(id),
      })) as RecoveryAccepted
      if (!result.ok) {
        Toast.warning(`未执行补传：${ELIGIBILITY_TEXT[result.eligibility] ?? result.eligibility}`)
      } else {
        Toast.success(describeAccepted(result)!)
        if (result.line_skip_reason) {
          Toast.warning(`已回退线路：${result.line_skip_reason}`)
        }
      }
      await mutate()
    } catch (e: any) {
      Toast.error(`补传失败：${e?.message ?? e}`)
    } finally {
      setRecoveringId(null)
    }
  }

  const handleRetry = async (id: number) => {
    setRetryingId(id)
    try {
      const result = (await sendRequest(`/v1/uploads/missing/${id}/retry`, {
        arg: lineArg(id),
      })) as RecoveryAccepted
      if (result.ok) {
        Toast.success(describeAccepted(result)!)
        if (result.line_skip_reason) {
          Toast.warning(`已回退线路：${result.line_skip_reason}`)
        }
      } else {
        Toast.warning(`未重新发起：${ELIGIBILITY_TEXT[result.eligibility] ?? result.eligibility}`)
      }
      await mutate()
    } catch (e: any) {
      Toast.error(`重新补投失败：${e?.message ?? e}`)
    } finally {
      setRetryingId(null)
    }
  }

  const handleStop = async (id: number) => {
    setStoppingId(id)
    try {
      const result = (await sendRequest(`/v1/uploads/missing/${id}/stop`, { arg: {} })) as {
        outcome: string
        status?: string
      }
      if (result.outcome === 'stopped') {
        Toast.success('已停止当前 attempt；不会自动重传，请自行决定下一步')
      } else {
        Toast.warning(`当前没有在跑的 attempt（状态：${result.status ?? '未知'}）`)
      }
      await mutate()
    } catch (e: any) {
      Toast.error(`停止失败：${e?.message ?? e}`)
    } finally {
      setStoppingId(null)
    }
  }

  const handleDelete = async (id: number) => {
    setDeletingId(id)
    try {
      await requestDelete('/v1/uploads/missing', { arg: id })
      Toast.success('已删除缺失记录和本地文件')
      await mutate()
    } catch (e: any) {
      Toast.error(`删除失败：${e?.message ?? e}`)
    } finally {
      setDeletingId(null)
    }
  }

  const handleSessionRecover = async (id: number) => {
    setRecoveringSessionId(id)
    try {
      const result = (await sendRequest(`/v1/uploads/sessions/${id}/recover`, {
        arg: {},
      })) as SessionRecoveryAccepted
      if (result.segments_started.length > 0) {
        Toast.success(`会话 #${id} 已开始补传 ${result.segments_started.length} 个分段`)
      } else if (result.submission_queued) {
        Toast.success(`会话 #${id} 已排队投稿，页面会自动刷新结果`)
      } else if (result.blocking_summary) {
        Toast.warning(result.blocking_summary.message)
      } else if (result.segments_busy) {
        Toast.warning(`会话 #${id} 已有恢复任务在运行`)
      } else {
        Toast.info(`会话 #${id} 状态已刷新`)
      }
      await Promise.all([mutate(), mutatePendingSessions()])
    } catch (e: any) {
      Toast.error(`恢复会话失败：${e?.message ?? e}`)
    } finally {
      setRecoveringSessionId(null)
    }
  }

  /** 线路下拉：默认「跟随配置」，选定后 recover/retry 会严格按它走（除非该线路正在冷却）。 */
  const renderLinePicker = (record: MissingSegment) => (
    <SimpleSelect
      size="small"
      style={{ width: 148 }}
      value={lineChoice[record.id] ?? ''}
      onChange={(value: unknown) =>
        setLineChoice((prev) => ({ ...prev, [record.id]: String(value ?? '') }))
      }
      optionList={LINE_OPTIONS.map((option) => ({
        value: option.value,
        label: coolingLines.some((line) => line.line_key === option.value)
          ? `${option.label}（冷却中）`
          : option.label,
      }))}
    />
  )

  const columns = [
    {
      title: '文件',
      dataIndex: 'file_path',
      render: (path: string, record: MissingSegment) => (
        <div id={`missing-segment-${record.id}`}>
          <Text ellipsis={{ showTooltip: { opts: { content: path } } }} style={{ maxWidth: 240 }}>
            {baseName(path)}
          </Text>
        </div>
      ),
    },
    { title: '分 P 顺序', dataIndex: 'segment_order', width: 96 },
    {
      title: '状态',
      dataIndex: 'status',
      width: 100,
      render: (status: string) => {
        const meta = STATUS_META[status] ?? { color: 'grey' as const, text: status }
        return <Tag color={meta.color}>{meta.text}</Tag>
      },
    },
    { title: '尝试次数', dataIndex: 'attempts', width: 96 },
    {
      title: '上传进度',
      dataIndex: 'uploaded_bytes',
      width: 260,
      render: (_: number, record: MissingSegment) => {
        if (record.status !== 'uploading') return '—'
        const phase = record.attempt_phase ? PHASE_META[record.attempt_phase] : undefined
        const phaseSeconds = record.phase_started_at
          ? (now - new Date(record.phase_started_at).getTime()) / 1000
          : 0
        // 转码与排队阶段没有网络字节可言，显示「已无进度」只会让人误以为卡住了。
        if (record.attempt_phase && record.attempt_phase !== 'transferring') {
          return (
            <div>
              <div>{phase?.text ?? record.attempt_phase} · 已 {fmtDuration(phaseSeconds)}</div>
              <Text type="tertiary" size="small">{phase?.hint ?? ''}</Text>
              <div><Text type="tertiary" size="small">开始于 {fmtTime(record.upload_started_at)}</Text></div>
            </div>
          )
        }
        const total = record.total_bytes ?? 0
        const percent = total > 0 ? Math.min(100, (record.uploaded_bytes / total) * 100) : 0
        const stalledSeconds = record.last_progress_at
          ? Math.max(0, Math.floor((now - new Date(record.last_progress_at).getTime()) / 1000))
          : 0
        return (
          <div>
            <div>{percent.toFixed(1)}% · {fmtBytes(record.uploaded_bytes)} / {fmtBytes(total)}</div>
            <Text type="tertiary" size="small">
              {record.current_line ?? '未知线路'}
              {record.line_source ? `（${LINE_SOURCE_TEXT[record.line_source] ?? record.line_source}）` : ''}
              {' · '}已无进度 {fmtDuration(stalledSeconds)}
            </Text>
            {record.last_chunk_index != null && (
              <div>
                <Text type="tertiary" size="small">
                  当前分块 #{record.last_chunk_index}
                  {record.last_chunk_started_at
                    ? ` · 已 ${fmtDuration((now - new Date(record.last_chunk_started_at).getTime()) / 1000)}`
                    : ''}
                </Text>
              </div>
            )}
            <div><Text type="tertiary" size="small">开始于 {fmtTime(record.upload_started_at)}</Text></div>
          </div>
        )
      },
    },
    {
      title: '线路',
      dataIndex: 'line_index',
      width: 210,
      render: (_: number, record: MissingSegment) => (
        <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
          <div>
            下次：{lineChoice[record.id] || record.next_line}
            {lineChoice[record.id] ? '（手动指定）' : ''}
          </div>
          {record.status !== 'succeeded' && renderLinePicker(record)}
          {record.line_skip_reason && (
            <Text type="tertiary" size="small">
              已跳过 {record.line_skip_reason}
            </Text>
          )}
        </div>
      ),
    },
    {
      title: '下次重试',
      dataIndex: 'next_retry_at',
      width: 180,
      render: (s: string) => fmtTime(s),
    },
    {
      title: '最后错误',
      dataIndex: 'last_error',
      render: (err: string | null, record: MissingSegment) => {
        if (!err && !record.last_chunk_error) return '—'
        return (
          <div>
            {err && (
              <Text type="danger" ellipsis={{ showTooltip: { opts: { content: err } } }} style={{ maxWidth: 280 }}>
                {err}
              </Text>
            )}
            {record.last_chunk_error && (
              <div>
                <Text
                  type="tertiary"
                  size="small"
                  ellipsis={{ showTooltip: { opts: { content: record.last_chunk_error } } }}
                  style={{ maxWidth: 280 }}
                >
                  {record.last_chunk_error}
                </Text>
              </div>
            )}
          </div>
        )
      },
    },
    {
      title: '去向',
      dataIndex: 'destination',
      width: 220,
      render: (_: unknown, record: MissingSegment) => {
        if (record.status !== 'succeeded') return '—'
        // 番号优先看 missing 行自身 aid，没有再回退到所属会话的 aid/bvid。
        const aid = record.aid ?? record.session_aid
        if (aid != null) {
          return (
            <a
              href={`https://www.bilibili.com/video/av${aid}`}
              target="_blank"
              rel="noreferrer"
              style={{ color: 'inherit' }}
            >
              已投稿 av{aid}
            </a>
          )
        }
        if (record.session_bvid) {
          return (
            <a
              href={`https://www.bilibili.com/video/${record.session_bvid}`}
              target="_blank"
              rel="noreferrer"
              style={{ color: 'inherit' }}
            >
              已投稿 {record.session_bvid}
            </a>
          )
        }
        if (record.upload_session_id != null) {
          return (
            <Text type="tertiary">
              待提交（会话 #{record.upload_session_id}，尚未投稿）
            </Text>
          )
        }
        return '—'
      },
    },
    {
      title: '完成时间',
      dataIndex: 'updated_at',
      width: 180,
      render: (s: string, record: MissingSegment) =>
        record.status === 'succeeded' ? fmtTime(s) : '—',
    },
    {
      title: '操作',
      dataIndex: 'operate',
      width: 180,
      fixed: 'right' as const,
      render: (_: unknown, record: MissingSegment) => {
        if (record.status === 'succeeded') return '—'

        if (record.status === 'source_missing') {
          return (
            <div style={{ display: 'flex', gap: 4 }}>
              <Button
                theme="borderless"
                icon={<IconRefresh />}
                loading={recoveringId === record.id}
                onClick={() => handleRecover(record.id)}
              >
                重新检查文件
              </Button>
              <Popconfirm
                title="删除这条缺失记录？"
                content="仅删除本地记录；源文件已经不存在。"
                okText="删除"
                okButtonProps={{ type: 'danger' }}
                onConfirm={() => handleDelete(record.id)}
              >
                <Button theme="borderless" type="danger" icon={<IconDeleteStroked />} loading={deletingId === record.id} />
              </Popconfirm>
            </div>
          )
        }

        if (record.status === 'uploading') {
          return (
            <div style={{ display: 'flex', gap: 4 }}>
              <Popconfirm
                title="停止这次补传？"
                content="取消当前 attempt 并释放它，状态转为「失败」。不会自动重传——停止之后由你决定下一步。"
                okText="停止"
                okButtonProps={{ type: 'danger' }}
                onConfirm={() => handleStop(record.id)}
              >
                <Button theme="borderless" type="danger" loading={stoppingId === record.id}>
                  停止
                </Button>
              </Popconfirm>
              <Popconfirm
                title="换线重投这一段？"
                content="将取消旧 attempt，等待其退出，并按上方选择的线路重新上传该分段。"
                okText="换线重投"
                onConfirm={() => handleRetry(record.id)}
              >
                <Button
                  theme="borderless"
                  icon={<IconSendStroked />}
                  loading={retryingId === record.id}
                >
                  换线重投
                </Button>
              </Popconfirm>
            </div>
          )
        }

        return (
          <div style={{ display: 'flex', gap: 4 }}>
            <Popconfirm
              title="补传这一段？"
              content="将在后台重新上传该分段，并按原分 P 位置补进对应稿件（已投稿）或待提交会话。"
              okText="补传"
              onConfirm={() => handleRecover(record.id)}
            >
              <Button
                theme="borderless"
                icon={<IconSendStroked />}
                loading={recoveringId === record.id}
              >
                补传
              </Button>
            </Popconfirm>
            <Popconfirm
              title="删除这条缺失记录？"
              content="将删除缺失补传记录，并同时删除对应本地视频文件和弹幕文件。此操作不会补投到 B 站。"
              okText="删除"
              okButtonProps={{ type: 'danger' }}
              onConfirm={() => handleDelete(record.id)}
            >
              <Button
                theme="borderless"
                type="danger"
                icon={<IconDeleteStroked />}
                loading={deletingId === record.id}
              />
            </Popconfirm>
          </div>
        )
      },
    },
  ]

  return (
    <>
      <Header style={{ backgroundColor: 'var(--semi-color-bg-1)' }}>
        <nav
          style={{
            display: 'flex',
            paddingLeft: '25px',
            paddingRight: '25px',
            alignItems: 'center',
            justifyContent: 'space-between',
            flexWrap: 'wrap',
            boxShadow: '0 1px 2px 0 rgb(0 0 0 / 0.05)',
          }}
        >
          <div style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>
            <IconSendStroked
              style={{
                backgroundColor: 'rgba(var(--semi-pink-5), 1)',
                borderRadius: 'var(--semi-border-radius-large)',
                color: 'var(--semi-color-bg-0)',
                padding: '6px',
              }}
              size="large"
            />
            <h4>缺失补传</h4>
          </div>
          <div style={{ display: 'flex', gap: 10, alignItems: 'center' }}>
            <SimpleSelect
              value={rescanStreamerInfoId ?? undefined}
              onChange={(value: unknown) => setRescanStreamerInfoId(Number(value))}
              filter
              placeholder="选择本场直播"
              style={{ width: 300 }}
              optionList={recentStreamerInfos.map((info) => ({
                value: info.id,
                label: `${info.name} · ${new Date(info.date * 1000).toLocaleString()}`,
              }))}
            />
            <Button icon={<IconRefresh />} loading={rescanning} onClick={handleRescan}>
              补扫本场
            </Button>
            <SimpleSelect
              value={statusFilter}
              onChange={(v: unknown) => setStatusFilter(v as 'active' | 'succeeded' | 'all')}
              style={{ width: 130 }}
              optionList={[
                { value: 'active', label: '待补传' },
                { value: 'succeeded', label: '已补传' },
                { value: 'all', label: '全部' },
              ]}
            />
            <Button
              icon={<IconRefresh />}
              type="tertiary"
              onClick={() => Promise.all([mutate(), mutatePendingSessions()])}
            >
              刷新
            </Button>
          </div>
        </nav>
      </Header>
      <Content style={{ padding: '24px', backgroundColor: 'var(--semi-color-bg-0)' }}>
        {coolingLines.length > 0 && (
          <div
            style={{
              marginBottom: 16,
              padding: 12,
              borderRadius: 6,
              background: 'var(--semi-color-danger-light-default)',
            }}
          >
            <Text strong>以下上传线路正在冷却，补传会自动绕开它们</Text>
            <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginTop: 4 }}>
              {coolingLines.map((line) => (
                <Text key={line.line_key} type="tertiary" size="small">
                  {line.line_key}：{line.last_failure_kind ?? '失败'} · 连续失败 {line.consecutive_failures} 次 ·
                  剩余 {fmtDuration((new Date(line.cooldown_until!).getTime() - now) / 1000)}
                </Text>
              ))}
            </div>
          </div>
        )}
        <section style={{ marginBottom: 24 }}>
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 8, marginBottom: 8 }}>
            <Text strong style={{ fontSize: 16 }}>待投稿会话</Text>
            <Text type="tertiary" size="small">独立于下方缺失分段筛选</Text>
          </div>
          {pendingSessionsLoading && <Text type="tertiary">正在读取待投稿状态…</Text>}
          {!pendingSessionsLoading && (pendingSessions?.length ?? 0) === 0 && (
            <div style={{ padding: 12, borderRadius: 6, background: 'var(--semi-color-fill-0)' }}>
              <Text type="tertiary">当前没有待投稿会话</Text>
            </div>
          )}
          <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
            {(pendingSessions ?? []).map((session) => {
              const meta = SUBMIT_ACTION_META[session.action]
              const completeness = session.completeness
              const canRecover = !['submitting', 'manual_inspection'].includes(session.action)
              return (
                <div
                  key={session.id}
                  style={{
                    padding: 12,
                    borderRadius: 6,
                    border: '1px solid var(--semi-color-border)',
                    background: session.action === 'manual_inspection'
                      ? 'var(--semi-color-danger-light-default)'
                      : 'var(--semi-color-warning-light-default)',
                  }}
                >
                  <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
                    <Text strong>会话 #{session.id} · {session.streamer_name}</Text>
                    <Tag color={meta.color}>{meta.text}</Tag>
                    <Text type="tertiary" size="small">{session.stream_title}</Text>
                  </div>
                  <div style={{ marginTop: 4 }}><Text>{session.action_message}</Text></div>
                  <div>
                    <Text type="tertiary" size="small">
                      分段 {completeness.succeeded}/{completeness.total_expected} · 远端投稿 {session.submit_attempts} 次 ·
                      退避 {session.submit_retry_attempts} 次 ·
                      请求于 {fmtTime(session.submit_requested_at)}
                      {session.next_submit_at ? ` · 下次 ${fmtTime(session.next_submit_at)}` : ''}
                    </Text>
                  </div>
                  {session.last_submit_error && (
                    <div><Text type="danger" size="small">最近错误：{session.last_submit_error}</Text></div>
                  )}
                  <div style={{ display: 'flex', gap: 12, marginTop: 8, alignItems: 'center' }}>
                    {session.action === 'waiting_segments' && completeness.earliest_blocking_segment_id != null && (
                      <a href={`#missing-segment-${completeness.earliest_blocking_segment_id}`}>
                        查看阻塞分段 #{completeness.earliest_blocking_segment_id}
                      </a>
                    )}
                    {canRecover && (
                      <Button
                        size="small"
                        loading={recoveringSessionId === session.id}
                        onClick={() => handleSessionRecover(session.id)}
                      >
                        恢复会话
                      </Button>
                    )}
                    {session.action === 'manual_inspection' && (
                      <Text type="danger" size="small">为避免重复稿件，此状态不提供普通重试按钮</Text>
                    )}
                  </div>
                </div>
              )
            })}
          </div>
        </section>
        <Text type="tertiary" style={{ display: 'block', marginBottom: 16 }}>
          录制期间上传失败、尚未补传的分段。下播提交前会自动换线重试到期的分段；这里可手动立即补传，
          补传成功后会按原分 P 位置补进对应稿件或待提交会话。切换「已补传」可查看历史记录与去向，
          其中「#会话号」即日志里的 session，可在「实时日志」按该号检索整条上传链路。若有效录像已留在
          本地但列表中没有记录，请选择对应的本场直播并点「补扫本场」；空片段不会被加入。
          「补传」「换线重投」都是后台执行，接口立刻返回，进度看本页；「停止」只释放卡住的任务，不会自动重传。
          展开任意一行可以看到它先后用过哪些线路、每次为何结束。
        </Text>
        <Table
          rowKey="id"
          columns={columns}
          dataSource={rows}
          loading={isLoading}
          pagination={false}
          scroll={{ x: 'max-content' }}
          expandedRowRender={(record?: MissingSegment) =>
            record ? <AttemptHistoryPanel missingId={record.id} /> : null
          }
          empty={
            <div style={{ padding: '40px 0', textAlign: 'center', color: 'var(--semi-color-text-2)' }}>
              <IconSendStroked
                size="extra-large"
                style={{ color: 'var(--semi-color-text-3)', marginBottom: 8 }}
              />
              <div>暂无待补传的缺失分段</div>
            </div>
          }
        />
      </Content>
    </>
  )
}

/**
 * 一行任务的 attempt 历史。
 *
 * `current_line` 只有「现在这次用的哪条线」，`line_index` 又只是个失败计数，所以
 * 「这个任务先后换过哪些线、各自为何结束」在页面上一直无从回答。后端为此新建了
 * `upload_attempt` 表，一次 attempt 一行、必有终态。
 */
function AttemptHistoryPanel({ missingId }: { missingId: number }) {
  const { Text } = Typography
  const { data, isLoading } = useSWR<AttemptHistory[]>(
    `/v1/uploads/missing/${missingId}/attempts`,
    fetcher,
  )

  if (isLoading) return <Text type="tertiary">加载线路切换历史…</Text>
  if (!data || data.length === 0) return <Text type="tertiary">这一段还没有任何 attempt 记录</Text>

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 6, padding: '4px 0' }}>
      {data.map((attempt) => {
        const meta = attempt.outcome ? OUTCOME_META[attempt.outcome] : undefined
        const elapsed = attempt.ended_at
          ? (new Date(attempt.ended_at).getTime() - new Date(attempt.started_at).getTime()) / 1000
          : null
        return (
          <div key={attempt.id} style={{ display: 'flex', gap: 8, alignItems: 'baseline', flexWrap: 'wrap' }}>
            <Tag color={meta?.color ?? 'grey'}>{meta?.text ?? attempt.outcome ?? '进行中'}</Tag>
            <Text>{attempt.line_key ?? '未知线路'}</Text>
            <Text type="tertiary" size="small">
              {LINE_SOURCE_TEXT[attempt.line_source ?? ''] ?? attempt.line_source ?? ''}
            </Text>
            <Text type="tertiary" size="small">
              {fmtTime(attempt.started_at)}
              {elapsed != null ? ` · 历时 ${fmtDuration(elapsed)}` : ' · 进行中'}
            </Text>
            <Text type="tertiary" size="small">
              止步于 {PHASE_META[attempt.phase_reached ?? '']?.text ?? attempt.phase_reached ?? '—'}
              {attempt.uploaded_bytes > 0 ? ` · 已确认 ${fmtBytes(attempt.uploaded_bytes)}` : ''}
              {attempt.last_chunk_index != null ? ` · 分块 #${attempt.last_chunk_index}` : ''}
            </Text>
            {attempt.error && (
              <Text
                type="danger"
                size="small"
                ellipsis={{ showTooltip: { opts: { content: attempt.error } } }}
                style={{ maxWidth: 420 }}
              >
                {attempt.error}
              </Text>
            )}
          </div>
        )
      })}
    </div>
  )
}

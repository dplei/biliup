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
  // 所属会话的投稿结果（后端 JOIN upload_session 得到，真正的番号在这里）
  session_aid: number | null
  session_bvid: string | null
  session_status: string | null
  session_submit_state: string | null
  session_completeness: SessionCompleteness | null
  next_line: string
  line_skip_reason: string | null
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
}

const STATUS_META: Record<string, { color: 'grey' | 'red' | 'orange' | 'green'; text: string }> = {
  pending: { color: 'grey', text: '待补传' },
  failed: { color: 'red', text: '失败' },
  uploading: { color: 'orange', text: '补传中' },
  succeeded: { color: 'green', text: '已完成' },
  source_missing: { color: 'grey', text: '源文件缺失' },
}

const fmtTime = (s?: string | null) => (s ? new Date(s).toLocaleString() : '—')
const fmtBytes = (bytes: number) => `${(bytes / 1024 / 1024).toFixed(1)} MiB`
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
  const [recoveringId, setRecoveringId] = useState<number | null>(null)
  const [retryingId, setRetryingId] = useState<number | null>(null)
  const [deletingId, setDeletingId] = useState<number | null>(null)
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
  const blockedSessions = useMemo(() => {
    const byId = new Map<number, MissingSegment>()
    for (const row of rows ?? []) {
      if (
        row.upload_session_id != null &&
        row.session_status !== 'finalized' &&
        row.session_submit_state === 'blocked_missing_segments'
      ) {
        byId.set(row.upload_session_id, row)
      }
    }
    return Array.from(byId.values())
  }, [rows])
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
      setStatusFilter('active')
      await mutate()
    } catch (e: any) {
      Toast.error(`补扫失败：${e?.message ?? e}`)
    } finally {
      setRescanning(false)
    }
  }

  const handleRecover = async (id: number) => {
    setRecoveringId(id)
    try {
      const result = (await sendRequest(`/v1/uploads/missing/${id}/recover`, { arg: {} })) as {
        ok: boolean
        eligibility: string
      }
      if (!result.ok) {
        Toast.warning(`未执行补传：${result.eligibility}`)
      } else if (result.eligibility === 'legacy_finalized_edit') {
        Toast.success('已补进现有稿件；该编辑可能触发重新审核')
      } else {
        Toast.success('补传成功，已补进对应稿件或待提交会话')
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
      const result = (await sendRequest(`/v1/uploads/missing/${id}/retry`, { arg: {} })) as {
        ok: boolean
        eligibility: string
      }
      if (result.ok) Toast.success('已重新发起补投')
      else Toast.warning(`未重新发起：${result.eligibility}`)
      await mutate()
    } catch (e: any) {
      Toast.error(`重新补投失败：${e?.message ?? e}`)
    } finally {
      setRetryingId(null)
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
      width: 230,
      render: (_: number, record: MissingSegment) => {
        if (record.status !== 'uploading') return '—'
        const total = record.total_bytes ?? 0
        const percent = total > 0 ? Math.min(100, (record.uploaded_bytes / total) * 100) : 0
        const stalledSeconds = record.last_progress_at
          ? Math.max(0, Math.floor((now - new Date(record.last_progress_at).getTime()) / 1000))
          : 0
        return (
          <div>
            <div>{percent.toFixed(1)}% · {fmtBytes(record.uploaded_bytes)} / {fmtBytes(total)}</div>
            <Text type="tertiary" size="small">
              {record.current_line ?? '未知线路'} · 已无进度 {Math.floor(stalledSeconds / 60)}分{stalledSeconds % 60}秒
            </Text>
            <div><Text type="tertiary" size="small">开始于 {fmtTime(record.upload_started_at)}</Text></div>
          </div>
        )
      },
    },
    {
      title: '下次线路',
      dataIndex: 'line_index',
      width: 180,
      render: (_: number, record: MissingSegment) => (
        <div>
          <div>{record.next_line}</div>
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
      render: (err: string | null) =>
        err ? (
          <Text type="danger" ellipsis={{ showTooltip: { opts: { content: err } } }} style={{ maxWidth: 280 }}>
            {err}
          </Text>
        ) : (
          '—'
        ),
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
      width: 110,
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
            <Popconfirm
              title="重新补投这一段？"
              content="将取消旧 attempt，等待其退出，并从下一条健康线路重新上传该分段。"
              okText="重新补投"
              onConfirm={() => handleRetry(record.id)}
            >
              <Button
                theme="borderless"
                icon={<IconSendStroked />}
                loading={retryingId === record.id}
              >
                重新补投
              </Button>
            </Popconfirm>
          )
        }

        return (
          <div style={{ display: 'flex', gap: 4 }}>
            <Popconfirm
              title="补传这一段？"
              content="将重新上传该分段，并按原分 P 位置补进对应稿件（已投稿）或待提交会话。"
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
            <Button icon={<IconRefresh />} type="tertiary" onClick={() => mutate()}>
              刷新
            </Button>
          </div>
        </nav>
      </Header>
      <Content style={{ padding: '24px', backgroundColor: 'var(--semi-color-bg-0)' }}>
        {blockedSessions.map((row) => {
          const completeness = row.session_completeness
          if (!completeness) return null
          const incomplete = Math.max(
            completeness.reasons.length > 0 ? 1 : 0,
            completeness.total_expected - completeness.valid_videos,
          )
          return (
            <div
              key={row.upload_session_id!}
              style={{
                marginBottom: 16,
                padding: 12,
                borderRadius: 6,
                background: 'var(--semi-color-warning-light-default)',
              }}
            >
              <Text strong>会话 #{row.upload_session_id} 因 {incomplete} 个未完成分段暂停投稿</Text>
              <div>
                <Text type="tertiary" size="small">
                  待传 {completeness.pending} · 上传中 {completeness.uploading} · 失败 {completeness.failed} ·
                  源文件缺失 {completeness.source_missing} · 删除中 {completeness.deleting} · 异常 {completeness.unknown}
                </Text>
              </div>
              {completeness.earliest_blocking_segment_id != null && (
                <a href={`#missing-segment-${completeness.earliest_blocking_segment_id}`}>
                  查看最早阻塞分段 #{completeness.earliest_blocking_segment_id}
                </a>
              )}
            </div>
          )
        })}
        <Text type="tertiary" style={{ display: 'block', marginBottom: 16 }}>
          录制期间上传失败、尚未补传的分段。下播提交前会自动换线重试到期的分段；这里可手动立即补传，
          补传成功后会按原分 P 位置补进对应稿件或待提交会话。切换「已补传」可查看历史记录与去向，
          其中「#会话号」即日志里的 session，可在「实时日志」按该号检索整条上传链路。若有效录像已留在
          本地但列表中没有记录，请选择对应的本场直播并点「补扫本场」；空片段不会被加入。
        </Text>
        <Table
          rowKey="id"
          columns={columns}
          dataSource={rows}
          loading={isLoading}
          pagination={false}
          scroll={{ x: 'max-content' }}
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

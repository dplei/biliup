'use client'
import { Button, Layout, Popconfirm, Select, Table, Tag, Toast, Typography } from '@douyinfe/semi-ui'
import { IconRefresh, IconSendStroked } from '@douyinfe/semi-icons'
import { useState } from 'react'
import useSWR from 'swr'
import { fetcher, sendRequest } from '../../lib/api-streamer'

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
  // 所属会话的投稿结果（后端 JOIN upload_session 得到，真正的番号在这里）
  session_aid: number | null
  session_bvid: string | null
  session_status: string | null
}

const FALLBACK_LINES = ['bda2', 'tx', 'bldsa', 'auto']
const lineName = (i: number) => FALLBACK_LINES[((i % 4) + 4) % 4] ?? 'auto'

const STATUS_META: Record<string, { color: 'grey' | 'red' | 'orange' | 'green'; text: string }> = {
  pending: { color: 'grey', text: '待补传' },
  failed: { color: 'red', text: '失败' },
  uploading: { color: 'orange', text: '补传中' },
  succeeded: { color: 'green', text: '已完成' },
}

const fmtTime = (s?: string | null) => (s ? new Date(s).toLocaleString() : '—')
const baseName = (p: string) => p.split(/[/\\]/).pop() || p

export default function MissingRecovery() {
  const { Header, Content } = Layout
  const { Text } = Typography
  const [statusFilter, setStatusFilter] = useState<'active' | 'succeeded' | 'all'>('active')
  const {
    data: rows,
    isLoading,
    mutate,
  } = useSWR<MissingSegment[]>(`/v1/uploads/missing?status=${statusFilter}`, fetcher)
  const [recoveringId, setRecoveringId] = useState<number | null>(null)

  const handleRecover = async (id: number) => {
    setRecoveringId(id)
    try {
      await sendRequest(`/v1/uploads/missing/${id}/recover`, { arg: {} })
      Toast.success('补传成功，已补进对应稿件或待提交会话')
      await mutate()
    } catch (e: any) {
      Toast.error(`补传失败：${e?.message ?? e}`)
    } finally {
      setRecoveringId(null)
    }
  }

  const columns = [
    {
      title: '文件',
      dataIndex: 'file_path',
      render: (path: string) => (
        <Text ellipsis={{ showTooltip: { opts: { content: path } } }} style={{ maxWidth: 240 }}>
          {baseName(path)}
        </Text>
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
      title: '下次线路',
      dataIndex: 'line_index',
      width: 100,
      render: (i: number) => lineName(i),
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
            <a href={`https://www.bilibili.com/video/av${aid}`} target="_blank" rel="noreferrer">
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
      render: (_: unknown, record: MissingSegment) => (
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
            disabled={record.status === 'uploading' && recoveringId !== record.id}
          >
            补传
          </Button>
        </Popconfirm>
      ),
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
            <Select
              value={statusFilter}
              onChange={(v) => setStatusFilter(v as 'active' | 'succeeded' | 'all')}
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
        <Text type="tertiary" style={{ display: 'block', marginBottom: 16 }}>
          录制期间上传失败、尚未补传的分段。下播提交前会自动换线重试到期的分段；这里可手动立即补传，
          补传成功后会按原分 P 位置补进对应稿件或待提交会话。切换「已补传」可查看历史记录与去向，
          其中「#会话号」即日志里的 session，可在「实时日志」按该号检索整条上传链路。
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

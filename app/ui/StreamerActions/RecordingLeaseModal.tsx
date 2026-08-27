'use client'

import React, { useEffect, useMemo, useState } from 'react'
import { Banner, Button, Input, Modal, Notification, Popconfirm, Space, Typography } from '@douyinfe/semi-ui'
import { useSWRConfig } from 'swr'
import {
  clearRecordingLease,
  LiveStreamerEntity,
  saveRecordingLease,
} from '@/app/lib/api-streamer'

interface Props {
  streamer: LiveStreamerEntity
  visible: boolean
  onClose: () => void
}

function toLocalMinute(value: Date) {
  const pad = (number: number) => String(number).padStart(2, '0')
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())}T${pad(value.getHours())}:${pad(value.getMinutes())}`
}

function initialExpiry(streamer: LiveStreamerEntity) {
  if (streamer.recording_lease?.expires_at) {
    return toLocalMinute(new Date(streamer.recording_lease.expires_at))
  }
  const next = new Date(Date.now() + 60 * 60 * 1000)
  next.setSeconds(0, 0)
  return toLocalMinute(next)
}

const stateText: Record<string, string> = {
  scheduled: '待到期',
  grace_current_session: '已到期 · 本场结束后暂停',
  expired_paused: '已到期暂停',
}

const notificationText: Record<string, string> = {
  not_ready: '尚未到通知阶段',
  pending: '等待发送',
  sending: '正在发送',
  failed: '发送失败，后台将自动重试',
  sent: '已发送',
  not_configured: '未配置 Webhook',
}

export default function RecordingLeaseModal({ streamer, visible, onClose }: Props) {
  const { mutate } = useSWRConfig()
  const [expiresAt, setExpiresAt] = useState(() => initialExpiry(streamer))
  const [note, setNote] = useState(streamer.recording_lease?.customer_note ?? '')
  const [saving, setSaving] = useState(false)
  const [serverNow, setServerNow] = useState(streamer.server_now)
  const timezone = useMemo(() => Intl.DateTimeFormat().resolvedOptions().timeZone || '浏览器本地时区', [])

  useEffect(() => {
    if (!visible) return
    setExpiresAt(initialExpiry(streamer))
    setNote(streamer.recording_lease?.customer_note ?? '')
    setServerNow(streamer.server_now)
  }, [visible, streamer])

  const save = async () => {
    const localDate = new Date(expiresAt)
    if (!expiresAt || Number.isNaN(localDate.getTime())) {
      Notification.error({ title: '时间无效', content: '请选择明确的录制期限' })
      return
    }
    const trimmed = note.trim()
    if (!trimmed || Array.from(trimmed).length > 200) {
      Notification.error({ title: '备注无效', content: '客户/需求备注须为 1 到 200 个字符' })
      return
    }
    setSaving(true)
    try {
      const response = await saveRecordingLease(streamer.id, {
        expires_at: localDate.toISOString(),
        customer_note: trimmed,
        expected_lease_id: streamer.recording_lease?.id ?? null,
      })
      setServerNow(response.server_now)
      await mutate('/v1/streamers')
      Notification.success({ title: streamer.recording_lease ? '录制期限已更新' : '录制期限已设置' })
      onClose()
    } catch (error) {
      Notification.error({ title: '保存失败', content: (error as Error).message })
    } finally {
      setSaving(false)
    }
  }

  const clear = async () => {
    const lease = streamer.recording_lease
    if (!lease) return
    setSaving(true)
    try {
      const response = await clearRecordingLease(streamer.id, lease.id)
      setServerNow(response.server_now)
      await mutate('/v1/streamers')
      Notification.success({ title: '录制期限已清除' })
      onClose()
    } catch (error) {
      Notification.error({ title: '清除失败', content: (error as Error).message })
    } finally {
      setSaving(false)
    }
  }

  const lease = streamer.recording_lease
  return (
    <Modal
      title={`录制期限 · ${streamer.remark}`}
      visible={visible}
      onCancel={onClose}
      footer={
        <Space>
          {lease ? (
            <Popconfirm
              title="确定清除录制期限？"
              content="若当前暂停由期限施加，清除后会恢复轮询；人工暂停不会自动恢复。"
              onConfirm={clear}
            >
              <Button type="danger" theme="borderless" disabled={saving}>清除期限</Button>
            </Popconfirm>
          ) : null}
          <Button onClick={onClose} disabled={saving}>取消</Button>
          <Button theme="solid" type="primary" loading={saving} onClick={save}>
            {lease ? '保存 / 延期' : '保存'}
          </Button>
        </Space>
      }
      width={560}
    >
      <Space vertical align="start" spacing="medium" style={{ width: '100%' }}>
        <Typography.Text type="tertiary">
          服务器时间：{serverNow ? new Date(serverNow).toLocaleString() : '加载中'}；显示时区：{timezone}
        </Typography.Text>

        {lease ? (
          <Typography.Text>
            当前状态：{stateText[lease.state] ?? lease.state}；通知：{notificationText[lease.notification_status] ?? lease.notification_status}
          </Typography.Text>
        ) : null}

        {lease?.state === 'grace_current_session' ? (
          <Banner
            type="warning"
            description="本场直播不会被期限中断；确认下播后才会暂停后续录制。"
          />
        ) : null}
        {lease?.state === 'expired_paused' ? (
          <Banner
            type={lease.notification_status === 'failed' ? 'danger' : 'info'}
            description={
              lease.notification_status === 'failed'
                ? `${lease.last_notification_error ?? '通知发送失败'}；后台将自动重试。`
                : '后续新场次已被阻止。延期或清除期限后，只有期限施加的暂停会自动恢复。'
            }
          />
        ) : null}

        <label style={{ width: '100%' }}>
          <Typography.Text strong>录制至（精确到分钟）</Typography.Text>
          <input
            type="datetime-local"
            value={expiresAt}
            onChange={event => setExpiresAt(event.target.value)}
            style={{
              display: 'block', width: '100%', boxSizing: 'border-box', marginTop: 8,
              padding: '8px 12px', border: '1px solid var(--semi-color-border)',
              borderRadius: 6, color: 'var(--semi-color-text-0)', background: 'var(--semi-color-fill-0)',
            }}
          />
        </label>

        <label style={{ width: '100%' }}>
          <Typography.Text strong>客户/需求备注</Typography.Text>
          <Input
            value={note}
            onChange={setNote}
            maxLength={200}
            showClear
            placeholder="必填，例如：客户 A · 八月活动"
            style={{ marginTop: 8 }}
          />
          <Typography.Text type="tertiary">{Array.from(note).length}/200</Typography.Text>
        </label>
      </Space>
    </Modal>
  )
}

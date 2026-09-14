'use client'
import {
    Layout,
    Button,
    Tag,
    Typography,
    Popconfirm,
    Notification,
    Card,
    Dropdown,
} from '@douyinfe/semi-ui'
import {
    IconHelpCircle,
    IconPlusCircle,
    IconVideoListStroked,
    IconEdit2Stroked,
    IconDeleteStroked,
    IconWrench,
    IconUpload,
    IconMoreStroked,
    IconCalendarClockStroked,
} from '@douyinfe/semi-icons'
import { List, ButtonGroup } from '@douyinfe/semi-ui'
import React from 'react'
import useStreamers from '../../lib/use-streamers'
import TemplateModal from '../../ui/TemplateModal'
import OverrideModal from '../../ui/OverrideModal'
import { LiveStreamerEntity, put, requestDelete, sendRequest } from '../../lib/api-streamer'
import useSWRMutation from 'swr/mutation'
import {PauseButton} from "@/app/ui/StreamerActions/PauseButton";
import RecordingLeaseButton from '@/app/ui/StreamerActions/RecordingLeaseButton'
import CheckStreamButton from '@/app/ui/StreamerActions/CheckStreamButton'

export default function Home() {
  const { Header, Content } = Layout
  const { Text } = Typography
  const { streamers, isLoading } = useStreamers()
  const { trigger: deleteStreamers } = useSWRMutation('/v1/streamers', requestDelete)
  const { trigger: updateStreamers } = useSWRMutation('/v1/streamers', put)
  const { trigger } = useSWRMutation('/v1/streamers', sendRequest)

  const onConfirm = async (id: number) => {
    await deleteStreamers(id)
  }
  const handleEntityPostprocessor = (values: any) => {
    if (values?.postprocessor) {
      values.postprocessor = values.postprocessor.map(
        (element: { [key: string]: string } | string) => {
          if (element === 'rm') {
            return { cmd: 'rm' }
          } else if (typeof element === 'object' && !element.cmd) {
            const [key, value] = Object.entries(element)[0]
            return { cmd: key, value: value }
          }
          return element
        }
      )
      // console.log(values.postprocessor);
    }
    return values
  }
  const data: (LiveStreamerEntity & { leaseInfo?: React.ReactNode })[] | undefined = streamers?.map(live => {
    let statusTag
    switch (live.status) {
      case 'Working':
        statusTag = <Tag color="red">直播中</Tag>
        break
      case 'Idle':
        statusTag = <Tag color="green">空闲</Tag>
        break
      case 'Pending':
        statusTag = <Tag color="indigo">检测中</Tag>
        break
      case 'OutOfSchedule':
        statusTag = <Tag color="green">非录播时间</Tag>
        break
      case 'Pause':
        statusTag = <Tag color="pink">暂停中</Tag>
        break
    }
    // 未绑定投稿模板：不录制，额外显示「缺少投稿」标签提示去绑定。
    const missingUpload =
      live.upload_streamers_id == null ? (
        <Tag color="orange">
          缺少投稿
        </Tag>
      ) : null
    const qualityName: Record<string, string> = {
      origin: '原画', uhd: '蓝光', hd: '超清', sd: '高清', ld: '标清', md: '流畅',
      30000: '杜比', 20000: '4K', 10000: '原画', 401: '蓝光-杜比', 400: '蓝光',
      250: '超清', 150: '高清', 80: '流畅', 0: '最低画质',
    }
    const recordingTag =
      live.status === 'Working' && live.recording_quality ? (
        <Tag color="light-blue">
          {(qualityName[live.recording_quality] ?? live.recording_quality)}录制
        </Tag>
      ) : null
    const uploadTag =
      live.upload_status === 'Pending' ? (
        <Tag color="blue" prefixIcon={<IconUpload />}>上传中</Tag>
      ) : null
    let leaseInfo = null
    if (live.recording_lease?.state === 'scheduled') {
      const expiry = new Date(live.recording_lease.expires_at)
      const label = `${String(expiry.getMonth() + 1).padStart(2, '0')}-${String(expiry.getDate()).padStart(2, '0')} ${String(expiry.getHours()).padStart(2, '0')}:${String(expiry.getMinutes()).padStart(2, '0')}`
      leaseInfo = <Text type="secondary">录制至 {label}</Text>
    } else if (live.recording_lease?.state === 'grace_current_session') {
      leaseInfo = <Text type="warning">已到期 · 本场结束后暂停</Text>
    } else if (live.recording_lease?.state === 'expired_paused') {
      leaseInfo = <Text type="danger">已到期暂停</Text>
    }
    return {
      ...handleEntityPostprocessor(live),
      statusTag: (
        <>
          {statusTag}
          {recordingTag}
          {uploadTag}
          {missingUpload}
        </>
      ),
      leaseInfo,
    }
  })

  const handleOk = async (values: any) => {
    if (values?.postprocessor) {
      values.postprocessor = values.postprocessor.map(
        ({ cmd, value }: { cmd: string; value: string }) => (cmd === 'rm' ? 'rm' : { [cmd]: value })
      )
    }
    try {
      const res = await trigger(values)
    } catch (e: any) {
      Notification.error({
        title: '创建失败',
        content: <Typography.Paragraph style={{ maxWidth: 450 }}>{e.message}</Typography.Paragraph>,
        style: { width: 'min-content' },
      })
      throw e
    }
  }

  const handleUpdate = async (values: any) => {
    console.log(values);
    delete values.status
    delete values.statusTag
    delete values.upload_status
    if (values?.postprocessor) {
      values.postprocessor = values.postprocessor.map(
        ({ cmd, value }: { cmd: string; value: string }) => (cmd === 'rm' ? 'rm' : { [cmd]: value })
      )
    }
    try {
      const res = await updateStreamers(values)
    } catch (e: any) {
      Notification.error({
        title: '更新失败',
        content: <Typography.Paragraph style={{ maxWidth: 450 }}>{e.message}</Typography.Paragraph>,
        style: { width: 'min-content' },
      })
      throw e
    }
  }

  return (
    <>
      <Header
        style={{
          backgroundColor: 'var(--semi-color-bg-1)',
          position: 'sticky',
          top: 0,
          zIndex: 1,
        }}
      >
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
          <div
            style={{
              display: 'flex',
              gap: 10,
              justifyContent: 'center',
              alignItems: 'center',
              flexWrap: 'wrap',
            }}
          >
            <IconVideoListStroked
              size="large"
              style={{
                backgroundColor: 'rgba(var(--semi-green-4), 1)',
                borderRadius: 'var(--semi-border-radius-large)',
                color: 'var(--semi-color-bg-0)',
                padding: '6px',
              }}
            />
            <h4>录播管理</h4>
          </div>
          <div
            style={{
              display: 'flex',
              flexWrap: 'wrap',
              alignItems: 'center',
              justifyContent: 'center',
              gap: 6,
            }}
          >
            <Button
              theme="borderless"
              icon={<IconHelpCircle size="large" />}
              style={{
                color: 'var(--semi-color-text-2)',
              }}
              onClick={() => (window.location.href = '/static/ds_update.log')}
            />
            <TemplateModal onOk={handleOk}>
              <Button icon={<IconPlusCircle />} theme="solid" style={{ marginRight: 10 }}>
                新建
              </Button>
            </TemplateModal>
          </div>
        </nav>
      </Header>
      <Content
        style={{
          padding: '24px',
          backgroundColor: 'var(--semi-color-bg-0)',
        }}
      >
        <main>
          <List
            grid={{
              gutter: 12,
              xs: 24,
              sm: 24,
              md: 12,
              lg: 8,
              xl: 8,
              xxl: 6,
            }}
            dataSource={data}
            renderItem={item => (
              <List.Item>
                <Card
                  shadows="hover"
                  style={{
                    margin: '9px 0px',
                    width: '100%',
                  }}
                  bodyStyle={{
                    display: 'flex',
                    flexDirection: 'column',
                    minHeight: 184,
                    padding: 20,
                  }}
                >
                  <div
                    style={{
                      display: 'flex',
                      flexWrap: 'wrap',
                      alignItems: 'center',
                      gap: 6,
                    }}
                  >
                    {item.statusTag}
                  </div>

                  <h3
                    style={{
                      minWidth: 0,
                      margin: '14px 0 0',
                      color: 'var(--semi-color-text-0)',
                      fontSize: 20,
                      lineHeight: '28px',
                      fontWeight: 600,
                      overflow: 'hidden',
                      textOverflow: 'ellipsis',
                      whiteSpace: 'nowrap',
                    }}
                  >
                    {item.remark || '未命名直播间'}
                  </h3>

                  <Text
                    style={{ width: '100%', minWidth: 0, marginTop: 4 }}
                    ellipsis={{ showTooltip: true }}
                    type="tertiary"
                  >
                    {item.url}
                  </Text>

                  {item.leaseInfo ? (
                    <div
                      style={{
                        display: 'flex',
                        alignItems: 'center',
                        gap: 6,
                        marginTop: 12,
                        padding: '8px 10px',
                        borderRadius: 'var(--semi-border-radius-medium)',
                        backgroundColor: 'var(--semi-color-fill-0)',
                      }}
                    >
                      <IconCalendarClockStroked style={{ color: 'var(--semi-color-text-2)' }} />
                      {item.leaseInfo}
                    </div>
                  ) : null}

                  <div
                    style={{
                      marginTop: 'auto',
                      paddingTop: 16,
                      display: 'flex',
                      alignItems: 'center',
                      justifyContent: 'space-between',
                    }}
                  >
                    <ButtonGroup theme="light">
                      <CheckStreamButton streamer={item} />
                      <PauseButton streamer={item}/>
                    </ButtonGroup>
                    <Dropdown
                      trigger="click"
                      position="bottomRight"
                      render={
                        <Dropdown.Menu>
                            <TemplateModal onOk={handleUpdate} entity={item}>
                              <Dropdown.Item icon={<IconEdit2Stroked />}>编辑主播</Dropdown.Item>
                            </TemplateModal>
                            <OverrideModal onOk={handleUpdate} entity={item}>
                              <Dropdown.Item icon={<IconWrench />}>配置覆写</Dropdown.Item>
                            </OverrideModal>
                            <RecordingLeaseButton streamer={item}>
                              <Dropdown.Item icon={<IconCalendarClockStroked />}>录制期限</Dropdown.Item>
                            </RecordingLeaseButton>
                            <Dropdown.Divider />
                            <Popconfirm
                              title="确定是否要删除？"
                              content="此操作将不可逆"
                              onConfirm={async () => await onConfirm(item.id)}
                            >
                              <Dropdown.Item type="danger" icon={<IconDeleteStroked />}>删除</Dropdown.Item>
                            </Popconfirm>
                        </Dropdown.Menu>
                      }
                    >
                      <Button theme="borderless" icon={<IconMoreStroked />} aria-label="更多操作" />
                    </Dropdown>
                  </div>
                </Card>

              </List.Item>
            )}
          />
        </main>
      </Content>
    </>
  )
}

import {
  Form,
  Modal,
  Notification,
  Collapse,
  Select,
  Avatar,
  useFormState,
} from '@douyinfe/semi-ui'
import { FormApi } from '@douyinfe/semi-ui/lib/es/form'
import React, { useRef } from 'react'
import { useState } from 'react'
import useSWR from 'swr'
import { fetcher, LiveStreamerEntity, StudioEntity } from '../lib/api-streamer'
import { SupportedPlatforms } from '@/app/ui/plugins'
import { useBiliUsers } from '../lib/use-streamers'
import CoverBackgroundField from './CoverBackgroundField'
import CoverPreviewButton from './CoverPreviewButton'

type PluginProps = {
  entity?: LiveStreamerEntity
  list?: { value: number; label: React.ReactNode }[]
  initValues?: any
}

type TemplateModalProps = {
  visible?: boolean
  entity?: LiveStreamerEntity
  children?: React.ReactNode
  onOk: (e: any) => Promise<void>
}

const removeCircularReferences = (obj: any, seen = new WeakSet()): any => {
  // 处理 null 或非对象类型
  if (obj === null || typeof obj !== 'object') return obj

  // 检测循环引用
  if (seen.has(obj)) return '[Circular Reference]'
  seen.add(obj)

  if (Array.isArray(obj)) {
    return obj.map((item: any) => removeCircularReferences(item, seen))
  }

  const result: Record<string, any> = {}
  for (const [key, value] of Object.entries(obj)) {
    // 跳过 React 相关的属性
    if (key === '_context' || key === 'Provider' || key === 'Consumer') continue
    result[key] = removeCircularReferences(value, seen)
  }
  return result
}

const serializeTimeRange = (timeRange: LiveStreamerEntity['time_range']) => {
  if (Array.isArray(timeRange)) {
    return JSON.stringify(timeRange.map(date => date.toISOString()))
  }
  return timeRange
}

/**
 * 封面设置区：主播级的背景图，覆盖所属上传模板的同名设置。
 *
 * 刻意放在下面那个 Collapse **之外**：Semi 的折叠面板未展开时 children 不挂载，
 * 字段也就不进 values，提交时「用户没展开过」与「用户主动清空」看起来一模一样——
 * 而这两者一个要保持原值、一个要清空，判错方向就是把用户配好的背景悄悄抹掉。
 */
const CoverSection: React.FC<{ template?: StudioEntity; templatesLoading: boolean }> = ({
  template,
  templatesLoading,
}) => {
  const { values } = useFormState()

  // 主播级留空时回退到模板的背景——与投稿时的三级回退（主播 → 模板 → 纯黑）一致，
  // 预览才真的等于产出。
  const background = values.cover_background?.trim() || template?.cover_background

  // 模板列表还在路上时 template 必然是 undefined，与「真的没绑模板」长得一样。
  // 不区分的话，弹窗刚打开就点预览会收到一句错误的「还没绑定投稿模板」。
  const emptyTemplateHint = templatesLoading
    ? '投稿模板还在加载，请稍候再点一次'
    : template
      ? `所属模板「${template.template_name}」没有填「封面文字模板」，投稿不会生成自动封面`
      : '该主播还没有绑定投稿模板；封面文字模板取自模板，请先在「录播管理」里绑定'

  return (
    <Form.Section text="封面">
      <CoverBackgroundField
        style={{ width: '100%' }}
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
        extraText={
          <>
            <strong>覆盖</strong>所属上传模板的封面背景；留空则回退到模板的设置。
            仅在模板填了「封面文字模板」时生效。
          </>
        }
      />
      <Form.Slot label={{ text: '预览封面' }}>
        <CoverPreviewButton
          template={template?.cover_template}
          background={background}
          emptyTemplateHint={emptyTemplateHint}
        />
      </Form.Slot>
    </Form.Section>
  )
}

const OverrideModal: React.FC<TemplateModalProps> = ({ children, entity, onOk }) => {
  const [isOpen, setOpen] = useState(false)

  // 封面文字模板是模板级的设置，主播这边只覆盖背景。预览要用文字模板，
  // 所以得把所属模板捞出来——`upload_streamers_id` 为空则没有可预览的文字。
  const { data: templates, isLoading: templatesLoading } = useSWR<StudioEntity[]>(
    '/v1/upload/streamers',
    fetcher
  )
  const boundTemplate = templates?.find(item => item.id === entity?.upload_streamers_id)

  const toggle = () => {
    setOpen(!isOpen)
  }

  const platformSetting = () => {
    for (const [pattern, Plugin] of Object.entries(SupportedPlatforms)) {
      if (entity?.url.match(new RegExp(pattern))) {
        // console.log('匹配到平台:', pattern)
        return Plugin as React.ComponentType<PluginProps>
      }
    }
    // console.log('未匹配到平台')
    return null
  }

  const api = useRef<FormApi>()

  const { biliUsers } = useBiliUsers()
  const list = biliUsers?.map(item => {
    return {
      value: item.value,
      label: (
        <>
          <Avatar size="extra-small" src={item.face} />
          <span style={{ marginLeft: 8 }}>{item.name}</span>
        </>
      ),
    }
  })

  const [visible, setVisible] = useState(false)
  const showDialog = () => {
    setVisible(true)
  }
  const handleOk = async () => {
    let values = await api.current?.validate()
    const entityFields = new Set([
      'id',
      'url',
      'remark',
      'filename_prefix',
      'time_range',
      'upload_streamers_id',
      'status',
      'upload_status',
      'statusTag',
      'recording_quality',
      'format',
      'excluded_keywords',
      'preprocessor',
      'segment_processor',
      'downloaded_processor',
      'postprocessor',
      'opt_args',
      'override',
      // 它是 livestreamers 表上的真实列，不是覆写 JSON 里的一项。
      // 不列在这里的话，下面那圈循环会把它一并塞进 override，
      // 库里于是同时存在「列上的值」和「override 里的值」，投稿只认前者。
      'cover_background',
    ])

    if (values) {
      const baseValues: Record<string, any> = {
        id: entity?.id,
        url: entity?.url,
        remark: entity?.remark,
        filename_prefix: entity?.filename_prefix,
        time_range: serializeTimeRange(entity?.time_range),
        upload_streamers_id: entity?.upload_streamers_id ?? null,
        format: entity?.format,
        excluded_keywords: entity?.excluded_keywords,
        preprocessor: entity?.preprocessor,
        segment_processor: entity?.segment_processor,
        downloaded_processor: entity?.downloaded_processor,
        postprocessor: entity?.postprocessor,
        opt_args: entity?.opt_args,
        // 取表单当前值而非 entity——这一项是本弹窗里可编辑的，别的都不是。
        //
        // `?? ''` 不是防御性写法，是必需的：Semi 默认 allowEmpty=false，用户清空输入框后
        // 该键会从 values 里消失；而服务端有条守卫是「载荷里整项缺失就沿用库里的值」，
        // 直接透传 undefined 会被 filter 掉 → 守卫还原 → 字段永远清不掉。
        // 显式提交空字符串，服务端的解析侧会把空白当作「未配置」，正好回退到模板的背景。
        cover_background: values.cover_background ?? '',
      }
      const nextValues = Object.fromEntries(
        Object.entries(baseValues).filter(([, value]) => value !== undefined)
      )

      // 处理 override_text
      let textOverride: Record<string, any> = {}
      if (values.override_text) {
        try {
          textOverride = JSON.parse(values.override_text)
        } catch (e) {
          Notification.error({
            title: '错误',
            content: '配置格式不正确，请检查 JSON 格式',
          })
          return
        }
      }

      const overrideConfig: Record<string, any> = { ...textOverride }
      Object.keys(values).forEach(key => {
        if (key !== 'override_text' && !entityFields.has(key) && values[key] !== undefined) {
          overrideConfig[key] = values[key] === '' ? null : values[key]
        }
      })
      nextValues.override = overrideConfig

      // 处理循环引用
      const cleanValues = removeCircularReferences(nextValues)
      await onOk(cleanValues)
      setVisible(false)
      return
    }
    setVisible(false)
  }
  const handleCancel = () => {
    setVisible(false)
  }

  const childrenWithProps = React.Children.map(children, child => {
    if (React.isValidElement<any>(child)) {
      return React.cloneElement(child, {
        onClick: () => {
          showDialog()
          child.props.onClick?.()
        },
      })
    }
  })

  const downloadSettings = (
    <Collapse.Panel header="下载设置" itemKey="download">
      <div style={{ marginBottom: 12 }}>
        请到
        <a href="/dashboard" style={{ textDecoration: 'none', color: 'var(--semi-color-primary)' }}>
          空间配置
        </a>
        查看选项说明
      </div>
      <Form.Select
        label="下载插件（downloader）"
        field="downloader"
        placeholder="stream-gears（默认）"
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      >
        <Select.Option value="streamlink">streamlink（hls多线程下载）</Select.Option>
        <Select.Option value="ffmpeg">ffmpeg</Select.Option>
        <Select.Option value="stream-gears">stream-gears（默认）</Select.Option>
        <Select.Option value="sync-downloader">sync-downloader（边录边传）</Select.Option>
      </Form.Select>

      <Form.InputNumber
        label="视频分段大小（file_size）"
        field="file_size"
        placeholder=""
        suffix={'Byte'}
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      />

      <Form.Input
        field="segment_time"
        label="视频分段时长（segment_time）"
        placeholder="01:00:00"
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
        rules={[
          {
            pattern: /^[^：]*$/,
            message: '请使用英文冒号',
          },
          {
            pattern: /^[0-9:]*$/,
            message: '只接受数字和英文冒号',
          },
          {
            pattern: /^$|^[0-9]{2,4}:[0-5][0-9]:[0-5][0-9]$/,
            message: '分或秒不符合规范',
          },
        ]}
        stopValidateWithError={true}
      />

      <Form.InputNumber
        field="filtering_threshold"
        label="短片探测阈值（filtering_threshold）"
        suffix={'MB'}
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      />
      <Form.Switch
        field="preserve_recoverable_short_segments"
        label="保留有效短分段（preserve_recoverable_short_segments）"
        extraText="默认关闭；开启后保留通过媒体探测的有效短分段。"
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      />
      <Form.Switch
        field="route_health_enabled"
        label="拉流线路健康退避（route_health_enabled）"
        extraText="默认关闭；开启后使用独立的线路健康计数与退避。"
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      />
    </Collapse.Panel>
  )

  return (
    <>
      {childrenWithProps}
      <Modal
        title="配置覆写"
        visible={visible}
        onOk={handleOk}
        style={{ width: 'min(600px, 90vw)' }}
        onCancel={handleCancel}
        bodyStyle={{
          overflow: 'auto',
          maxHeight: 'calc(100vh - 320px)',
          paddingLeft: 10,
          paddingRight: 10,
        }}
      >
        <Form initValues={entity} getFormApi={formApi => (api.current = formApi)}>
          <Form.TextArea
            field="override_text"
            label="配置覆写"
            placeholder="请输入 JSON 格式的配置"
            style={{ marginBottom: 12 }}
            initValue={entity?.override ? JSON.stringify(entity.override, null, 2) : ''}
            rules={[
              { required: false },
              {
                validator: (rule, value) => {
                  if (!value) return true
                  try {
                    JSON.parse(value)
                    return true
                  } catch (e) {
                    return false
                  }
                },
                message: '请输入有效的 JSON 格式',
              },
            ]}
          />
          <CoverSection template={boundTemplate} templatesLoading={templatesLoading} />
          <Form.Section>
            <Collapse defaultActiveKey={['plugin']}>
              {downloadSettings}
              {(() => {
                const Plugin = platformSetting()
                return Plugin ? (
                  <Plugin entity={entity} list={list} initValues={entity?.override} />
                ) : null
              })()}
            </Collapse>
          </Form.Section>
        </Form>
      </Modal>
    </>
  )
}

export default OverrideModal

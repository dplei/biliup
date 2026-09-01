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
import AudioNormalizationControl from './AudioNormalizationControl'
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
 * 「下载设置」面板里取值型的字段。清空控件＝撤销这一条覆写，写回 `null`（patch 侧的
 * `None`，与后端返回的形态一致）。
 */
const DOWNLOAD_VALUE_FIELDS = [
  'downloader',
  'file_size',
  'segment_time',
  'filtering_threshold',
] as const

/**
 * 同一面板里的布尔字段。`Config` 上它们是裸 `bool`，patch 侧却是 `Option<bool>`——
 * 「跟随全局」是「键为 null」，和「覆写成关闭」完全两回事，Switch 的两态说不出这个区别：
 * 全局开着的项在弹窗里一律显示成关，用户点一下开再点回关，就从「跟随」变成了「强制关闭」，
 * 而界面上两者长得一模一样。所以这里用三选一的 Select。
 */
const DOWNLOAD_BOOL_FIELDS = ['preserve_recoverable_short_segments', 'route_health_enabled'] as const

/** 三态 Select 的「跟随全局」选项值。空串正好也是 Semi 清空后的值。 */
const INHERIT = ''

/** override JSON 里属于音量的键，整组一起写入或一起拿掉。 */
const AUDIO_OVERRIDE_FIELDS = [
  'audio_normalization_enabled',
  'audio_normalization_offset_db',
  'audio_normalization_disk_reserve_gib',
  'audio_normalization_keep_original',
] as const

/** 「为这个房间单独设置音量」——只在表单里存在，不是 config 的字段，不能进 override。 */
const AUDIO_OVERRIDE_TOGGLE = 'audio_override_enabled'

/**
 * 音量覆写区：外层开关表达「这个房间要不要单独设」，里面直接复用空间配置那套控件，
 * 两处界面完全一致。
 *
 * 需要这个外层开关，是因为全局那几项是裸 `bool`/数字，覆写侧却是 `Option`——「跟随全局」
 * 只能用「override 里没有这些键」来表达，光靠内层开关的 true/false 说不出这一态。
 */
const AudioOverrideSection: React.FC<{ fieldInitValues: Record<string, any> }> = ({
  fieldInitValues,
}) => {
  const { values } = useFormState()

  return (
    <>
      <Form.Switch
        field={AUDIO_OVERRIDE_TOGGLE}
        label="为这个房间单独设置音量"
        extraText="关闭＝跟随空间配置里的全局设置。打开后下面几项只影响这个房间，初值取自当前的全局设置。"
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      />
      {values[AUDIO_OVERRIDE_TOGGLE] && (
        <AudioNormalizationControl showSample={false} bordered={false} fieldInitValues={fieldInitValues} />
      )}
    </>
  )
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

  // 「跟随全局」时开关打开后要显示全局此刻的值，所以得把全局配置读进来。没读到就退回
  // 各字段的内置默认值——总比显示一个凭空捏造的数字强。
  const { data: globalConfig } = useSWR('/v1/configuration', fetcher)

  // 覆写值只存在于 entity.override 里，Form 的 initValues={entity} 够不着。取值型控件都得
  // 自己拿 initValue 回显——库里存着 50，输入框却空着，用户只能去顶部的 JSON 框里认。
  const overrideInit = (key: string) => (entity?.override as Record<string, any>)?.[key] ?? undefined
  const tristateInit = (key: string) => {
    const raw = (entity?.override as Record<string, any>)?.[key]
    return raw === true ? 'on' : raw === false ? 'off' : INHERIT
  }

  // 覆写值只存在于 entity.override 里，Form 的 initValues={entity} 够不着，得显式合进去。
  const audioOverride = (entity?.override ?? {}) as Record<string, any>
  const pickAudio = (key: string, fallback: any) =>
    audioOverride[key] ?? globalConfig?.[key] ?? fallback
  const audioInitValues = {
    // 只要 override 里已经存着任何一项，这个房间就是「单独设置」状态。
    [AUDIO_OVERRIDE_TOGGLE]: AUDIO_OVERRIDE_FIELDS.some(
      key => audioOverride[key] !== null && audioOverride[key] !== undefined
    ),
    audio_normalization_enabled: pickAudio('audio_normalization_enabled', false),
    audio_normalization_offset_db: pickAudio('audio_normalization_offset_db', 0),
    audio_normalization_disk_reserve_gib: pickAudio('audio_normalization_disk_reserve_gib', 5),
    audio_normalization_keep_original: pickAudio('audio_normalization_keep_original', false),
  }

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
  // 折叠面板没展开时字段压根不挂载，values 里留的是平台插件灌进去的库中原值。记下这一轮
  // 开过哪些面板，只让开过的面板接管自己的字段——否则「清空输入框」和「面板根本没打开」
  // 在提交时长得一模一样，想撤销一条覆写就永远做不到。
  const [openedPanels, setOpenedPanels] = useState<string[]>([])
  const showDialog = () => {
    setOpenedPanels([])
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
      // 表单自己的状态位，不是 config 的字段。
      AUDIO_OVERRIDE_TOGGLE,
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
      // 「下载设置」展开过，这一组就由控件说了算：控件的初值来自 override 本身，不改则等值
      // 写回，清空则写 null 撤销覆写。没展开过就一项都不碰——那时 values 里是平台插件灌进来的
      // 库中原值，接管它等于把「没打开过的面板」也当成用户表达，清空与未展开还分不开。
      if (openedPanels.includes('download')) {
        DOWNLOAD_VALUE_FIELDS.forEach(key => {
          const raw = values[key]
          overrideConfig[key] = raw === undefined || raw === '' ? null : raw
        })
        DOWNLOAD_BOOL_FIELDS.forEach(key => {
          const raw = values[key]
          // 库里的原值是 boolean，Select 给出的是字符串，两种都要认。
          if (raw === 'on' || raw === true) overrideConfig[key] = true
          else if (raw === 'off' || raw === false) overrideConfig[key] = false
          else overrideConfig[key] = null
        })
      }

      // 音量三项由控件独占：只要「音量设置」面板被展开过，控件当前值就是权威，顶部 JSON
      // 文本框里的同名旧值一律让位。不这么做的话用户从界面上撤销不掉一条已有的覆写——
      // 控件回显「跟随全局」，提交后 textOverride 里的旧值又被原样写了回去。
      //
      // 反过来，面板没展开时 Semi 压根不挂载这些字段（values 里没有这些键），那就一个都不能碰，
      // 否则点一次「确定」就把用户配好的音量覆写洗掉了。
      // 音量这一组由「为这个房间单独设置音量」独占：关掉就意味着整组从 override 里拿走
      // （= 跟随全局）。面板没展开时 Semi 不挂载这些字段，values 里连开关都没有，那就一项
      // 都不能碰——否则「打开弹窗改个别的再确定」会顺手洗掉用户配好的音量覆写。
      if (Object.prototype.hasOwnProperty.call(values, AUDIO_OVERRIDE_TOGGLE)) {
        AUDIO_OVERRIDE_FIELDS.forEach(key => delete overrideConfig[key])
        if (values[AUDIO_OVERRIDE_TOGGLE]) {
          // 与全局同值、而且本来也没覆写过的项就不写进去。控件的初值取自全局设置，用户没动过
          // 的那几项照单写下来只会把它们钉死在此刻的全局值上——生效结果一样，却从此不再跟随
          // 全局调整。override 保持最小，才对得上「只覆写需要单独设的项」。
          const write = (key: string, value: any) => {
            const hadOverride = audioOverride[key] !== null && audioOverride[key] !== undefined
            if (!hadOverride && value === globalConfig?.[key]) return
            overrideConfig[key] = value
          }

          const enabled = Boolean(values.audio_normalization_enabled)
          write('audio_normalization_enabled', enabled)
          // 关闭时后面几项在界面上根本不显示，也就没有用户表达过的值可写，只留开关本身。
          if (enabled) {
            // 只认真正的数字。空输入框在 Semi 里是空字符串，`Number('')` 却是 0——照着写下去
            // 会把磁盘保留线悄悄覆写成 1 GiB，比不写危险得多。
            const offset = values.audio_normalization_offset_db
            if (typeof offset === 'number' && Number.isFinite(offset)) {
              // 后端 `effective_audio_target_lufs` 同样 clamp 在 -6..=4，这里先夹一次是为了让
              // 库里存的就是生效值，排查时不必再心算一遍。
              write('audio_normalization_offset_db', Math.max(-6, Math.min(4, Math.round(offset))))
            }
            const reserve = values.audio_normalization_disk_reserve_gib
            if (typeof reserve === 'number' && Number.isFinite(reserve)) {
              write('audio_normalization_disk_reserve_gib', Math.max(1, Math.min(1024, Math.round(reserve))))
            }
            write('audio_normalization_keep_original', Boolean(values.audio_normalization_keep_original))
          }
        }
      }

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
        initValue={overrideInit('downloader')}
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
        initValue={overrideInit('file_size')}
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
        initValue={overrideInit('segment_time')}
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
        initValue={overrideInit('filtering_threshold')}
        suffix={'MB'}
        style={{ width: '100%' }}
        fieldStyle={{
          alignSelf: 'stretch',
          padding: 0,
        }}
        showClear={true}
      />
      <Form.Select
        field="preserve_recoverable_short_segments"
        label="保留有效短分段（preserve_recoverable_short_segments）"
        initValue={tristateInit('preserve_recoverable_short_segments')}
        extraText="开启后保留通过媒体探测的有效短分段。"
        style={{ width: '100%' }}
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      >
        <Select.Option value={INHERIT}>跟随全局设置</Select.Option>
        <Select.Option value="on">强制开启</Select.Option>
        <Select.Option value="off">强制关闭</Select.Option>
      </Form.Select>
      <Form.Select
        field="route_health_enabled"
        label="拉流线路健康退避（route_health_enabled）"
        initValue={tristateInit('route_health_enabled')}
        extraText="开启后使用独立的线路健康计数与退避。"
        style={{ width: '100%' }}
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      >
        <Select.Option value={INHERIT}>跟随全局设置</Select.Option>
        <Select.Option value="on">强制开启</Select.Option>
        <Select.Option value="off">强制关闭</Select.Option>
      </Form.Select>
    </Collapse.Panel>
  )

  const audioSettings = (
    <Collapse.Panel header="音量设置" itemKey="audio">
      <AudioOverrideSection fieldInitValues={audioInitValues} />
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
        {/*
          `key` 让每次打开都重建表单。Semi 在弹窗关闭后并不卸载已经渲染过的 Collapse 面板，
          而 initValues 只在字段真正首次挂载时生效——不重建的话，展开过音量面板的用户第二次
          打开会看到一片空白，库里明明存着覆写。顺带也清掉了上次取消时留下的未提交编辑。
          全局配置是异步到的，它落地后同样要重建一次，否则「跟随全局」的初值会停在兜底值。
        */}
        <Form
          key={visible ? `open-${entity?.id ?? 'new'}-${globalConfig ? 'cfg' : 'nocfg'}` : 'closed'}
          initValues={{ ...entity, ...audioInitValues }}
          getFormApi={formApi => (api.current = formApi)}
        >
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
            <Collapse
              defaultActiveKey={['plugin']}
              onChange={activeKey => {
                const keys = (Array.isArray(activeKey) ? activeKey : [activeKey]).filter(
                  Boolean
                ) as string[]
                setOpenedPanels(prev => Array.from(new Set([...prev, ...keys])))
              }}
            >
              {downloadSettings}
              {audioSettings}
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

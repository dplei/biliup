import React from 'react'
import { Button, Form, Notification, Upload, useFormApi } from '@douyinfe/semi-ui'
import { IconUpload } from '@douyinfe/semi-icons'
import { API_BASE } from '../lib/api-streamer'

/** 背景图体积上限，与服务端的 MAX_UPLOAD_BYTES 一致；超限在选择时就拦下，省一次白跑的请求。 */
export const BACKGROUND_MAX_MB = 10

type CoverBackgroundFieldProps = {
  /** 表单字段名。模板级与主播级都叫 cover_background，只是落在不同的表上。 */
  field?: string
  label?: string
  extraText?: React.ReactNode
  placeholder?: string
  /** 输入框宽度等样式，两个页面的表单排版不同 */
  style?: React.CSSProperties
  fieldStyle?: React.CSSProperties
}

/**
 * 「封面背景图」输入框 + 上传按钮。
 *
 * 抽成组件是因为模板页与主播页要的是同一套控件、同一个上传接口，只有文案和排版不同。
 * 各写一份的话，哪天上传接口或体积上限变了，改漏一处就成了两种行为。
 *
 * 上传成功后把文件名填回输入框——库里存的是文件名，路径由服务端拼接。
 */
const CoverBackgroundField: React.FC<CoverBackgroundFieldProps> = ({
  field = 'cover_background',
  label = '封面背景图',
  extraText,
  placeholder = '留空为纯黑底；示例：aurora.jpg',
  style,
  fieldStyle,
}) => {
  const formApi = useFormApi()

  return (
    <>
      <Form.Input
        field={field}
        label={label}
        style={style}
        fieldStyle={fieldStyle}
        placeholder={placeholder}
        extraText={extraText}
        showClear
      />
      <Form.Slot label={{ text: '上传背景图' }}>
        <Upload
          action={`${API_BASE}/v1/cover-backgrounds`}
          name="file"
          accept=".jpg,.jpeg,.png,.webp"
          limit={1}
          // 文件名上传成功后就填进上面的输入框了，再挂一份文件列表只是重复信息
          showUploadList={false}
          // Semi 的 maxSize 单位是 KB
          maxSize={BACKGROUND_MAX_MB * 1024}
          onSuccess={(response: { file_name?: string }) => {
            if (!response?.file_name) return
            formApi.setValue(field, response.file_name)
            Notification.success({
              title: '背景图已上传',
              content: `已填入「${response.file_name}」，保存后生效`,
              position: 'top',
              duration: 3,
            })
          }}
          onError={() => {
            Notification.error({
              title: '背景图上传失败',
              content: `请确认是 jpg / png / webp 图片且不超过 ${BACKGROUND_MAX_MB} MB`,
              position: 'top',
              duration: 5,
            })
          }}
          onSizeError={() => {
            Notification.error({
              title: '背景图过大',
              content: `单张背景图不能超过 ${BACKGROUND_MAX_MB} MB`,
              position: 'top',
              duration: 5,
            })
          }}
        >
          <Button icon={<IconUpload />}>选择图片上传</Button>
        </Upload>
      </Form.Slot>
    </>
  )
}

export default CoverBackgroundField

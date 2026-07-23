import React, { useEffect, useRef, useState } from 'react'
import { Button, Modal, Notification, Typography } from '@douyinfe/semi-ui'
import { IconEyeOpened } from '@douyinfe/semi-icons'
import { proxy } from '../lib/api-streamer'

const { Text } = Typography

type CoverPreviewButtonProps = {
  /** 封面文字模板原文，占位符尚未展开 */
  template?: string
  /** 背景图文件名，留空为纯黑底 */
  background?: string
  /**
   * 模板为空时的提示语。主播页的模板来自所属上传模板而非当前表单，
   * 照搬模板页那句话会把用户支使到错误的地方去改。
   */
  emptyTemplateHint?: string
}

/**
 * 「预览封面」按钮：点一下向服务端要一张成品封面看看。
 *
 * 服务端用的是与投稿时同一个渲染函数，所以这里看着好看就代表实际产出好看——
 * 不必等下一次下播走完整条上传流程才发现排版有问题。
 *
 * 只在点击时请求，**不做输入防抖式的自动重渲**：每敲一个键就让服务端编码一张 JPG，
 * 代价与收益完全不成比例。
 *
 * 做成独立组件是为了主播页能原样复用——预览接口不读数据库，
 * 两边各把自己那一级的值传进来即可。
 */
const CoverPreviewButton: React.FC<CoverPreviewButtonProps> = ({
  template,
  background,
  emptyTemplateHint = '「封面文字模板」留空时不会生成自动封面，投稿用的是上方的「视频封面」',
}) => {
  const [visible, setVisible] = useState(false)
  const [loading, setLoading] = useState(false)
  const [imageUrl, setImageUrl] = useState<string>()

  // object URL 得手动释放，否则每预览一次就漏掉一张图的内存。
  // 用 ref 而不是从 state 里读旧值：释放是副作用，不该写在 setState 的 updater 里。
  const imageUrlRef = useRef<string>()

  const showImage = (url?: string) => {
    if (imageUrlRef.current) URL.revokeObjectURL(imageUrlRef.current)
    imageUrlRef.current = url
    setImageUrl(url)
  }

  useEffect(
    () => () => {
      if (imageUrlRef.current) URL.revokeObjectURL(imageUrlRef.current)
    },
    []
  )

  const handlePreview = async () => {
    // 模板为空时投稿根本不会生成自动封面，这时候给一张图反而是误导。
    if (!template?.trim()) {
      Notification.warning({
        title: '还没有封面文字模板',
        content: emptyTemplateHint,
        position: 'top',
        duration: 5,
      })
      return
    }

    setLoading(true)
    try {
      const params = new URLSearchParams({ template })
      if (background?.trim()) params.set('background', background.trim())

      const response = await proxy(`/v1/cover-preview?${params}`)
      showImage(URL.createObjectURL(await response.blob()))
      setVisible(true)
    } catch (e) {
      // 服务端对参数问题会回一句中文，直接透出来比「预览失败」有用得多。
      Notification.error({
        title: '预览失败',
        content: e instanceof Error ? e.message : '请稍后再试',
        position: 'top',
        duration: 5,
      })
    } finally {
      setLoading(false)
    }
  }

  const handleClose = () => {
    setVisible(false)
    showImage(undefined)
  }

  return (
    <>
      <Button icon={<IconEyeOpened />} loading={loading} onClick={handlePreview}>
        预览封面
      </Button>
      <Modal
        title="封面预览"
        visible={visible}
        onCancel={handleClose}
        footer={null}
        style={{ width: 'min(720px, 92vw)' }}
      >
        {imageUrl && (
          // 用原生 img 而非 next/image：图是运行时拿到的 blob object URL，
          // 何况本项目是静态导出且已 `images.unoptimized`，next/image 在这里没有任何作用。
          // eslint-disable-next-line @next/next/no-img-element
          <img
            src={imageUrl}
            alt="封面预览"
            style={{ width: '100%', display: 'block', borderRadius: 4 }}
          />
        )}
        <Text type="tertiary" size="small" style={{ display: 'block', marginTop: 8 }}>
          主播名与直播标题用的是示例值，时间取当前时刻；渲染器与投稿时完全相同。
        </Text>
      </Modal>
    </>
  )
}

export default CoverPreviewButton

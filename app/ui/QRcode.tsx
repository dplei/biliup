import React, { useEffect, useState } from 'react'
import { fetcher, sendRequest, proxy } from '@/app/lib/api-streamer'
import { QRCodeSVG } from 'qrcode.react'
import { Notification, Spin, Typography } from '@douyinfe/semi-ui'

type QrcodeProps = {
  onSuccess: (e: string) => void
}

const Qrcode: React.FC<QrcodeProps> = ({ onSuccess }) => {
  const [url, setUrl] = useState('')
  useEffect(() => {
    // Create an instance.
    const controller = new AbortController()
    const signal = controller.signal
    // Register a listenr.
    signal.addEventListener('abort', () => {
      console.log('aborted!')
    })
      ; (async () => {
        let qrData = await fetcher('/v1/get_qrcode', undefined)
        setUrl(qrData['data']['url'])
        console.log(qrData['data']['url'])
        let res = await proxy('/v1/login_by_qrcode', {
          method: 'POST',
          headers: {
            'Content-Type': 'application/json',
          },
          body: JSON.stringify(qrData),
          signal: signal,
        })
        const data = await res.json()
        onSuccess(data['filename'])
      })().catch(e => {
        // 已 abort 说明组件卸载了（dev 下 StrictMode 会 mount→unmount→mount，
        // 或用户直接离开登录页），不该再动 state。
        //
        // 这一判还挡着一个当场崩溃：`abort(reason)` 带 reason 时，fetch 就 reject 成
        // **那个 reason 本身**（不带才是 AbortError）。下面传的是字符串 'qrcode exit'，
        // 于是 e.message 为 undefined，setUrl(undefined) 会让 url.startsWith 直接抛。
        if (signal.aborted) return

        console.log(e)
        // 非 Error 的 reject 值同样要兜住，否则又是一个 undefined 写进 url。
        const message = e instanceof Error ? e.message : String(e)
        setUrl(message)
        Notification.error({
          title: 'QRcode',
          content: <Typography.Paragraph style={{ maxWidth: 450 }}>{message}</Typography.Paragraph>,
          style: { width: 'min-content' },
        })
      })

    return () => {
      controller.abort('qrcode exit')
    }
  }, [onSuccess])
  if (url === '') {
    return <Spin />
  }

  if (!url.startsWith("http")) {
    return <> {url} </>
  }
  return (
    <div
      style={{
        marginTop: 30,
        marginLeft: 'auto',
        marginRight: 'auto',
        width: 'max-content',
      }}
    >
      <QRCodeSVG value={url} />
    </div>
  )
}

export default Qrcode

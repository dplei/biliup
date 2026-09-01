'use client'

import React, { useCallback, useEffect, useRef, useState } from 'react'
import { Button, Form, Space, Toast, useFormApi, useFormState } from '@douyinfe/semi-ui'
import { API_BASE } from '../lib/api-streamer'

type SampleStatus = {
  sample_ready: boolean
  capture_pending: boolean
  updated_at?: string
  size_bytes?: number
}

const STATUS_URL = '/v1/audio-normalization/sample/status'
const SAMPLE_URL = '/v1/audio-normalization/sample'

type AudioNormalizationControlProps = {
  /**
   * 样片是全局唯一一份，「更新 / 删除样片」也都是全局操作。主播级的覆写弹窗里要关掉，
   * 否则那两个按钮看起来像是只作用于当前房间，点下去却影响所有人。
   */
  showSample?: boolean
  /** 嵌在别的容器（如覆写弹窗的折叠面板）里时去掉外框，避免套两层边框。 */
  bordered?: boolean
  /**
   * 字段初值，按 field 名索引。
   *
   * 折叠面板展开时字段才挂载，而 Semi 的 InputNumber 挂载后不会回头去取表单里已有的值，
   * 只能显式喂给它——空间配置页里字段跟着表单一起挂载，所以那边不用传。Switch 和音量推子
   * 都是受控的，每次渲染都跟着 form value 走，不受这个问题影响。
   */
  fieldInitValues?: Record<string, any>
}

export default function AudioNormalizationControl({
  showSample = true,
  bordered = true,
  fieldInitValues,
}: AudioNormalizationControlProps = {}) {
  const formApi = useFormApi()
  const { values } = useFormState()
  const enabled = Boolean(values.audio_normalization_enabled)
  const offset = Math.max(-6, Math.min(4, Number(values.audio_normalization_offset_db ?? 0)))
  const [status, setStatus] = useState<SampleStatus | null>(null)
  const [busy, setBusy] = useState(false)
  const audioRef = useRef<HTMLAudioElement>(null)
  const contextRef = useRef<AudioContext | null>(null)
  const sourceRef = useRef<MediaElementAudioSourceNode | null>(null)
  const gainRef = useRef<GainNode | null>(null)

  const refresh = useCallback(async () => {
    try {
      const response = await fetch(API_BASE + STATUS_URL, { cache: 'no-store' })
      if (!response.ok) throw new Error(`HTTP ${response.status}`)
      setStatus(await response.json())
    } catch (error) {
      console.error('读取响度样片状态失败', error)
    }
  }, [])

  useEffect(() => { void refresh() }, [refresh])
  useEffect(() => {
    if (!status?.capture_pending) return
    const timer = window.setInterval(() => void refresh(), 5000)
    return () => window.clearInterval(timer)
  }, [status?.capture_pending, refresh])

  useEffect(() => {
    const gain = gainRef.current
    const context = contextRef.current
    if (!gain || !context) return
    gain.gain.setTargetAtTime(Math.pow(10, offset / 20), context.currentTime, 0.025)
  }, [offset])

  useEffect(() => () => {
    sourceRef.current?.disconnect()
    gainRef.current?.disconnect()
    void contextRef.current?.close()
  }, [])

  const prepareAudio = async () => {
    const element = audioRef.current
    if (!element) return
    if (!contextRef.current) {
      const context = new AudioContext()
      const source = context.createMediaElementSource(element)
      const gain = context.createGain()
      gain.gain.value = Math.pow(10, offset / 20)
      source.connect(gain).connect(context.destination)
      contextRef.current = context
      sourceRef.current = source
      gainRef.current = gain
    }
    if (contextRef.current.state === 'suspended') await contextRef.current.resume()
  }

  const request = async (method: 'POST' | 'DELETE', suffix = '') => {
    setBusy(true)
    try {
      const response = await fetch(API_BASE + SAMPLE_URL + suffix, { method })
      if (!response.ok) throw new Error(await response.text())
      await refresh()
    } catch (error) {
      Toast.error(`样片操作失败：${error instanceof Error ? error.message : String(error)}`)
    } finally {
      setBusy(false)
    }
  }

  const sampleMessage = status?.capture_pending
    ? status.sample_ready
      ? '正在等待新样片；当前仍播放旧样片。'
      : '等待下一段完整录像……'
    : status?.sample_ready
      ? '播放样片并上下拖动试听。'
      : '还没有样片，可从下一段录像自动截取。'

  const cacheBuster = encodeURIComponent(status?.updated_at ?? 'current')

  return (
    <div
      style={
        bordered
          ? { border: '1px solid var(--semi-color-border)', borderRadius: 8, padding: 16, marginBottom: 20 }
          : undefined
      }
    >
      <Form.Switch
        field="audio_normalization_enabled"
        label="自动增强录音音量"
        extraText="自动统一后续录像的听感音量。视频不重编码，音频会重新编码；失败时自动上传原片。"
        fieldStyle={{ alignSelf: 'stretch', padding: 0 }}
      />

      {enabled && (
        <Form.InputNumber
          field="audio_normalization_disk_reserve_gib"
          label="磁盘保留线"
          initValue={fieldInitValues?.audio_normalization_disk_reserve_gib}
          suffix="GiB"
          min={1}
          max={1024}
          extraText="可用空间放不下一份处理结果时跳过本段的音量处理、直接上传原片；处理途中磁盘跌破这条线也会立即中止并回退。录制与上传不受影响。"
          style={{ width: '100%' }}
          fieldStyle={{ alignSelf: 'stretch', padding: 0, marginTop: 12 }}
        />
      )}

      {enabled && (
        <Form.Switch
          field="audio_normalization_keep_original"
          label="保留原始录像"
          extraText="默认关闭：标准化结果直接替换原片，磁盘上每段只留一份。开启后额外保留一份未处理的原片，磁盘占用翻倍；后处理脚本收到的也会变回原片。"
          fieldStyle={{ alignSelf: 'stretch', padding: 0, marginTop: 12 }}
        />
      )}

      {enabled && (
        <div
          style={{
            display: 'flex',
            gap: 28,
            alignItems: 'center',
            flexWrap: 'wrap',
            marginTop: 16,
            // 没有样片那一栏时推子会孤零零地贴在左边，居中才不显得是残缺的两栏布局。
            justifyContent: showSample ? undefined : 'center',
          }}
        >
          <div style={{ minWidth: 130, textAlign: 'center' }}>
            <div style={{ fontSize: 13, marginBottom: 8 }}>更响</div>
            <input
              aria-label="样片音量偏移"
              aria-orientation="vertical"
              type="range"
              min={-6}
              max={4}
              step={1}
              value={offset}
              onChange={(event) => formApi.setValue('audio_normalization_offset_db', Number(event.target.value))}
              style={{ writingMode: 'vertical-lr', direction: 'rtl', width: 34, height: 180, accentColor: offset >= 4 ? '#f59e0b' : offset >= 3 ? '#eab308' : '#22a06b' }}
            />
            <div style={{ fontSize: 13, marginTop: 8 }}>更轻</div>
            <div style={{ marginTop: 8, fontWeight: 600 }}>{offset === 0 ? '推荐' : `推荐 ${offset > 0 ? '+' : ''}${offset} dB`}</div>
            <Button theme="borderless" size="small" onClick={() => formApi.setValue('audio_normalization_offset_db', 0)}>
              恢复推荐音量
            </Button>
          </div>

          {showSample && (
          <div style={{ flex: 1, minWidth: 280 }}>
            <p style={{ marginTop: 0 }}>{sampleMessage}</p>
            {status?.sample_ready && (
              <audio
                ref={audioRef}
                crossOrigin="anonymous"
                controls
                preload="metadata"
                src={`${API_BASE}${SAMPLE_URL}?v=${cacheBuster}`}
                onPlay={() => void prepareAudio()}
                style={{ width: '100%', marginBottom: 12 }}
              />
            )}
            <Space wrap>
              {!status?.capture_pending ? (
                <Button loading={busy} onClick={() => void request('POST', '/capture')}>从下一段录像更新样片</Button>
              ) : (
                <Button loading={busy} onClick={() => void request('DELETE', '/capture')}>取消等待</Button>
              )}
              {status?.sample_ready && (
                <Button type="danger" theme="borderless" loading={busy} onClick={() => void request('DELETE')}>
                  删除样片
                </Button>
              )}
            </Space>
            <p style={{ color: 'var(--semi-color-text-2)', fontSize: 13, marginBottom: 0 }}>
              样片只用于试听；删除样片不会关闭音量增强。处理时会暂时增加磁盘占用。
            </p>
          </div>
          )}
        </div>
      )}
    </div>
  )
}

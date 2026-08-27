'use client'

import React, { useState } from 'react'
import { Button, Tooltip } from '@douyinfe/semi-ui'
import { IconCalendarClockStroked } from '@douyinfe/semi-icons'
import { LiveStreamerEntity } from '@/app/lib/api-streamer'
import RecordingLeaseModal from './RecordingLeaseModal'

export default function RecordingLeaseButton({ streamer }: { streamer: LiveStreamerEntity }) {
  const [visible, setVisible] = useState(false)
  return (
    <>
      <Tooltip content="录制期限">
        <Button
          theme="borderless"
          icon={<IconCalendarClockStroked />}
          aria-label="录制期限"
          onClick={() => setVisible(true)}
        />
      </Tooltip>
      <RecordingLeaseModal streamer={streamer} visible={visible} onClose={() => setVisible(false)} />
    </>
  )
}

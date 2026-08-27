import React, { useState } from 'react';
import { useSWRConfig } from 'swr';
import { Button, Notification, Tooltip } from '@douyinfe/semi-ui';
import { IconPulse } from '@douyinfe/semi-icons';
import { checkStreamNow, LiveStreamerEntity } from '@/app/lib/api-streamer';

interface CheckStreamButtonProps {
    streamer: LiveStreamerEntity;
}

/**
 * 主动检查一次直播流。
 *
 * 轮询是所有房间排队、每轮睡一个间隔，服务意外重启后要绕完一圈才轮得到某个房间；
 * 这个按钮把那一次检查提前，开播的话后端当场接上录制。
 */
export const CheckStreamButton: React.FC<CheckStreamButtonProps> = ({ streamer }) => {
    const { mutate } = useSWRConfig();
    const [loading, setLoading] = useState(false);

    const handleCheck = async () => {
        setLoading(true);
        try {
            const res = await checkStreamNow(streamer.id);
            const notify = res.outcome === 'started' ? Notification.success : Notification.info;
            notify({ title: streamer.remark, content: res.message, duration: 4 });
            // 无论开播与否都刷新一次：状态标签、画质标签都来自这个列表。
            await mutate('/v1/streamers');
        } catch (error) {
            Notification.error({
                title: '检查直播流失败',
                content: (error as Error).message,
            });
        } finally {
            setLoading(false);
        }
    };

    return (
        <Tooltip content="立即检查直播流（不等轮询）">
            <Button
                onClick={handleCheck}
                loading={loading}
                icon={<IconPulse />}
                theme="borderless"
                aria-label="立即检查直播流"
            />
        </Tooltip>
    );
};

export default CheckStreamButton;

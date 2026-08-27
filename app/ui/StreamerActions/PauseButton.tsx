import React from 'react';
import { useSWRConfig } from 'swr';
import {Button, Notification, Tooltip} from "@douyinfe/semi-ui";
import {IconPause, IconPlay} from "@douyinfe/semi-icons";
import {LiveStreamerEntity, setRecordingState} from "@/app/lib/api-streamer";

interface PauseButtonProps {
    streamer: LiveStreamerEntity;
    onSuccess?: () => void;
    onError?: (error: Error) => void;
}

export const PauseButton: React.FC<PauseButtonProps> = ({
                                                            streamer,
                                                            onSuccess,
                                                            onError
                                                        }) => {
    const { mutate } = useSWRConfig();

    const handlePause = async () => {
        try {
            await setRecordingState(streamer.id, streamer.status !== 'Pause');
            // 重新加载列表数据
            await mutate('/v1/streamers');
            onSuccess?.();
        } catch (error) {
            console.error('暂停失败:', error);
            Notification.error({
                title: '录制状态更新失败',
                content: (error as Error).message,
            });
            onError?.(error as Error);
        }
    };

    const leaseExpired = streamer.recording_lease?.state === 'expired_paused';
    const isResume = streamer.status === 'Pause';
    const disabled = leaseExpired && isResume;

    return (
        <Tooltip content={disabled ? '录制期限已到期，请先延期或清除期限' : isResume ? '恢复录制' : '暂停录制'}>
            <Button disabled={disabled} onClick={handlePause} icon={isResume ? <IconPlay />: <IconPause />} theme="borderless" aria-label={isResume ? '恢复录制' : '暂停录制'} />
        </Tooltip>
    );
};

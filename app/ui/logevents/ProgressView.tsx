'use client';
import Link from 'next/link';
import useSWR from 'swr';
import { Button, Card, Empty, Progress, Tag, Typography } from '@douyinfe/semi-ui';
import { IconArrowRight } from '@douyinfe/semi-icons';
import { fetcher } from '../../lib/api-streamer';
import { AssocField, formatBytes, formatMs, relativeText } from '../../lib/log-events';
import styles from './ProgressView.module.css';

/** 超过这个时间没有新进度就标成过期快照，不当作「仍在运行」。 */
const STALE_MS = 120_000;

interface StatusRoom {
	downloader_status: string;
	uploader_status: string;
	live_streamer: { id: number; remark?: string; url?: string } | null;
}

interface StatusResponse {
	rooms: StatusRoom[];
}

interface MissingRow {
	id: number;
	live_streamer_id: number;
	streamer_info_id: number;
	upload_session_id: number | null;
	file_path: string;
	segment_order: number;
	status: string;
	attempts: number;
	total_bytes: number | null;
	uploaded_bytes: number;
	current_line: string | null;
	attempt_phase: string | null;
	last_progress_at: string | null;
	updated_at: string;
	last_error: string | null;
}

interface PendingSession {
	id: number;
	streamer_info_id: number;
	streamer_name: string;
	stream_title: string;
	action: string;
	action_message: string;
	submit_state: string | null;
	last_submit_error: string | null;
	completeness: { total_expected: number; succeeded: number; pending: number; failed: number };
}

const PHASE_TEXT: Record<string, string> = {
	preprocessing: '本地预处理',
	queued: '等待上传许可',
	transferring: '正在传输',
};

/** `/v1/status` 给的是 Rust 调试输出，可能带 `Ok(...)` 外壳，取里面的状态名。 */
function workerState(raw: string | undefined): string {
	const match = /([A-Za-z]+)\)*$/.exec((raw ?? '').trim());
	return match ? match[1] : '';
}

const WORKER_TEXT: Record<string, string> = {
	Working: '进行中',
	Pending: '等待中',
	Idle: '空闲',
	Pause: '已暂停',
};

// 这张账本同时记录正常分段和补传，所以状态文案保持中性，不把首次上传说成补传。
const STATUS_TEXT: Record<string, string> = {
	pending: '等待上传',
	uploading: '上传中',
	failed: '上传失败',
	source_missing: '源文件缺失',
	succeeded: '已完成',
};

/** 只有真正失败的行才用危险色；`last_error` 在正常流程里也会存进度说明。 */
const FAILED_STATUS = ['failed', 'source_missing'];

interface Props {
	/** 跳到事件视图；failuresOnly 时只留下警告和错误。 */
	onJump: (key: AssocField, value: string, failuresOnly: boolean) => void;
	/** 关联筛选需要运行实例；未知时把跳转按钮禁用并说明原因，而不是给一个查不出东西的链接。 */
	instanceId: string;
}

function RecoveryLink() {
	return (
		<Link href="/missing" className={styles.recoveryLink}>
			去补传处理
			<IconArrowRight size="small" aria-hidden="true" />
		</Link>
	);
}

/**
 * 运行进度：直接复用已有业务快照（房间状态、补传 attempt、待投稿会话），日志页不再自己
 * 维护一份上传状态机。没有明确总量的阶段只显示阶段名，不画假百分比；恢复/取消这类操作
 * 一律回到既有的管理页。
 */
export default function ProgressView({ onJump, instanceId }: Props) {
	const now = Date.now();
	const { data: status } = useSWR<StatusResponse>('/v1/status', fetcher, { refreshInterval: 5000 });
	const { data: missing } = useSWR<MissingRow[]>('/v1/uploads/missing?status=active', fetcher, {
		refreshInterval: 5000,
	});
	const { data: sessions } = useSWR<PendingSession[]>('/v1/uploads/sessions/pending', fetcher, {
		refreshInterval: 5000,
	});

	const recording = (status?.rooms ?? []).filter((room) =>
		['Working', 'Pending'].includes(workerState(room.downloader_status)),
	);

	const jump = (key: AssocField, value: string, failuresOnly: boolean) => (
		<Button
			size="small"
			theme="borderless"
			disabled={!instanceId}
			onClick={() => onJump(key, value, failuresOnly)}
		>
			{failuresOnly ? '查看失败原因' : '查看本场事件'}
		</Button>
	);

	return (
		<div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
			{!instanceId ? (
				<Typography.Text type="tertiary" size="small">
					还没有读到任何事件，暂时无法从这里跳转到对应的事件范围。
				</Typography.Text>
			) : null}

			<Card title="录制中" bodyStyle={{ padding: 12 }}>
				{recording.length === 0 ? (
					<Empty description="当前没有正在录制的直播间" />
				) : (
					recording.map((room) => (
						<div
							key={room.live_streamer?.id ?? Math.random()}
							style={{
								display: 'flex',
								flexWrap: 'wrap',
								alignItems: 'center',
								gap: 8,
								padding: '6px 0',
								borderBottom: '1px solid var(--semi-color-border)',
							}}
						>
							<Typography.Text strong>
								{room.live_streamer?.remark || room.live_streamer?.url || '未命名直播间'}
							</Typography.Text>
							<Tag size="small" color="green">
								录制 {WORKER_TEXT[workerState(room.downloader_status)] ?? room.downloader_status}
							</Tag>
							<Tag size="small" color="grey">
								上传 {WORKER_TEXT[workerState(room.uploader_status)] ?? room.uploader_status}
							</Tag>
							<Typography.Text type="tertiary" size="small">
								录制没有已知总时长，这里只显示阶段
							</Typography.Text>
							<span style={{ marginLeft: 'auto' }}>
								{room.live_streamer
									? jump('live_streamer_id', String(room.live_streamer.id), false)
									: null}
							</span>
						</div>
					))
				)}
			</Card>

			<Card
				title="上传与补传"
				headerExtraContent={<RecoveryLink />}
				bodyStyle={{ padding: 12 }}
			>
				{(missing?.length ?? 0) === 0 ? (
					<Empty description="没有进行中的补传分段" />
				) : (
					missing!.map((row) => {
						const updatedAt = new Date(row.last_progress_at ?? row.updated_at).getTime();
						const stale = Number.isFinite(updatedAt) && now - updatedAt > STALE_MS;
						const total = row.total_bytes ?? 0;
						return (
							<div
								key={row.id}
								style={{ padding: '8px 0', borderBottom: '1px solid var(--semi-color-border)' }}
							>
								<div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'center' }}>
									<Typography.Text strong>第 {row.segment_order} 段</Typography.Text>
									<Tag size="small" color={row.status === 'failed' ? 'red' : 'blue'}>
										{STATUS_TEXT[row.status] ?? row.status}
									</Tag>
									{row.attempt_phase ? (
										<Tag size="small" color="grey">
											{PHASE_TEXT[row.attempt_phase] ?? row.attempt_phase}
										</Tag>
									) : null}
									{row.current_line ? (
										<Typography.Text type="tertiary" size="small">
											线路 {row.current_line}
										</Typography.Text>
									) : null}
									<span style={{ marginLeft: 'auto', display: 'flex', gap: 4 }}>
										{jump('streamer_info_id', String(row.streamer_info_id), false)}
										{jump('missing_id', String(row.id), true)}
									</span>
								</div>
								<div style={{ marginTop: 4, display: 'flex', flexWrap: 'wrap', gap: 12 }}>
									{total > 0 ? (
										<div style={{ flex: '1 1 220px', minWidth: 180 }}>
											<Progress
												percent={Math.min(100, Math.round((row.uploaded_bytes / total) * 100))}
												showInfo
												size="small"
											/>
											<Typography.Text type="tertiary" size="small">
												{formatBytes(row.uploaded_bytes)} / {formatBytes(total)}
											</Typography.Text>
										</div>
									) : (
										<Typography.Text type="tertiary" size="small">
											这一阶段没有已知总量，只显示阶段与已确认字节（{formatBytes(row.uploaded_bytes)}）
										</Typography.Text>
									)}
									<Typography.Text type={stale ? 'warning' : 'tertiary'} size="small">
										{Number.isFinite(updatedAt)
											? `最后更新 ${relativeText(updatedAt, now)}（${formatMs(updatedAt)}）${stale ? '，已是过期快照' : ''}`
											: '没有进度时间'}
									</Typography.Text>
								</div>
								{row.last_error ? (
									<Typography.Text
										type={FAILED_STATUS.includes(row.status) ? 'danger' : 'tertiary'}
										size="small"
										style={{ wordBreak: 'break-all' }}
									>
										{FAILED_STATUS.includes(row.status) ? '最近错误' : '最近记录'}：{row.last_error}
									</Typography.Text>
								) : null}
							</div>
						);
					})
				)}
			</Card>

			<Card
				title="待投稿会话"
				headerExtraContent={<RecoveryLink />}
				bodyStyle={{ padding: 12 }}
			>
				{(sessions?.length ?? 0) === 0 ? (
					<Empty description="没有等待投稿的会话" />
				) : (
					sessions!.map((session) => (
						<div
							key={session.id}
							style={{ padding: '8px 0', borderBottom: '1px solid var(--semi-color-border)' }}
						>
							<div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'center' }}>
								<Typography.Text strong>{session.streamer_name}</Typography.Text>
								<Typography.Text type="tertiary" size="small">
									{session.stream_title}
								</Typography.Text>
								<Tag size="small" color="violet">
									{session.action_message}
								</Tag>
								<span style={{ marginLeft: 'auto', display: 'flex', gap: 4 }}>
									{jump('upload_session_id', String(session.id), false)}
									{session.last_submit_error
										? jump('upload_session_id', String(session.id), true)
										: null}
								</span>
							</div>
							<Typography.Text type="tertiary" size="small">
								分段完成 {session.completeness.succeeded}/{session.completeness.total_expected}
								，待处理 {session.completeness.pending}，失败 {session.completeness.failed}
							</Typography.Text>
						</div>
					))
				)}
			</Card>
		</div>
	);
}

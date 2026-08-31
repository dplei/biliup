'use client';
import { useEffect, useState } from 'react';
import { Button, Spin, Tag, Typography } from '@douyinfe/semi-ui';
import { IconAlertCircle, IconAlertTriangle, IconInfoCircle } from '@douyinfe/semi-icons';
import {
	AssocField,
	DiagnosticPayload,
	LogLevel,
	StoredEvent,
	categoryText,
	fieldText,
	formatMs,
	levelText,
	OUTCOME_TEXT,
} from '../../lib/log-events';
import { API_BASE } from '../../lib/api-streamer';

/** 级别的颜色与图标。文字和图标同时给，不靠颜色单独表意。 */
const LEVEL_STYLE: Record<LogLevel, { color: string; background: string; icon: JSX.Element }> = {
	TRACE: {
		color: 'var(--semi-color-text-2)',
		background: 'transparent',
		icon: <IconInfoCircle size="small" />,
	},
	DEBUG: {
		color: 'var(--semi-color-text-2)',
		background: 'transparent',
		icon: <IconInfoCircle size="small" />,
	},
	INFO: {
		color: 'var(--semi-color-info)',
		background: 'transparent',
		icon: <IconInfoCircle size="small" />,
	},
	WARN: {
		color: 'var(--semi-color-warning)',
		background: 'var(--semi-color-warning-light-default)',
		icon: <IconAlertTriangle size="small" />,
	},
	ERROR: {
		color: 'var(--semi-color-danger)',
		background: 'var(--semi-color-danger-light-default)',
		icon: <IconAlertCircle size="small" />,
	},
};

const MONO = 'var(--semi-font-family-mono, ui-monospace, SFMono-Regular, Menlo, monospace)';

const IDENTITY_FIELDS = [
	'streamer_info_id',
	'live_streamer_id',
	'upload_session_id',
	'segment_id',
	'missing_id',
	'download_attempt_id',
	'upload_attempt_id',
	'task_id',
];

/** 可以直接跳到「本场/本分段完整事件」的关联维度，按从大到小排。 */
const SCOPES: { key: AssocField; text: string }[] = [
	{ key: 'streamer_info_id', text: '查看本场完整事件' },
	{ key: 'upload_session_id', text: '查看本次投稿会话' },
	{ key: 'segment_id', text: '查看本分段' },
];

function text(value: unknown): string {
	if (value === null || value === undefined) return '';
	return String(value);
}

interface Props {
	event: StoredEvent;
	expanded: boolean;
	onToggle: () => void;
	onScope: (key: AssocField, value: string, instanceId: string) => void;
}

export default function EventRow({ event, expanded, onToggle, onScope }: Props) {
	const { data } = event;
	const style = LEVEL_STYLE[data.level] ?? LEVEL_STYLE.INFO;
	const values = data.fields?.values ?? {};
	const streamer = text(values.streamer_name);
	const file = text(values.original_file) || text(values.artifact_file);
	const outcome = text(values.outcome);

	return (
		<div
			style={{
				borderBottom: '1px solid var(--semi-color-border)',
				borderLeft: `3px solid ${data.level === 'WARN' || data.level === 'ERROR' ? style.color : 'transparent'}`,
				background: style.background,
			}}
		>
			<div
				role="button"
				tabIndex={0}
				aria-expanded={expanded}
				onClick={onToggle}
				onKeyDown={(inputEvent) => {
					if (inputEvent.key === 'Enter' || inputEvent.key === ' ') {
						inputEvent.preventDefault();
						onToggle();
					}
				}}
				style={{ padding: '8px 12px', cursor: 'pointer', outlineOffset: -2 }}
			>
				<div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'baseline', gap: 8 }}>
					<span style={{ fontFamily: MONO, fontSize: 12, color: 'var(--semi-color-text-2)' }}>
						{formatMs(data.occurred_at_ms)}
					</span>
					<span
						style={{
							display: 'inline-flex',
							alignItems: 'center',
							gap: 4,
							color: style.color,
							fontSize: 12,
							fontWeight: 600,
						}}
					>
						{style.icon}
						{levelText(data.level)}
					</span>
					<span style={{ flex: '1 1 260px', minWidth: 0, wordBreak: 'break-word' }}>
						{data.message || data.event_name}
					</span>
				</div>
				<div
					style={{
						display: 'flex',
						flexWrap: 'wrap',
						gap: 6,
						marginTop: 4,
						fontSize: 12,
						color: 'var(--semi-color-text-2)',
					}}
				>
					<Tag size="small" color="grey">
						{categoryText(data.category)}
					</Tag>
					{data.capture_kind === 'legacy_bridge' ? (
						<Tag size="small" color="orange">
							桥接诊断
						</Tag>
					) : null}
					{outcome ? <span>{OUTCOME_TEXT[outcome] ?? outcome}</span> : null}
					{streamer ? <span>主播：{streamer}</span> : null}
					{file ? <span style={{ fontFamily: MONO, wordBreak: 'break-all' }}>{file}</span> : null}
					{values.count ? (
						<span>来源已汇总 {String(values.count)} 次</span>
					) : null}
					{event.has_diagnostic ? <span>含原始诊断</span> : null}
				</div>
			</div>
			{expanded ? <Detail event={event} onScope={onScope} /> : null}
		</div>
	);
}

function Detail({
	event,
	onScope,
}: {
	event: StoredEvent;
	onScope: (key: AssocField, value: string, instanceId: string) => void;
}) {
	const { data } = event;
	const values = data.fields?.values ?? {};
	const quality = data.fields?.quality;
	const identity = IDENTITY_FIELDS.filter((key) => text(values[key]));
	const rest = Object.keys(values)
		.filter((key) => !IDENTITY_FIELDS.includes(key))
		.sort();
	// 结果是结构化取值，中文只是读起来方便，原始码要一起留着，方便和契约/导出对照。
	const display = (key: string, value: string) =>
		key === 'outcome' && OUTCOME_TEXT[value] ? `${OUTCOME_TEXT[value]}（${value}）` : value;

	return (
		<div
			style={{
				padding: '10px 12px 14px',
				background: 'var(--semi-color-fill-0)',
				borderTop: '1px dashed var(--semi-color-border)',
			}}
		>
			<Grid
				rows={[
					['事件名', data.event_name],
					['稳定事件 ID', data.event_uid],
					['入库序号', String(event.id)],
					['运行实例', data.instance_id],
					['进程运行', data.process_run_id],
					['程序版本', data.app_version],
					['来源', data.capture_kind === 'native' ? '原生事件' : `桥接诊断（${data.target}）`],
				]}
			/>
			{identity.length > 0 ? (
				<>
					<Typography.Text type="secondary" style={{ display: 'block', margin: '10px 0 4px' }}>
						关联身份
					</Typography.Text>
					<Grid rows={identity.map((key) => [fieldText(key), text(values[key])])} />
				</>
			) : null}
			{rest.length > 0 ? (
				<>
					<Typography.Text type="secondary" style={{ display: 'block', margin: '10px 0 4px' }}>
						技术字段
					</Typography.Text>
					<Grid rows={rest.map((key) => [fieldText(key), display(key, text(values[key]))])} />
				</>
			) : null}
			{values.count ? (
				<Typography.Text type="tertiary" size="small" style={{ display: 'block', marginTop: 8 }}>
					这条是采集源自己按分段汇总的记录（次数、首末时间与极值见上），库里没有保存被汇总掉的
					每一条原始记录，不是界面折叠。
				</Typography.Text>
			) : null}
			{quality && (quality.redacted > 0 || quality.truncated > 0 || quality.rejected > 0) ? (
				<Typography.Text type="tertiary" size="small" style={{ display: 'block', marginTop: 8 }}>
					字段处理：脱敏 {quality.redacted} 项、截断 {quality.truncated} 项、拒绝{' '}
					{quality.rejected} 项。
				</Typography.Text>
			) : null}
			{event.has_diagnostic ? <RawDiagnostic eventUid={data.event_uid} /> : null}
			<div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, marginTop: 12 }}>
				{SCOPES.filter((scope) => text(values[scope.key])).map((scope) => (
					<Button
						key={scope.key}
						size="small"
						theme="light"
						onClick={() => onScope(scope.key, text(values[scope.key]), data.instance_id)}
					>
						{scope.text}
					</Button>
				))}
			</div>
		</div>
	);
}

function Grid({ rows }: { rows: string[][] }) {
	return (
		<div
			style={{
				display: 'grid',
				gridTemplateColumns: 'minmax(88px, max-content) minmax(0, 1fr)',
				columnGap: 12,
				rowGap: 4,
				fontSize: 12,
			}}
		>
			{rows.map(([label, value]) => (
				<Row key={label} label={label} value={value} />
			))}
		</div>
	);
}

function Row({ label, value }: { label: string; value: string }) {
	return (
		<>
			<span style={{ color: 'var(--semi-color-text-2)' }}>{label}</span>
			<span style={{ fontFamily: MONO, wordBreak: 'break-all' }}>{value || '—'}</span>
		</>
	);
}

/**
 * 原始诊断按需取，且始终折叠：一段 stderr 不该把列表撑高好几屏。内容按普通文本渲染。
 */
function RawDiagnostic({ eventUid }: { eventUid: string }) {
	const [payload, setPayload] = useState<DiagnosticPayload | null>(null);
	const [state, setState] = useState<'idle' | 'loading' | 'failed'>('idle');
	const [open, setOpen] = useState(false);

	useEffect(() => {
		if (!open || payload || state === 'loading') return;
		const controller = new AbortController();
		setState('loading');
		fetch(`${API_BASE}/v1/log-events/${eventUid}/diagnostic`, {
			signal: controller.signal,
		})
			.then((response) => (response.ok ? response.json() : Promise.reject(response.status)))
			.then((body) => {
				setPayload(body);
				setState('idle');
			})
			.catch(() => {
				if (!controller.signal.aborted) setState('failed');
			});
		return () => controller.abort();
	}, [open, payload, state, eventUid]);

	return (
		<div style={{ marginTop: 10 }}>
			<Button size="small" theme="borderless" onClick={() => setOpen(!open)}>
				{open ? '收起原始诊断' : '展开原始诊断'}
			</Button>
			{open ? (
				<div style={{ marginTop: 6 }}>
					{state === 'loading' ? <Spin size="small" /> : null}
					{state === 'failed' ? (
						<Typography.Text type="danger" size="small">
							原始诊断读取失败，可能已被保留期清理。
						</Typography.Text>
					) : null}
					{payload ? (
						<>
							<Grid
								rows={[
									['退出码', payload.exit_code === null || payload.exit_code === undefined ? '未知（可能被信号结束）' : String(payload.exit_code)],
									['首个致命错误', payload.first_fatal ?? ''],
									['原始字节', String(payload.total_bytes ?? 0)],
									['是否截断', payload.truncated ? '是，只保留有界尾部' : '否'],
									['是否脱敏', payload.redacted ? '是' : '否'],
								]}
							/>
							<pre
								style={{
									margin: '6px 0 0',
									padding: 8,
									maxHeight: 220,
									overflow: 'auto',
									fontFamily: MONO,
									fontSize: 12,
									whiteSpace: 'pre-wrap',
									wordBreak: 'break-all',
									background: 'var(--semi-color-fill-1)',
									borderRadius: 4,
								}}
							>
								{payload.tail || '（无尾部内容）'}
							</pre>
						</>
					) : null}
				</div>
			) : null}
		</div>
	);
}

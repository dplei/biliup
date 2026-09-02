'use client';
import Link from 'next/link';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import useSWR from 'swr';
import {
	Banner,
	Button,
	Card,
	Empty,
	Layout,
	Nav,
	RadioGroup,
	Radio,
	Spin,
	Tag,
	Tooltip,
	Typography,
} from '@douyinfe/semi-ui';
import { IconActivity, IconDownload, IconPause, IconPlay, IconRefresh } from '@douyinfe/semi-icons';
import { fetcher, LiveStreamerEntity, API_BASE } from '../../lib/api-streamer';
import {
	AssocField,
	CATEGORY_TEXT,
	EventFilters,
	LogLevel,
	NATIVE_CATEGORIES,
	StoredEvent,
	categoryText,
	exportUrl,
	filterParams,
	storageTrouble,
} from '../../lib/log-events';
import { LEGACY_LOG_HREF } from '../../lib/log-view-config';
import EventRow from './EventRow';
import FilterBar, { StreamerOption } from './FilterBar';
import ProgressView from './ProgressView';
import { useLogEventFeed } from './useLogEventFeed';
import { PageState, useUrlState, ViewKey } from './useUrlFilters';

const QUICK_LEVELS: LogLevel[] = ['INFO', 'WARN', 'ERROR'];
/** 滚离顶部多少像素就算「正在读历史」，此后新事件只进缓冲区。 */
const FOLLOW_THRESHOLD = 24;

const COVERAGE_NOTE = `新页默认只显示原生事件。目前原生已覆盖：${NATIVE_CATEGORIES.map(
	(category) => CATEGORY_TEXT[category],
).join('、')}；系统、认证、审计仍只有桥接诊断，需要在「更多条件」里显式选择来源。空结果可能是该业务尚未接入，不代表没有发生异常。`;

interface ScopeMemory {
	filters: EventFilters;
	scrollTop: number;
}

/**
 * 「日志与事件」页面主体。
 *
 * 数据全部来自独立事件库的只读接口：这里不读日志文件、不解析日志字符串，也不自己推断级别。
 * 旧的 `/logviewer` 与静态 `.log` 下载在迁移期原样保留，新页出问题可以直接回去。
 */
export default function LogEventsView({ preview = true }: { preview?: boolean }) {
	const [state, setState] = useUrlState();
	const [paused, setPaused] = useState(false);
	const [expanded, setExpanded] = useState<Set<number>>(new Set());
	const [scopeMemory, setScopeMemory] = useState<ScopeMemory | null>(null);
	const [levelCounts, setLevelCounts] = useState<Partial<Record<LogLevel, number>> | undefined>(
		undefined,
	);
	const listRef = useRef<HTMLDivElement | null>(null);

	const { view, filters } = state;
	const feed = useLogEventFeed(filters, paused);
	const { setFrozen } = feed;

	const { data: streamers } = useSWR<LiveStreamerEntity[]>('/v1/streamers', fetcher);
	const streamerOptions: StreamerOption[] = useMemo(
		() =>
			(streamers ?? []).map((streamer) => ({
				value: String(streamer.id),
				label: streamer.remark || streamer.url,
			})),
		[streamers],
	);

	const instances = useMemo(() => {
		const seen: string[] = [];
		for (const event of feed.events) {
			if (!seen.includes(event.data.instance_id)) seen.push(event.data.instance_id);
		}
		if (filters.instanceId && !seen.includes(filters.instanceId)) seen.push(filters.instanceId);
		return seen;
	}, [feed.events, filters.instanceId]);
	const defaultInstance = filters.instanceId || instances[0] || '';

	const update = useCallback(
		(next: Partial<PageState>) => setState({ ...state, ...next }),
		[setState, state],
	);

	const applyFilters = useCallback(
		(next: EventFilters) => {
			setExpanded(new Set());
			update({ filters: next });
		},
		[update],
	);

	// 级别计数用其余筛选条件单独统计，不是当前这一页的条数；统计没回来就显示「统计中」。
	const countKey = useMemo(
		() => JSON.stringify({ ...filters, levels: [] }),
		// eslint-disable-next-line react-hooks/exhaustive-deps
		[filters],
	);
	const [countsTick, setCountsTick] = useState(0);
	const countedTotal = useRef(-1);
	const totalRef = useRef(0);
	totalRef.current = feed.meta.total;
	// 实时事件会改变命中数；每 5 秒对比一次，变了才重算，避免一条一条地打统计请求。
	useEffect(() => {
		const timer = setInterval(() => {
			if (totalRef.current !== countedTotal.current) setCountsTick((value) => value + 1);
		}, 5000);
		return () => clearInterval(timer);
	}, []);
	useEffect(() => {
		if (feed.meta.availability !== 'ready') {
			setLevelCounts({});
			return;
		}
		const controller = new AbortController();
		const countedAt = totalRef.current;
		setLevelCounts(undefined);
		(async () => {
			try {
				const entries = await Promise.all(
					QUICK_LEVELS.map(async (level) => {
						const params = filterParams({ ...filters, levels: [level] });
						params.set('limit', '1');
						const response = await fetch(`${API_BASE}/v1/log-events?${params.toString()}`, {
							signal: controller.signal,
						});
						if (!response.ok) throw new Error(String(response.status));
						const body = await response.json();
						return [level, body.total as number] as const;
					}),
				);
				countedTotal.current = countedAt;
				setLevelCounts(Object.fromEntries(entries));
			} catch {
				if (!controller.signal.aborted) setLevelCounts({});
			}
		})();
		return () => controller.abort();
		// eslint-disable-next-line react-hooks/exhaustive-deps
	}, [countKey, countsTick, feed.meta.availability]);

	// 展开详情或滚离顶部都冻结阅读位置：新事件不插进正在看的这一屏。
	const [scrolled, setScrolled] = useState(false);
	useEffect(() => {
		setFrozen(scrolled || expanded.size > 0);
	}, [scrolled, expanded, setFrozen]);

	const enterScope = useCallback(
		(key: AssocField, value: string, instanceId: string, failuresOnly = false) => {
			setScopeMemory({ filters, scrollTop: listRef.current?.scrollTop ?? 0 });
			setExpanded(new Set());
			// 场次范围要看完整经过，所以临时放开会挡住前后事件的级别、关键词和时间条件；
			// 只看失败时保留警告与错误，其余同样放开。
			update({
				view: 'events',
				filters: {
					...filters,
					levels: failuresOnly ? ['WARN', 'ERROR'] : ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'],
					keyword: '',
					categories: [],
					range: 'all',
					sinceMs: undefined,
					untilMs: undefined,
					instanceId: instanceId || defaultInstance,
					assocKey: key,
					assocValue: value,
				},
			});
		},
		[defaultInstance, filters, update],
	);

	const leaveScope = useCallback(() => {
		if (!scopeMemory) return;
		const restore = scopeMemory;
		setScopeMemory(null);
		setExpanded(new Set());
		update({ filters: restore.filters });
		window.setTimeout(() => {
			if (listRef.current) listRef.current.scrollTop = restore.scrollTop;
		}, 0);
	}, [scopeMemory, update]);

	const jumpFromProgress = useCallback(
		(key: AssocField, value: string, failuresOnly: boolean) =>
			enterScope(key, value, defaultInstance, failuresOnly),
		[defaultInstance, enterScope],
	);

	const download = (format: 'jsonl' | 'csv') => {
		const anchor = document.createElement('a');
		anchor.href = exportUrl(filters, format);
		anchor.rel = 'noopener';
		document.body.appendChild(anchor);
		anchor.click();
		anchor.remove();
	};

	const trouble = storageTrouble(feed.meta.health);
	const connection = feed.error
		? { text: '服务端连接失败', tone: 'danger' as const }
		: feed.meta.availability === 'disabled'
			? { text: '采集未开启', tone: 'warning' as const }
			: feed.meta.availability === 'unavailable'
				? { text: '事件库不可读', tone: 'danger' as const }
				: describeConnection(feed.live, feed.frozen, paused);

	return (
		<>
			<Layout.Header style={{ backgroundColor: 'var(--semi-color-bg-1)' }}>
				<Nav
					style={{ border: 'none' }}
					mode="horizontal"
					header={
						<>
							<div
								style={{
									backgroundColor: 'rgba(var(--semi-blue-4), 1)',
									borderRadius: 'var(--semi-border-radius-large)',
									color: 'var(--semi-color-bg-0)',
									display: 'flex',
									padding: '6px',
								}}
							>
								<IconActivity size="large" />
							</div>
							<h4 style={{ marginLeft: 12 }}>日志与事件{preview ? '（试用）' : ''}</h4>
							<Tag size="small" color="blue" style={{ marginLeft: 12 }}>
								数据来源：结构化事件库
							</Tag>
							<Link
								href={LEGACY_LOG_HREF}
								style={{ marginLeft: 12, fontSize: 13, color: 'var(--semi-color-link)' }}
							>
								回到旧的实时日志
							</Link>
						</>
					}
				/>
			</Layout.Header>
			<Layout.Content
				style={{
					padding: 12,
					backgroundColor: 'var(--semi-color-bg-0)',
					height: 'calc(100vh - 60px)',
					display: 'flex',
					flexDirection: 'column',
					gap: 8,
					// 只让事件列表自己滚：外层再滚一层的话，「滚离顶部就冻结」就跟着失效了。
					overflow: 'hidden',
				}}
			>
				<div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, alignItems: 'center' }}>
					<RadioGroup
						type="button"
						value={view}
						onChange={(event) => update({ view: event.target.value as ViewKey })}
					>
						<Radio value="events">事件日志</Radio>
						<Radio value="progress">运行进度</Radio>
					</RadioGroup>
					<span style={{ marginLeft: 'auto', display: 'flex', flexWrap: 'wrap', gap: 8 }}>
						<Typography.Text type={connection.tone} size="small" style={{ alignSelf: 'center' }}>
							{connection.text}
						</Typography.Text>
						<Button
							size="small"
							theme="borderless"
							icon={paused ? <IconPlay /> : <IconPause />}
							onClick={() => setPaused(!paused)}
						>
							{paused ? '恢复刷新' : '暂停刷新'}
						</Button>
						<Button size="small" theme="borderless" icon={<IconRefresh />} onClick={feed.refresh}>
							刷新
						</Button>
						<Tooltip content="导出的是当前筛选结果，最多 2 万条；超出部分会在文件末尾标注截断。">
							<Button
								size="small"
								theme="borderless"
								icon={<IconDownload />}
								onClick={() => download('jsonl')}
							>
								导出当前筛选结果
							</Button>
						</Tooltip>
						<Button size="small" theme="borderless" onClick={() => download('csv')}>
							CSV
						</Button>
					</span>
				</div>

				{view === 'progress' ? (
					<div style={{ flex: 1, minHeight: 0, overflow: 'auto' }}>
						<ProgressView onJump={jumpFromProgress} instanceId={defaultInstance} />
					</div>
				) : (
					<Card
						style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}
						bodyStyle={{
							padding: 12,
							flex: 1,
							minHeight: 0,
							display: 'flex',
							flexDirection: 'column',
							overflow: 'auto',
						}}
					>
						<FilterBar
							filters={filters}
							onChange={applyFilters}
							levelCounts={levelCounts}
							streamers={streamerOptions}
							instances={instances}
							defaultInstance={defaultInstance}
						/>

						<div
							style={{
								display: 'flex',
								flexWrap: 'wrap',
								alignItems: 'center',
								gap: 8,
								margin: '10px 0 6px',
							}}
						>
							<Typography.Text size="small">
								命中 {feed.meta.total} 条（当前筛选，最新在前）
							</Typography.Text>
							{scopeMemory ? (
								<Button size="small" theme="light" onClick={leaveScope}>
									返回原筛选
								</Button>
							) : null}
							{feed.meta.uncleanShutdowns > 0 ? (
								<Typography.Text type="warning" size="small">
									事件库曾检测到 {feed.meta.uncleanShutdowns}
									 个未确认正常关闭或心跳中断的运行，相关时段可能有事件未写完。
								</Typography.Text>
							) : null}
							{feed.meta.unknownWriterRuns > 0 ? (
								<Typography.Text type="warning" size="small">
									当前仍有 {feed.meta.unknownWriterRuns} 个状态未知的 writer。
								</Typography.Text>
							) : null}
						</div>

						{trouble ? (
							<Banner
								type="danger"
								closeIcon={null}
								description={`事件写入异常：${trouble}。页面能连上不代表都写进去了。`}
							/>
						) : null}
						{feed.meta.gap || feed.liveGap !== null ? (
							<Banner
								type="warning"
								closeIcon={null}
								description={`保留期已清理到 #${feed.liveGap ?? feed.meta.prunedThrough}，这个范围里更早的事件已经不在库中，结果不完整。`}
							/>
						) : null}
						<Typography.Text type="tertiary" size="small" style={{ margin: '4px 0 8px' }}>
							{COVERAGE_NOTE}
						</Typography.Text>

						{feed.releasedNewest ? (
							<Banner
								type="info"
								closeIcon={null}
								description="往回翻得够远，为控内存已经释放了较新的一段；点「刷新」回到最新。"
							/>
						) : null}
						<div
							ref={listRef}
							onScroll={(event) =>
								setScrolled((event.target as HTMLDivElement).scrollTop > FOLLOW_THRESHOLD)
							}
							style={{
								marginTop: 8,
								border: '1px solid var(--semi-color-border)',
								borderRadius: 4,
								flex: 1,
								minHeight: 160,
								overflow: 'auto',
							}}
						>
							{feed.pending.length > 0 ? (
								// 粘在列表顶部而不是插在列表上方：提示出现时不能把正在读的那一行顶走。
								<div style={{ position: 'sticky', top: 0, zIndex: 2, padding: 4 }}>
									<Button
										block
										theme="solid"
										size="small"
										onClick={() => {
											feed.flushPending();
											if (listRef.current) listRef.current.scrollTop = 0;
										}}
									>
										有 {feed.pending.length} 条新事件
										{feed.pendingOverflow ? '（缓冲已满，更早的新事件请刷新查看）' : ''}，点击插入
									</Button>
								</div>
							) : null}
							<Body
								feed={feed}
								expanded={expanded}
								onToggle={(id) =>
									setExpanded((current) => {
										const next = new Set(current);
										if (!next.delete(id)) next.add(id);
										return next;
									})
								}
								onScope={enterScope}
							/>
						</div>

						<div
							style={{ display: 'flex', justifyContent: 'center', padding: '8px 0', flexShrink: 0 }}
						>
							{feed.hasOlder ? (
								<Button size="small" theme="light" loading={feed.loadingOlder} onClick={feed.loadOlder}>
									加载更早记录
								</Button>
							) : feed.events.length > 0 ? (
								<Typography.Text type="tertiary" size="small">
									已经到这个范围的最早一条
								</Typography.Text>
							) : null}
						</div>
					</Card>
				)}
			</Layout.Content>
		</>
	);
}

function Body({
	feed,
	expanded,
	onToggle,
	onScope,
}: {
	feed: ReturnType<typeof useLogEventFeed>;
	expanded: Set<number>;
	onToggle: (id: number) => void;
	onScope: (key: AssocField, value: string, instanceId: string) => void;
}) {
	if (feed.meta.availability === 'disabled') {
		return (
			<Empty
				title="结构化事件采集没有开启"
				description={
					<span>
						这个进程没有启用独立事件库（<code>BILIUP_OBSERVABILITY</code>），所以这里查不到任何
						事件——这不代表运行期没有异常。开启前请继续用
						<Link href={LEGACY_LOG_HREF}> 旧的实时日志</Link>。
					</span>
				}
				style={{ padding: 32 }}
			/>
		);
	}
	if (feed.meta.availability === 'unavailable') {
		return (
			<Empty
				title="事件库暂时读不了"
				description={feed.meta.error ?? '事件库打不开，历史与实时都不可用。'}
				style={{ padding: 32 }}
			/>
		);
	}
	if (feed.error) {
		return (
			<Empty
				title="查询失败"
				description={
					<span>
						{feed.error}
						<Button size="small" theme="borderless" onClick={feed.refresh}>
							重试
						</Button>
					</span>
				}
				style={{ padding: 32 }}
			/>
		);
	}
	if (feed.loading) {
		return (
			<div style={{ display: 'flex', justifyContent: 'center', padding: 40 }}>
				<Spin size="large" />
			</div>
		);
	}
	if (feed.events.length === 0) {
		return (
			<Empty
				title="没有符合条件的事件"
				description="换个时间范围或放宽条件再试；也可能是这一段业务还没接入原生事件。"
				style={{ padding: 32 }}
			/>
		);
	}
	return (
		<>
			{feed.events.map((event: StoredEvent) => (
				<EventRow
					key={event.id}
					event={event}
					expanded={expanded.has(event.id)}
					onToggle={() => onToggle(event.id)}
					onScope={onScope}
				/>
			))}
		</>
	);
}

function describeConnection(
	live: ReturnType<typeof useLogEventFeed>['live'],
	frozen: boolean,
	paused: boolean,
): { text: string; tone: 'success' | 'warning' | 'danger' | 'tertiary' } {
	if (paused) return { text: '已暂停刷新（后台仍在记录）', tone: 'warning' };
	switch (live) {
		case 'open':
			return frozen
				? { text: '实时连接正常，已冻结阅读位置', tone: 'tertiary' }
				: { text: '实时连接正常', tone: 'success' };
		case 'connecting':
			return { text: '正在连接实时流', tone: 'tertiary' };
		case 'error':
			return { text: '实时连接断开，正在重连；历史仍可查询', tone: 'danger' };
		default:
			return { text: '仅历史查询', tone: 'tertiary' };
	}
}

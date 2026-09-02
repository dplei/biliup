'use client';
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
	Availability,
	EventFilters,
	HealthSnapshot,
	ListResponse,
	StoredEvent,
	listUrl,
	streamUrl,
} from '../../lib/log-events';

/** 一页历史的条数；后端上限 200，这里取和默认页长一致的 50。 */
const PAGE = 50;
/** 内存里最多保留的事件数：长时间挂着页面不能无限增长，超出的从最旧一端丢，并允许再翻回来。 */
const MAX_KEPT = 500;
/** 冻结阅读时最多缓存多少条新事件；超出只保留最新的，并在提示里说明已经不完整。 */
const MAX_PENDING = 200;

export type LiveState = 'connecting' | 'open' | 'error' | 'paused' | 'off';

export interface FeedMeta {
	availability: Availability;
	coverage: string;
	total: number;
	prunedThrough: number;
	gap: boolean;
	uncleanShutdowns: number;
	activeWriterRuns: number;
	unknownWriterRuns: number;
	health: HealthSnapshot | undefined;
	error: string | null;
}

const EMPTY_META: FeedMeta = {
	availability: 'ready',
	coverage: 'native',
	total: 0,
	prunedThrough: 0,
	gap: false,
	uncleanShutdowns: 0,
	activeWriterRuns: 0,
	unknownWriterRuns: 0,
	health: undefined,
	error: null,
};

function describe(error: unknown): string {
	// fetch 在连不上时只抛一句英文的 TypeError，直接透出去等于让人自己猜。
	if (error instanceof TypeError) return '连不上服务端，请确认 biliup 服务正在运行';
	if (error instanceof Error) return error.message;
	return String(error);
}

async function requestList(url: string, signal: AbortSignal): Promise<ListResponse> {
	// 同源默认带上会话 cookie；跨源开发时服务端没开 allow-credentials，硬带反而会被浏览器挡掉。
	const response = await fetch(url, { signal });
	if (response.status === 401) {
		const returnTo = encodeURIComponent(window.location.pathname + window.location.search);
		window.location.href = `/login?next=${returnTo}`;
		throw new Error('未登录');
	}
	if (!response.ok) {
		const body = await response.json().catch(() => null);
		throw new Error(body?.message ?? `请求失败（HTTP ${response.status}）`);
	}
	return response.json();
}

export interface Feed {
	events: StoredEvent[];
	pending: StoredEvent[];
	pendingOverflow: boolean;
	meta: FeedMeta;
	loading: boolean;
	loadingOlder: boolean;
	error: string | null;
	hasOlder: boolean;
	/** 为了控内存丢掉了较新的一段：此时列表只是历史窗口，不能再当作「最新在前」。 */
	releasedNewest: boolean;
	live: LiveState;
	liveGap: number | null;
	frozen: boolean;
	setFrozen: (frozen: boolean) => void;
	flushPending: () => void;
	loadOlder: () => void;
	refresh: () => void;
}

/**
 * 事件列表的取数与实时接续。
 *
 * 历史查询按 id 倒序拿最新一页，往回翻用 `until_id`；实时永远从「已见过的最大 id」往前接，
 * 所以历史与订阅共用同一个游标，不会重复也不会跳过。冻结阅读时新事件只进缓冲区，
 * 不插进正在看的列表。暂停只停页面这一侧的接收，恢复时用游标补齐，不影响后台写入。
 */
export function useLogEventFeed(filters: EventFilters, paused: boolean): Feed {
	const [events, setEvents] = useState<StoredEvent[]>([]);
	const [pending, setPending] = useState<StoredEvent[]>([]);
	const [pendingOverflow, setPendingOverflow] = useState(false);
	const [meta, setMeta] = useState<FeedMeta>(EMPTY_META);
	const [loading, setLoading] = useState(true);
	const [loadingOlder, setLoadingOlder] = useState(false);
	const [error, setError] = useState<string | null>(null);
	const [hasOlder, setHasOlder] = useState(false);
	const [releasedNewest, setReleasedNewest] = useState(false);
	const [frozen, setFrozenState] = useState(false);
	const [live, setLive] = useState<LiveState>('off');
	const [liveGap, setLiveGap] = useState<number | null>(null);
	const [feedEpoch, setFeedEpoch] = useState(0);
	const [liveEpoch, setLiveEpoch] = useState(0);
	const [reloadEpoch, setReloadEpoch] = useState(0);

	const cursorRef = useRef(0);
	const frozenRef = useRef(false);
	/** 正在重新查询时到达的实时事件一律丢弃，否则会混进一份还没成形的结果里。 */
	const loadingRef = useRef(true);
	/** 当前生效的筛选。上一轮筛选的订阅在关闭前还可能推事件过来，靠它认出来并忽略。 */
	const activeKeyRef = useRef('');
	const filterKey = useMemo(() => JSON.stringify(filters), [filters]);

	const setFrozen = useCallback((value: boolean) => {
		frozenRef.current = value;
		setFrozenState(value);
	}, []);

	// 首屏与筛选变化：清空旧游标，取消还在路上的请求，重新拿最新一页。
	useEffect(() => {
		const controller = new AbortController();
		loadingRef.current = true;
		activeKeyRef.current = filterKey;
		setLoading(true);
		setError(null);
		setReleasedNewest(false);
		setEvents([]);
		setPending([]);
		setPendingOverflow(false);
		setLiveGap(null);
		frozenRef.current = false;
		setFrozenState(false);
		cursorRef.current = 0;
		(async () => {
			try {
				const data = await requestList(
					listUrl(filters, { order: 'desc', limit: PAGE }),
					controller.signal,
				);
				setEvents(data.events);
				cursorRef.current = data.events.length > 0 ? data.events[0].id : 0;
				setHasOlder(data.next_until_id !== null);
				setMeta({
					availability: data.availability,
					coverage: data.coverage,
					total: data.total,
					prunedThrough: data.pruned_through,
					gap: data.gap,
					uncleanShutdowns: data.unclean_shutdowns,
					activeWriterRuns: data.active_writer_runs,
					unknownWriterRuns: data.unknown_writer_runs,
					health: data.health,
					error: data.error,
				});
				loadingRef.current = false;
				setLoading(false);
				setFeedEpoch((value) => value + 1);
			} catch (failure) {
				if (controller.signal.aborted) return;
				loadingRef.current = false;
				setError(describe(failure));
				setLoading(false);
			}
		})();
		return () => controller.abort();
		// eslint-disable-next-line react-hooks/exhaustive-deps
	}, [filterKey, reloadEpoch]);

	// 实时接续。断线后自己重开而不是让浏览器按原 URL 重连，否则会从旧游标重放。
	useEffect(() => {
		if (feedEpoch === 0) return;
		if (paused) {
			setLive('paused');
			return;
		}
		let timer: ReturnType<typeof setTimeout> | undefined;
		const subscribedKey = filterKey;
		const source = new EventSource(streamUrl(filters, cursorRef.current, PAGE));
		setLive('connecting');
		source.onopen = () => setLive('open');
		source.addEventListener('log-event', (message) => {
			if (loadingRef.current || activeKeyRef.current !== subscribedKey) return;
			let event: StoredEvent;
			try {
				event = JSON.parse((message as MessageEvent).data);
			} catch {
				return;
			}
			if (event.id <= cursorRef.current) return;
			cursorRef.current = event.id;
			setMeta((value) => ({ ...value, total: value.total + 1 }));
			if (frozenRef.current) {
				setPending((list) => {
					if (list.length >= MAX_PENDING) {
						setPendingOverflow(true);
						return [event, ...list.slice(0, MAX_PENDING - 1)];
					}
					return [event, ...list];
				});
				return;
			}
			setEvents((list) => {
				const next = [event, ...list];
				if (next.length > MAX_KEPT) {
					setHasOlder(true);
					return next.slice(0, MAX_KEPT);
				}
				return next;
			});
		});
		source.addEventListener('gap', (message) => {
			try {
				setLiveGap(JSON.parse((message as MessageEvent).data).pruned_through ?? 0);
			} catch {
				setLiveGap(0);
			}
		});
		source.addEventListener('unavailable', (message) => {
			setLive('error');
			try {
				const payload = JSON.parse((message as MessageEvent).data);
				setMeta((value) => ({ ...value, availability: payload.availability ?? 'unavailable' }));
			} catch {
				/* 状态已经标成 error，解析失败不额外覆盖 */
			}
		});
		source.onerror = () => {
			setLive('error');
			source.close();
			timer = setTimeout(() => setLiveEpoch((value) => value + 1), 3000);
		};
		return () => {
			source.close();
			if (timer) clearTimeout(timer);
		};
		// eslint-disable-next-line react-hooks/exhaustive-deps
	}, [feedEpoch, liveEpoch, paused]);

	const loadOlder = useCallback(() => {
		const oldest = events[events.length - 1];
		if (!oldest || oldest.id <= 1 || loadingOlder) return;
		const controller = new AbortController();
		setLoadingOlder(true);
		(async () => {
			try {
				const data = await requestList(
					listUrl(filters, { order: 'desc', limit: PAGE, until_id: oldest.id - 1 }),
					controller.signal,
				);
				setEvents((list) => {
					const merged = [...list, ...data.events];
					if (merged.length > MAX_KEPT) {
						// 往回读得足够远时释放最新的一段，内存才是有界的；页面会明说这件事。
						setReleasedNewest(true);
						return merged.slice(merged.length - MAX_KEPT);
					}
					return merged;
				});
				setHasOlder(data.next_until_id !== null);
				setMeta((value) => ({
					...value,
					prunedThrough: data.pruned_through,
					gap: value.gap || data.gap,
				}));
			} catch (failure) {
				if (!controller.signal.aborted) setError(describe(failure));
			} finally {
				setLoadingOlder(false);
			}
		})();
	}, [events, filters, loadingOlder]);

	const refresh = useCallback(() => setReloadEpoch((value) => value + 1), []);

	const flushPending = useCallback(() => {
		if (releasedNewest) {
			// 列表已经不含最新那一段，插进去会造成看不见的空洞，直接回到最新一页。
			refresh();
			return;
		}
		setPending((buffered) => {
			if (buffered.length > 0) {
				setEvents((list) => [...buffered, ...list].slice(0, MAX_KEPT));
			}
			return [];
		});
		setPendingOverflow(false);
	}, [refresh, releasedNewest]);

	return {
		events,
		pending,
		pendingOverflow,
		meta,
		loading,
		loadingOlder,
		error,
		hasOlder,
		releasedNewest,
		live,
		liveGap,
		frozen,
		setFrozen,
		flushPending,
		loadOlder,
		refresh,
	};
}

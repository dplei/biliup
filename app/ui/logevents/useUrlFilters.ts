'use client';
import { useCallback, useEffect, useRef, useState } from 'react';
import {
	ALL_LEVELS,
	ASSOC_FIELDS,
	AssocField,
	DEFAULT_FILTERS,
	EventFilters,
	LogLevel,
	RangeKey,
} from '../../lib/log-events';

export type ViewKey = 'events' | 'progress';

export interface PageState {
	view: ViewKey;
	filters: EventFilters;
}

const RANGES: RangeKey[] = ['1h', '24h', '7d', 'all', 'custom'];

export const DEFAULT_STATE: PageState = { view: 'events', filters: DEFAULT_FILTERS };

function parseNumber(value: string | null): number | undefined {
	if (!value) return undefined;
	const parsed = Number(value);
	return Number.isFinite(parsed) && parsed > 0 ? parsed : undefined;
}

/**
 * 从查询参数还原页面状态。取值不认识就退回默认，不把一个坏链接变成一次坏查询。
 */
export function decodeState(search: string): PageState {
	const params = new URLSearchParams(search);
	const levels = (params.get('levels') ?? '')
		.split(',')
		.filter((value): value is LogLevel => (ALL_LEVELS as string[]).includes(value));
	const categories = (params.get('categories') ?? '').split(',').filter(Boolean);
	const range = params.get('range') as RangeKey | null;
	const assoc = (params.get('assoc') ?? '').split(':');
	const assocKey = ASSOC_FIELDS.includes(assoc[0] as AssocField) ? (assoc[0] as AssocField) : '';
	const kind = params.get('kind');
	return {
		view: params.get('view') === 'progress' ? 'progress' : 'events',
		filters: {
			levels: levels.length > 0 ? levels : DEFAULT_FILTERS.levels,
			categories,
			keyword: params.get('q') ?? '',
			eventName: params.get('name') ?? '',
			captureKind:
				kind === 'legacy_bridge' || kind === 'all' || kind === 'native' ? kind : 'native',
			range: range && RANGES.includes(range) ? range : DEFAULT_FILTERS.range,
			sinceMs: parseNumber(params.get('since')),
			untilMs: parseNumber(params.get('until')),
			instanceId: params.get('instance') ?? '',
			assocKey,
			assocValue: assocKey ? assoc.slice(1).join(':') : '',
		},
	};
}

export function encodeState(state: PageState): string {
	const params = new URLSearchParams();
	const { filters } = state;
	if (state.view !== 'events') params.set('view', state.view);
	if (filters.levels.join(',') !== DEFAULT_FILTERS.levels.join(',')) {
		params.set('levels', filters.levels.join(','));
	}
	if (filters.categories.length > 0) params.set('categories', filters.categories.join(','));
	if (filters.keyword) params.set('q', filters.keyword);
	if (filters.eventName) params.set('name', filters.eventName);
	if (filters.captureKind !== 'native') params.set('kind', filters.captureKind);
	if (filters.range !== DEFAULT_FILTERS.range) params.set('range', filters.range);
	if (filters.range === 'custom') {
		if (filters.sinceMs) params.set('since', String(filters.sinceMs));
		if (filters.untilMs) params.set('until', String(filters.untilMs));
	}
	if (filters.instanceId) params.set('instance', filters.instanceId);
	if (filters.assocKey && filters.assocValue) {
		params.set('assoc', `${filters.assocKey}:${filters.assocValue}`);
	}
	const query = params.toString();
	return query ? `?${query}` : '';
}

/**
 * 页面状态同步到地址栏：刷新、复制链接、浏览器前进后退都能回到同一份筛选。
 *
 * 这里直接用 History API 而不是 next/navigation，因为本应用是静态导出，`useSearchParams`
 * 会把整页拖进 Suspense 边界；地址栏行为本身是一样的。
 */
export function useUrlState(): [PageState, (next: PageState) => void] {
	const [state, setState] = useState<PageState>(() => DEFAULT_STATE);
	const applied = useRef('');

	useEffect(() => {
		const restore = () => {
			applied.current = window.location.search;
			setState(decodeState(window.location.search));
		};
		restore();
		window.addEventListener('popstate', restore);
		return () => window.removeEventListener('popstate', restore);
	}, []);

	const update = useCallback((next: PageState) => {
		setState(next);
		const query = encodeState(next);
		if (query !== applied.current) {
			applied.current = query;
			window.history.replaceState(null, '', `${window.location.pathname}${query}`);
		}
	}, []);

	return [state, update];
}


/**
 * 独立事件库（`/v1/log-events`）的前端契约与文案表。
 *
 * 这里只做「后端字段 → 可读中文」的映射：级别、结果、原因都是后端给的结构化字段，页面不解析
 * 日志字符串，也不从摘要里反推级别或业务。未收录的字段/取值原样显示，不猜。
 */
import { API_BASE } from './api-streamer';

export type LogLevel = 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR';
export type CaptureKind = 'native' | 'legacy_bridge';
export type Availability = 'ready' | 'disabled' | 'unavailable';

export interface EventFields {
	values: Record<string, string | number | boolean | null>;
	quality: { redacted: number; truncated: number; rejected: number };
}

export interface EventData {
	event_uid: string;
	schema_version: number;
	instance_id: string;
	process_run_id: string;
	app_version: string;
	occurred_at_ms: number;
	sequence: number;
	level: LogLevel;
	category: string;
	event_name: string;
	message: string;
	target: string;
	capture_kind: CaptureKind;
	fields: EventFields;
}

export interface StoredEvent {
	id: number;
	ingested_at_ms: number;
	data: EventData;
	has_diagnostic: boolean;
}

export interface HealthRun {
	instance_id?: string;
	process_run_id?: string;
	state?: string;
	closed?: boolean;
	queue_depth?: number;
	dropped?: number[];
	storage_failures?: number;
	recoveries?: number;
	last_commit_ms?: number | null;
	last_error?: string | null;
}

export interface HealthSnapshot {
	schema_version?: number;
	capture_config_version?: string;
	legacy_file_health?: string;
	runs?: HealthRun[];
}

export interface ListResponse {
	version: string;
	availability: Availability;
	coverage: string;
	events: StoredEvent[];
	next_after_id: number | null;
	next_until_id: number | null;
	total: number;
	pruned_through: number;
	gap: boolean;
	unclean_shutdowns: number;
	health: HealthSnapshot;
	error: string | null;
}

export interface DiagnosticPayload {
	exit_code?: number | null;
	first_fatal?: string | null;
	tail?: string;
	total_bytes?: number;
	truncated?: boolean;
	redacted?: boolean;
}

export const ALL_LEVELS: LogLevel[] = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'];
/** 首屏默认只看这三级；DEBUG/TRACE 属于高级选项。 */
export const DEFAULT_LEVELS: LogLevel[] = ['INFO', 'WARN', 'ERROR'];

export const LEVEL_TEXT: Record<LogLevel, string> = {
	TRACE: '追踪',
	DEBUG: '调试',
	INFO: '信息',
	WARN: '警告',
	ERROR: '错误',
};

export const CATEGORY_TEXT: Record<string, string> = {
	system: '系统',
	recording: '录制',
	processing: '预处理',
	upload: '上传',
	submission: '投稿',
	auth: '认证',
	audit: '审计',
};

export const ALL_CATEGORIES = Object.keys(CATEGORY_TEXT);

/**
 * 原生事件当前的接入范围（覆盖清单 C02–C10）。系统/认证/审计还只有桥接文本，属于任务 14；
 * 空结果不等于没有异常，所以页面必须把这个边界写出来。
 */
export const NATIVE_CATEGORIES = ['recording', 'processing', 'upload', 'submission'];

export const OUTCOME_TEXT: Record<string, string> = {
	executed: '已执行',
	succeeded: '成功',
	failed: '失败',
	skipped: '跳过',
	waiting: '等待中',
	cancelled: '已取消',
	fallback: '降级',
	recovered: '已恢复',
	unknown: '结果未知',
};

/** 详情里的字段名。契约允许列表之外的键不会出现，出现了也照原样显示。 */
export const FIELD_TEXT: Record<string, string> = {
	event_name: '事件名',
	outcome: '结果',
	reason_code: '原因代码',
	message: '消息',
	error: '错误',
	live_streamer_id: '直播间',
	streamer_info_id: '录制场次',
	upload_session_id: '投稿会话',
	segment_id: '分段',
	missing_id: '补传记录',
	download_attempt_id: '本次拉流',
	upload_attempt_id: '本次上传',
	task_id: '命令任务',
	streamer_name: '主播',
	platform: '平台',
	stage: '阶段',
	phase: '子阶段',
	line: '上传线路',
	original_file: '原始文件',
	artifact_file: '产物文件',
	previous_ms: '上一时间戳(ms)',
	current_ms: '当前时间戳(ms)',
	first_ms: '首次(ms)',
	last_ms: '末次(ms)',
	max_backward_ms: '最大倒退(ms)',
	duration_ms: '耗时(ms)',
	delay_ms: '退避(ms)',
	silent_ms: '静默(ms)',
	gap_ms: '缺口(ms)',
	size_bytes: '大小(字节)',
	confirmed_bytes: '已确认(字节)',
	updated_at_ms: '更新时间(ms)',
	total_bytes: '总大小(字节)',
	count: '次数',
	pending_count: '待完成数',
	segment_order: '分段序号',
	timeout_secs: '超时(秒)',
	exit_code: '退出码',
};

/** 可作为关联筛选的身份字段；后端只允许这几个，并且必须同时给出运行实例。 */
export const ASSOC_FIELDS = [
	'streamer_info_id',
	'live_streamer_id',
	'upload_session_id',
	'segment_id',
	'missing_id',
	'download_attempt_id',
	'upload_attempt_id',
	'task_id',
] as const;
export type AssocField = (typeof ASSOC_FIELDS)[number];

export type RangeKey = '1h' | '24h' | '7d' | 'all' | 'custom';

export const RANGE_TEXT: Record<RangeKey, string> = {
	'1h': '最近 1 小时',
	'24h': '最近 24 小时',
	'7d': '最近 7 天',
	all: '全部时间',
	custom: '自定义',
};

export interface EventFilters {
	levels: LogLevel[];
	categories: string[];
	keyword: string;
	eventName: string;
	captureKind: 'native' | 'legacy_bridge' | 'all';
	range: RangeKey;
	/** 仅在 range === 'custom' 时有效，单位毫秒。 */
	sinceMs?: number;
	untilMs?: number;
	instanceId: string;
	assocKey: AssocField | '';
	assocValue: string;
}

export const DEFAULT_FILTERS: EventFilters = {
	levels: DEFAULT_LEVELS,
	categories: [],
	keyword: '',
	eventName: '',
	captureKind: 'native',
	range: '24h',
	instanceId: '',
	assocKey: '',
	assocValue: '',
};

const RANGE_MS: Record<Exclude<RangeKey, 'all' | 'custom'>, number> = {
	'1h': 3600_000,
	'24h': 24 * 3600_000,
	'7d': 7 * 24 * 3600_000,
};

/** 把筛选条件翻成查询参数。时间下限按「现在」算，所以每次查询都重新求值。 */
export function filterParams(filters: EventFilters, now = Date.now()): URLSearchParams {
	const params = new URLSearchParams();
	if (filters.levels.length > 0 && filters.levels.length < ALL_LEVELS.length) {
		params.set('levels', filters.levels.join(','));
	}
	if (filters.categories.length > 0) params.set('categories', filters.categories.join(','));
	if (filters.keyword.trim()) params.set('keyword', filters.keyword.trim());
	if (filters.eventName.trim()) params.set('event_name', filters.eventName.trim());
	params.set('capture_kind', filters.captureKind);
	if (filters.range === 'custom') {
		if (filters.sinceMs) params.set('since_ms', String(filters.sinceMs));
		if (filters.untilMs) params.set('until_ms', String(filters.untilMs));
	} else if (filters.range !== 'all') {
		params.set('since_ms', String(now - RANGE_MS[filters.range]));
	}
	if (filters.instanceId) params.set('instance_id', filters.instanceId);
	if (filters.assocKey && filters.assocValue && filters.instanceId) {
		params.set('assoc_key', filters.assocKey);
		params.set('assoc_value', filters.assocValue);
	}
	return params;
}

export function listUrl(filters: EventFilters, extra: Record<string, string | number>): string {
	const params = filterParams(filters);
	for (const [key, value] of Object.entries(extra)) params.set(key, String(value));
	return `${API_BASE}/v1/log-events?${params.toString()}`;
}

export function streamUrl(filters: EventFilters, afterId: number, limit: number): string {
	const params = filterParams(filters);
	params.set('after_id', String(afterId));
	params.set('limit', String(limit));
	return `${API_BASE}/v1/log-events/stream?${params.toString()}`;
}

export function exportUrl(filters: EventFilters, format: 'jsonl' | 'csv'): string {
	const params = filterParams(filters);
	if (format === 'csv') params.set('format', 'csv');
	return `${API_BASE}/v1/log-events/export?${params.toString()}`;
}

export function levelText(level: LogLevel): string {
	return LEVEL_TEXT[level] ?? level;
}

export function categoryText(category: string): string {
	return CATEGORY_TEXT[category] ?? category;
}

export function fieldText(key: string): string {
	return FIELD_TEXT[key] ?? key;
}

export const LOCAL_TIME_ZONE =
	typeof Intl !== 'undefined' ? Intl.DateTimeFormat().resolvedOptions().timeZone : 'local';

/** 事件时间精确到毫秒；时区跟随浏览器，并在页面上写明是哪个时区。 */
export function formatMs(ms: number): string {
	const date = new Date(ms);
	const pad = (value: number, size = 2) => String(value).padStart(size, '0');
	return (
		`${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
		`${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.` +
		`${pad(date.getMilliseconds(), 3)}`
	);
}

export function formatBytes(bytes: number): string {
	if (!Number.isFinite(bytes) || bytes < 0) return '—';
	const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
	let value = bytes;
	let unit = 0;
	while (value >= 1024 && unit < units.length - 1) {
		value /= 1024;
		unit += 1;
	}
	return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

export function relativeText(ms: number, now = Date.now()): string {
	const delta = Math.max(0, now - ms);
	if (delta < 1000) return '刚刚';
	if (delta < 60_000) return `${Math.floor(delta / 1000)} 秒前`;
	if (delta < 3600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
	if (delta < 86_400_000) return `${Math.floor(delta / 3600_000)} 小时前`;
	return `${Math.floor(delta / 86_400_000)} 天前`;
}

/** 事件库里聚合出的写入健康：连接正常不代表写进去了，所以这两件事分开显示。 */
export function storageTrouble(health: HealthSnapshot | undefined): string | null {
	const runs = health?.runs ?? [];
	let dropped = 0;
	let failures = 0;
	let lastError: string | null = null;
	for (const run of runs) {
		dropped += (run.dropped ?? []).reduce((sum, value) => sum + (value ?? 0), 0);
		failures += run.storage_failures ?? 0;
		if (run.last_error) lastError = run.last_error;
	}
	if (dropped === 0 && failures === 0) return null;
	const parts: string[] = [];
	if (dropped > 0) parts.push(`已丢弃 ${dropped} 条`);
	if (failures > 0) parts.push(`写入失败 ${failures} 次`);
	if (lastError) parts.push(`最近错误：${lastError}`);
	return parts.join('；');
}

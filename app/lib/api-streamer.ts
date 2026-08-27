// Fetcher implementation. // The extra argument will be passed via the `arg` property of the 2nd parameter.// In the example below, `arg` will be `'my_token'`
export const API_BASE = process.env.NEXT_PUBLIC_API_SERVER ?? '';
export async function sendRequest<T>(url: string, { arg }: { arg: T }) {
	const res = await fetch(API_BASE + url, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(arg),
	});
	await handleResponse(res);
	return res.json();
}

export const fetcher = async (input: RequestInfo | URL, init?: RequestInit) => {
	const res = await fetch(API_BASE + input, init);
	await handleResponse(res);
	return res.json();
};

export const proxy = async (input: RequestInfo | URL, init?: RequestInit) => {
	const res = await fetch(API_BASE + input, init);
	await handleResponse(res);
	return res;
};

export async function requestDelete<T>(url: string, { arg }: { arg: T }) {
	const res = await fetch(`${API_BASE}${url}/${arg}`, { method: 'DELETE' });
	await handleResponse(res);
	return res;
}

export async function put<T>(url: string, { arg }: { arg: T }) {
	const res = await fetch(`${API_BASE}${url}`, {
		method: 'PUT',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(arg),
	});
	await handleResponse(res);
	return res;
}

export interface RecordingLeaseEntity {
	id: number;
	expires_at: string;
	customer_note: string;
	state: 'scheduled' | 'grace_current_session' | 'expired_paused';
	effective_paused_at?: string | null;
	notification_status: 'not_ready' | 'pending' | 'sending' | 'failed' | 'sent' | 'not_configured';
	last_notification_error?: string | null;
	notified_at?: string | null;
}

export interface RecordingLeaseMutationResponse {
	recording_lease: RecordingLeaseEntity | null;
	server_now: string;
}

export async function setRecordingState(id: number, paused: boolean) {
	const res = await fetch(`${API_BASE}/v1/streamers/${id}/recording-state`, {
		method: 'PUT',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ paused }),
	});
	await handleResponse(res);
}

export interface CheckStreamResponse {
	/** 后端给出的结论标识，前端据此决定提示语气；message 是可以直接展示的中文说明。 */
	outcome:
		| 'started'
		| 'offline'
		| 'already_recording'
		| 'checking'
		| 'paused'
		| 'no_upload_template'
		| 'download_pool_full'
		| 'lease_rejected';
	message: string;
}

/** 立刻检查一次直播流；开播的话后端会当场接上录制，不必等下一轮轮询。 */
export async function checkStreamNow(id: number): Promise<CheckStreamResponse> {
	const res = await fetch(`${API_BASE}/v1/streamers/${id}/check`, { method: 'POST' });
	await handleResponse(res);
	return res.json();
}

export async function saveRecordingLease(
	id: number,
	payload: { expires_at: string; customer_note: string; expected_lease_id: number | null },
): Promise<RecordingLeaseMutationResponse> {
	const res = await fetch(`${API_BASE}/v1/streamers/${id}/recording-lease`, {
		method: 'PUT',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify(payload),
	});
	await handleResponse(res);
	return res.json();
}

export async function clearRecordingLease(
	id: number,
	leaseId: number,
): Promise<RecordingLeaseMutationResponse> {
	const res = await fetch(`${API_BASE}/v1/streamers/${id}/recording-lease/${leaseId}`, {
		method: 'DELETE',
	});
	await handleResponse(res);
	return res.json();
}

async function handleResponse(res: Response) {
	// 如果未登录，统一跳转
	if (res.status === 401) {
		// 可选：清理本地状态/缓存
		// localStorage.removeItem('token') 等

		// 跳转登录（带回跳）
		const returnTo = encodeURIComponent(window.location.pathname + window.location.search);
		window.location.href = `/login?next=${returnTo}`;
		// 抛错让 SWR 知道失败（别返回 json）
		throw new Error('Unauthorized');
	}

	if (!res.ok) {
		throw new Error(await describeError(res));
	}
	return res;
}

/** 状态码到中文提示的兜底映射。服务端自己给了 JSON 错误时不会走到这里。 */
const STATUS_MESSAGES: Record<number, string> = {
	400: '请求参数有误，请检查后重试',
	403: '没有权限执行该操作',
	404: '请求的资源不存在',
	409: '当前状态下无法执行该操作，请刷新后查看最新状态',
	429: '请求过于频繁，请稍后再试',
	500: '服务端内部错误，请查看实时日志',
	502: '网关错误，服务端可能正在重启，请稍后重试',
	503: '服务暂时不可用，请稍后重试',
	504: '服务端处理超时，任务可能仍在后台执行，请刷新查看状态',
};

/** Toast 里塞不下一整页 HTML，超长正文一律截断。 */
const MAX_DETAIL_LENGTH = 300;

/**
 * 把一个失败响应变成一句能读的话。
 *
 * 这里原本直接把响应体原文 throw 出去，于是反向代理返回的 504 页面会把整段 OpenResty HTML
 * 弹进 Toast——既看不出发生了什么，也盖住了半个屏幕。现在只透传服务端自己给的结构化错误，
 * 其余按状态码翻译。
 */
async function describeError(res: Response): Promise<string> {
	const contentType = res.headers.get('content-type') ?? '';
	const fallback = STATUS_MESSAGES[res.status] ?? `请求失败（HTTP ${res.status}）`;

	if (contentType.includes('application/json')) {
		const detail = await res
			.json()
			.then((body: unknown) => {
				if (typeof body === 'string') return body;
				if (body && typeof body === 'object') {
					const record = body as Record<string, unknown>;
					for (const key of ['message', 'error', 'detail']) {
						if (typeof record[key] === 'string') return record[key] as string;
					}
					return JSON.stringify(body);
				}
				return '';
			})
			.catch(() => '');
		return detail ? truncate(detail) : fallback;
	}

	// 纯文本的短错误（后端很多 handler 就是 `(StatusCode, &str)`）仍然值得展示；
	// HTML 一律丢掉，它从来不是给人看的。
	if (contentType.includes('text/html')) return fallback;
	const text = await res.text().catch(() => '');
	const trimmed = text.trim();
	if (!trimmed || trimmed.startsWith('<')) return fallback;
	return truncate(trimmed);
}

function truncate(detail: string) {
	return detail.length > MAX_DETAIL_LENGTH ? `${detail.slice(0, MAX_DETAIL_LENGTH)}…` : detail;
}

type Credit = {
	username: string;
	uid: number;
};

export interface StudioEntity {
	id: number;
	template_name: string;
	user_cookie: string;
	copyright: number;
	copyright_source: string;
	tid: number;
	cover_path: string;
	cover_template?: string;
	/** 封面背景图文件名（留空=纯黑底）。存文件名不存路径，实际路径由服务端拼接。 */
	cover_background?: string;
	title: string;
	description: string;
	dynamic: string;
	tags: string[];
	dtime: number;
	// interactive: number;
	mission_id?: number;
	dolby: number;
	hires: number;
	no_reprint: number;
	is_only_self: number;
	up_selection_reply: number;
	up_close_reply: number;
	up_close_danmu: number;
	charging_pay: number;
	credits: Credit[];
	uploader: string;
	extra_fields?: string;
}

export interface LiveStreamerEntity {
	id: number;
	url: string;
	remark: string;
	filename_prefix?: string;
	split_time?: number;
	split_size?: number;
	upload_id?: number;
	upload_streamers_id?: number | null;
	status?: string;
	upload_status?: string;
	statusTag?: React.ReactNode;
	format?: string;
    time_range?: string | Date[];
    excluded_keywords?: string[];
	preprocessor?: Record<'run', string>[];
	segment_processor?: Record<'run', string>[];
	downloaded_processor?: Record<'run', string>[];
	postprocessor?: (Record<'run' | 'mv', string> | 'rm')[];
	opt_args?: string[];
	override?: Record<string, any>;
	recording_quality?: string;
	recording_lease?: RecordingLeaseEntity | null;
	server_now?: string;
	/** 主播级封面背景图文件名，覆盖所属上传模板的同名设置；留空则回退到模板的背景。 */
	cover_background?: string;
}

export interface BiliType {
	id: number;
	children: BiliType[];
	name: string;
	desc: string;
}

export interface User {
	id: number;
	name: string;
	value: string;
	platform: string;
}

export interface FileList {
	key: number;
	name: string;
	updateTime: number;
	size: number;
}

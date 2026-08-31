/**
 * 日志入口的默认页开关。
 *
 * P3/16 只增加「日志与事件（试用）」入口，`/logviewer` 仍然是旧的文件日志页；P4/17 把这里
 * （或构建期的 `NEXT_PUBLIC_LOG_EVENTS_DEFAULT`）改成 `1` 就能切换默认页，改回去就是回退。
 * 两个方向都不需要动路由或页面代码：新页始终能从 `/log-events` 打开，旧页始终能从
 * `/logviewer/legacy` 打开，切换只决定 `/logviewer` 渲染哪一个、导航怎么标。
 */
export const LOG_EVENTS_IS_DEFAULT = (process.env.NEXT_PUBLIC_LOG_EVENTS_DEFAULT ?? '0') === '1';

export interface LogNavEntry {
	itemKey: string;
	text: string;
	href: string;
}

/** 侧边导航里的两个日志入口。顺序即展示顺序，第一个是当前的默认页。 */
export const LOG_NAV_ENTRIES: LogNavEntry[] = LOG_EVENTS_IS_DEFAULT
	? [
			{ itemKey: 'logViewer', text: '日志与事件', href: '/logviewer' },
			{ itemKey: 'logEvents', text: '实时日志（旧）', href: '/logviewer/legacy' },
		]
	: [
			{ itemKey: 'logViewer', text: '实时日志', href: '/logviewer' },
			{ itemKey: 'logEvents', text: '日志与事件（试用）', href: '/log-events' },
		];

export const LOG_NAV_ROUTES: Record<string, string> = Object.fromEntries(
	LOG_NAV_ENTRIES.map((entry) => [entry.itemKey, entry.href]),
);

/** 出问题时回旧页的地址；默认页是谁，旧页的入口就换到哪一个路由。 */
export const LEGACY_LOG_HREF = LOG_EVENTS_IS_DEFAULT ? '/logviewer/legacy' : '/logviewer';

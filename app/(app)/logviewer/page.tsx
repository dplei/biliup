'use client';
import LegacyLogViewer from '../../ui/logviewer/LegacyLogViewer';
import LogEventsView from '../../ui/logevents/LogEventsView';
import { LOG_EVENTS_IS_DEFAULT } from '../../lib/log-view-config';

/**
 * 日志入口的默认页。P3/16 保持旧的文件日志页不变，P4/17 只改
 * `app/lib/log-view-config.ts` 里的开关就能切到新页，改回去即回退。
 */
export default function LogViewerPage() {
	return LOG_EVENTS_IS_DEFAULT ? <LogEventsView preview={false} /> : <LegacyLogViewer />;
}

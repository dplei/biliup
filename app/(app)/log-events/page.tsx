'use client';
import LogEventsView from '../../ui/logevents/LogEventsView';
import { LOG_EVENTS_IS_DEFAULT } from '../../lib/log-view-config';

/** 新事件页的固定入口：默认页开关怎么改，这个地址都能打开它。 */
export default function LogEventsPage() {
	return <LogEventsView preview={!LOG_EVENTS_IS_DEFAULT} />;
}

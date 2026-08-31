'use client';
import LegacyLogViewer from '../../../ui/logviewer/LegacyLogViewer';

/** 旧的文件日志页的固定入口：新页成为默认之后，这里仍然能看到原始文件与静态下载。 */
export default function LegacyLogViewerPage() {
	return <LegacyLogViewer />;
}

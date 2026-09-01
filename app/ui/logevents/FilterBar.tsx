'use client';
import { useEffect, useState } from 'react';
import type { CSSProperties, FC, ReactNode } from 'react';
import {
	Button,
	Checkbox,
	CheckboxGroup,
	Collapsible,
	DatePicker,
	Input,
	Select,
	Tooltip,
	Typography,
} from '@douyinfe/semi-ui';
import { IconChevronDown, IconChevronUp, IconSearch } from '@douyinfe/semi-icons';
import {
	ALL_CATEGORIES,
	ALL_LEVELS,
	ASSOC_FIELDS,
	AssocField,
	CATEGORY_TEXT,
	DEFAULT_FILTERS,
	EventFilters,
	LOCAL_TIME_ZONE,
	LogLevel,
	RANGE_TEXT,
	RangeKey,
	fieldText,
	levelText,
} from '../../lib/log-events';

/**
 * Semi 2.102 的 `Select` props 在 strict 模式下会把类型实例化推到 TS 的深度上限，本文件里
 * 每个用法都报 TS2589 并让 `next build` 失败。这里只把它的 props 收窄成实际用到的那几个，
 * 组件本身和运行时行为不变；prop 名与取值仍然受检查，`Select.Option` 继续用原组件。
 */
type FilterSelectProps = {
	multiple?: boolean;
	filter?: boolean;
	showClear?: boolean;
	disabled?: boolean;
	maxTagCount?: number;
	placeholder?: string;
	style?: CSSProperties;
	value?: string | string[];
	onChange?: (value: unknown) => void;
	optionList?: { value: string; label: string }[];
	children?: ReactNode;
};
const FilterSelect = Select as unknown as FC<FilterSelectProps>;

/** 快速筛选只放这三级；DEBUG/TRACE 在更多条件里，避免首屏堆字段。 */
const QUICK_LEVELS: LogLevel[] = ['INFO', 'WARN', 'ERROR'];

const LEVEL_COLOR: Record<string, string> = {
	INFO: 'var(--semi-color-info)',
	WARN: 'var(--semi-color-warning)',
	ERROR: 'var(--semi-color-danger)',
};

export interface StreamerOption {
	value: string;
	label: string;
}

interface Props {
	filters: EventFilters;
	onChange: (filters: EventFilters) => void;
	/** 各级别在「其余筛选条件」下的命中数；undefined 表示还在统计，不显示 0。 */
	levelCounts: Partial<Record<LogLevel, number>> | undefined;
	streamers: StreamerOption[];
	instances: string[];
	/** 页面从最近一条事件推断出的运行实例；关联筛选必须带实例，否则后端会拒绝。 */
	defaultInstance: string;
}

export default function FilterBar({
	filters,
	onChange,
	levelCounts,
	streamers,
	instances,
	defaultInstance,
}: Props) {
	const [advanced, setAdvanced] = useState(
		Boolean(filters.eventName || filters.assocKey || filters.captureKind !== 'native'),
	);
	const [keyword, setKeyword] = useState(filters.keyword);

	useEffect(() => setKeyword(filters.keyword), [filters.keyword]);

	const patch = (part: Partial<EventFilters>) => onChange({ ...filters, ...part });

	const toggleLevel = (level: LogLevel) => {
		const next = filters.levels.includes(level)
			? filters.levels.filter((value) => value !== level)
			: [...filters.levels, level];
		patch({ levels: next.length > 0 ? next : DEFAULT_FILTERS.levels });
	};

	const streamerValue =
		filters.assocKey === 'live_streamer_id' && filters.assocValue ? filters.assocValue : '';

	return (
		<div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
			<div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 8 }}>
				<Typography.Text type="secondary" size="small">
					按严重程度
				</Typography.Text>
				{QUICK_LEVELS.map((level) => {
					const active = filters.levels.includes(level);
					const count = levelCounts?.[level];
					return (
						<Button
							key={level}
							size="small"
							theme={active ? 'light' : 'borderless'}
							aria-pressed={active}
							onClick={() => toggleLevel(level)}
							style={{
								color: active ? LEVEL_COLOR[level] : 'var(--semi-color-text-2)',
								fontWeight: active ? 600 : 400,
							}}
						>
							{levelText(level)}
							<span style={{ marginLeft: 4, fontFamily: 'inherit', fontSize: 12 }}>
								{levelCounts === undefined ? '统计中' : count === undefined ? '—' : count}
							</span>
						</Button>
					);
				})}
			</div>
			<div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
				<FilterSelect
					multiple
					maxTagCount={2}
					placeholder="全部业务类型"
					style={{ minWidth: 180, flex: '1 1 180px' }}
					value={filters.categories}
					onChange={(value) => patch({ categories: (value as string[]) ?? [] })}
				>
					{ALL_CATEGORIES.map((category) => (
						<Select.Option key={category} value={category}>
							{CATEGORY_TEXT[category]}
						</Select.Option>
					))}
				</FilterSelect>
				<Tooltip
					content={
						filters.instanceId || defaultInstance
							? '按直播间关联筛选，作用于当前选定的运行实例'
							: '关联筛选需要运行实例；页面还没读到任何事件，暂时无法按主播过滤。'
					}
				>
					<FilterSelect
						filter
						showClear
						disabled={!filters.instanceId && !defaultInstance}
						placeholder="全部主播"
						style={{ minWidth: 160, flex: '1 1 160px' }}
						value={streamerValue || undefined}
						onChange={(value) =>
							patch(
								value
									? {
											assocKey: 'live_streamer_id',
											assocValue: String(value),
											instanceId: filters.instanceId || defaultInstance,
										}
									: { assocKey: '', assocValue: '' },
							)
						}
						optionList={streamers}
					/>
				</Tooltip>
				<FilterSelect
					style={{ minWidth: 140 }}
					value={filters.range}
					onChange={(value) => patch({ range: value as RangeKey })}
				>
					{(Object.keys(RANGE_TEXT) as RangeKey[]).map((key) => (
						<Select.Option key={key} value={key}>
							{RANGE_TEXT[key]}
						</Select.Option>
					))}
				</FilterSelect>
				{filters.range === 'custom' ? (
					<DatePicker
						type="dateTimeRange"
						style={{ minWidth: 320, flex: '1 1 320px' }}
						value={
							filters.sinceMs && filters.untilMs
								? [new Date(filters.sinceMs), new Date(filters.untilMs)]
								: undefined
						}
						onChange={(value) => {
							const range = value as Date[] | undefined;
							patch({
								sinceMs: range?.[0]?.getTime(),
								untilMs: range?.[1]?.getTime(),
							});
						}}
					/>
				) : null}
				<Input
					prefix={<IconSearch />}
					showClear
					placeholder="搜索摘要"
					style={{ minWidth: 180, flex: '1 1 180px' }}
					value={keyword}
					onChange={setKeyword}
					onKeyDown={(event) => {
						// 回车立即生效；离焦兜底，避免输完不动就以为没搜。
						if (event.key === 'Enter') patch({ keyword });
					}}
					onBlur={() => keyword !== filters.keyword && patch({ keyword })}
				/>
			</div>
			<div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
				<Button
					size="small"
					theme="borderless"
					icon={advanced ? <IconChevronUp /> : <IconChevronDown />}
					onClick={() => setAdvanced(!advanced)}
				>
					更多条件
				</Button>
				<Typography.Text type="tertiary" size="small">
					时间按 {LOCAL_TIME_ZONE} 显示
				</Typography.Text>
			</div>
			<Collapsible isOpen={advanced}>
				<div
					style={{
						display: 'flex',
						flexWrap: 'wrap',
						gap: 8,
						padding: '8px 0',
						alignItems: 'center',
					}}
				>
					<CheckboxGroup
						direction="horizontal"
						value={filters.levels}
						onChange={(value) =>
							patch({ levels: ((value as LogLevel[]) ?? []).length > 0 ? (value as LogLevel[]) : DEFAULT_FILTERS.levels })
						}
					>
						{ALL_LEVELS.map((level) => (
							<Checkbox key={level} value={level}>
								{levelText(level)}
							</Checkbox>
						))}
					</CheckboxGroup>
					<FilterSelect
						style={{ minWidth: 170 }}
						value={filters.captureKind}
						onChange={(value) => patch({ captureKind: value as EventFilters['captureKind'] })}
					>
						<Select.Option value="native">仅原生事件</Select.Option>
						<Select.Option value="legacy_bridge">仅桥接诊断</Select.Option>
						<Select.Option value="all">原生 + 桥接</Select.Option>
					</FilterSelect>
					<Input
						showClear
						placeholder="事件名，如 recording.segment_closed"
						style={{ minWidth: 240, flex: '1 1 240px' }}
						value={filters.eventName}
						onChange={(value) => patch({ eventName: value })}
					/>
					<FilterSelect
						showClear
						placeholder="运行实例"
						style={{ minWidth: 180 }}
						value={filters.instanceId || undefined}
						onChange={(value) => patch({ instanceId: value ? String(value) : '' })}
						optionList={instances.map((instance) => ({ value: instance, label: instance }))}
					/>
					<FilterSelect
						showClear
						placeholder="关联字段"
						style={{ minWidth: 150 }}
						value={filters.assocKey || undefined}
						onChange={(value) => patch({ assocKey: (value as AssocField) ?? '' })}
						optionList={ASSOC_FIELDS.map((field) => ({ value: field, label: fieldText(field) }))}
					/>
					<Input
						showClear
						placeholder="关联取值"
						style={{ minWidth: 200, flex: '1 1 200px' }}
						value={filters.assocValue}
						onChange={(value) => patch({ assocValue: value })}
					/>
				</div>
			</Collapsible>
		</div>
	);
}

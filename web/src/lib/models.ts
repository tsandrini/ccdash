// The models a session can be started on, as the start sheet lists them:
// the aliases Claude Code resolves itself (they follow the newest release)
// and the versions by their full names. Anything else `claude --model`
// takes can still be typed.

import type { Choice } from './components/ui/smart-select/index.js';

const LATEST = 'Latest';
const VERSIONS = 'Versions';

const version = (label: string, value: string, hint: string): Choice => ({
	value,
	label,
	hint,
	meta: value,
	group: VERSIONS
});

/** The start sheet's models; `''` leaves the choice to the host. */
export const MODELS: Choice[] = [
	{ value: '', label: 'Default', hint: "The host's own setting", group: LATEST },
	{
		value: 'fable',
		label: 'Fable',
		hint: 'For your toughest challenges',
		meta: 'fable',
		group: LATEST
	},
	{
		value: 'opus',
		label: 'Opus',
		hint: 'For complex work and everyday tasks',
		meta: 'opus',
		group: LATEST
	},
	{
		value: 'sonnet',
		label: 'Sonnet',
		hint: 'Fast and capable, for everyday coding',
		meta: 'sonnet',
		group: LATEST
	},
	{
		value: 'haiku',
		label: 'Haiku',
		hint: 'Fastest, for quick and small tasks',
		meta: 'haiku',
		group: LATEST
	},
	version('Fable 5.2', 'claude-fable-5-2', 'Most capable'),
	version('Fable 5.1', 'claude-fable-5-1', 'Previous Fable'),
	version('Fable 5', 'claude-fable-5', 'First Fable'),
	version('Opus 5.5', 'claude-opus-5-5', 'Current Opus'),
	version('Opus 5', 'claude-opus-5', 'Previous Opus'),
	version('Opus 4.8', 'claude-opus-4-8', 'Older Opus'),
	version('Opus 4.7', 'claude-opus-4-7', 'Older Opus'),
	version('Opus 4.6', 'claude-opus-4-6', 'Older Opus'),
	version('Opus 4.5', 'claude-opus-4-5', 'Older Opus'),
	version('Sonnet 5.5', 'claude-sonnet-5-5', 'Current Sonnet'),
	version('Sonnet 5', 'claude-sonnet-5', 'Previous Sonnet'),
	version('Sonnet 4.6', 'claude-sonnet-4-6', 'Older Sonnet'),
	version('Haiku 5.5', 'claude-haiku-5-5', 'Current Haiku'),
	version('Haiku 4.5', 'claude-haiku-4-5', 'Previous Haiku, 200K context')
];

/** The effort levels a session can start on (`EFFORTS` in src/actions.rs);
 * `''` leaves it to the model and the host. */
export const EFFORTS: Choice[] = [
	{ value: '', label: 'Default', hint: "The model's or the host's own" },
	{ value: 'low', label: 'Low', hint: 'Quick and terse, for simple tasks' },
	{ value: 'medium', label: 'Medium', hint: 'Balanced' },
	{ value: 'high', label: 'High', hint: 'Thorough, for demanding work' },
	{ value: 'xhigh', label: 'Extra high', hint: 'Long coding and agentic work', meta: 'xhigh' },
	{ value: 'max', label: 'Max', hint: 'When correctness matters more than cost' }
];

// NOTE: mirrors `valid_model` in src/actions.rs, which has the last word
const MODEL_NAME = /^[A-Za-z0-9._[\]][A-Za-z0-9._[\]-]{0,63}$/;

/**
 * A row for a model name that is not in {@link MODELS}, so a release this
 * list does not know yet can be started on.
 *
 * @param name - What was typed.
 * @returns The row, or null when the collector would refuse the name.
 *
 * @example
 * customModel('claude-opus-5-5[1m]')?.label // 'claude-opus-5-5[1m]'
 */
export function customModel(name: string): Choice | null {
	return MODEL_NAME.test(name)
		? { value: name, label: name, hint: 'Use this model name as typed' }
		: null;
}

// Formatting shared by every page.

import type { ToolRun } from './api/types';

export function fmtDuration(ms: number): string {
	if (!Number.isFinite(ms)) return '';
	const s = Math.max(0, Math.round(ms / 1000));
	if (s < 60) return `${s}s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m${s % 60 ? ` ${s % 60}s` : ''}`;
	const h = Math.floor(m / 60);
	if (h < 24) return `${h}h${m % 60 ? ` ${m % 60}m` : ''}`;
	const d = Math.floor(h / 24);
	return `${d}d${h % 24 ? ` ${h % 24}h` : ''}`;
}

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const stampFmt = new Intl.DateTimeFormat(undefined, {
	month: 'short',
	day: 'numeric',
	hour: '2-digit',
	minute: '2-digit'
});
const dateFmt = new Intl.DateTimeFormat(undefined, {
	weekday: 'short',
	month: 'short',
	day: 'numeric'
});

export const fmtTime = (ts: number) => timeFmt.format(new Date(ts));
export const fmtStamp = (ts: number) => stampFmt.format(new Date(ts));
export const fmtDate = (ts: number) => dateFmt.format(new Date(ts));

export const fmtUsd = (usd: number) => (usd >= 10 ? `$${usd.toFixed(0)}` : `$${usd.toFixed(2)}`);

export const plural = (n: number, word: string, many = `${word}s`) =>
	`${n} ${n === 1 ? word : many}`;

export function fmtBytes(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(0)} kB`;
	if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(0)} MB`;
	return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
}

export const known = <T>(v: T | null | undefined): v is T => v !== null && v !== undefined;

/** The last path component, for "where" lines. */
export const place = (cwd: string) => cwd.replace(/\/+$/, '').split('/').pop() || cwd;

/** Rough plain text of markdown, for one-line previews. */
export function plain(text: string | null | undefined): string {
	if (!text) return '';
	return text
		.replace(/```[\s\S]*?```/g, ' ')
		.replace(/`([^`]*)`/g, '$1')
		.replace(/^#+\s*/gm, '')
		.replace(/[*_~>]/g, '')
		.replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
		.replace(/\s+/g, ' ')
		.trim();
}

/** `mcp__plugin_hm_playwright__browser_click` → `playwright: browser_click`. */
export function toolName(name: string): string {
	const m = name.match(/^mcp__(?:plugin_)?(?:[a-z0-9]+_)?([a-z0-9-]+)__(.+)$/i);
	return m ? `${m[1]}: ${m[2]}` : name;
}

export const toolLabel = (tool: ToolRun) =>
	tool.summary ? `${toolName(tool.name)} ${tool.summary}` : toolName(tool.name);

export type Level = 'normal' | 'warning' | 'critical';

export const meterLevel = (pct: number): Level =>
	pct >= 90 ? 'critical' : pct >= 75 ? 'warning' : 'normal';

/** A tool's one-line description for lists. */
export const shorten = (text: string, max = 120) =>
	text.length > max ? `${text.slice(0, max - 1)}…` : text;

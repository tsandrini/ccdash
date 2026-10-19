// The navigation registry: every area the sidebar shows. Adding an area
// is one entry here plus its route folder.

import type { Component } from 'svelte';
import ActivityIcon from '@lucide/svelte/icons/activity';
import BarChart3Icon from '@lucide/svelte/icons/bar-chart-3';
import FolderKanbanIcon from '@lucide/svelte/icons/folder-kanban';
import HistoryIcon from '@lucide/svelte/icons/history';
import ListIcon from '@lucide/svelte/icons/list';
import SettingsIcon from '@lucide/svelte/icons/settings';
import type { Session } from './api/types';
import { shown } from './session';

export type Area = {
	href: string;
	label: string;
	icon: Component;
	/** Served by the previous panel until it is ported. */
	legacy?: boolean;
	/** Whether a path belongs to the area. */
	match: (path: string) => boolean;
	/** A count shown next to the entry. */
	badge?: (sessions: Session[]) => number;
};

const under = (prefix: string) => (path: string) =>
	path === prefix || path.startsWith(`${prefix}/`);

export const areas: Area[] = [
	{
		href: '/',
		label: 'Now',
		icon: ActivityIcon,
		match: (path) => path === '/',
		badge: (sessions) =>
			sessions.filter((s) => s.activity === 'permission' || shown(s) === 'your_turn').length
	},
	{
		href: '/sessions',
		label: 'Sessions',
		icon: ListIcon,
		match: under('/sessions'),
		badge: (sessions) => sessions.filter((s) => s.activity !== 'gone').length
	},
	{
		href: '/projects',
		label: 'Projects',
		icon: FolderKanbanIcon,
		match: (path) => under('/projects')(path) || under('/notes')(path)
	},
	{
		href: '/v1/#view=history',
		label: 'History',
		icon: HistoryIcon,
		legacy: true,
		match: () => false
	},
	{
		href: '/v1/#view=analytics',
		label: 'Analytics',
		icon: BarChart3Icon,
		legacy: true,
		match: () => false
	},
	{ href: '/settings', label: 'Settings', icon: SettingsIcon, match: under('/settings') }
];

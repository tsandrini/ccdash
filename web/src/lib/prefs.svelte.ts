// Preferences that stay on the device: theme, composer drafts, quick
// replies and templates.

import { browser } from '$app/env';

export type Theme = 'auto' | 'light' | 'dark';

const THEME_KEY = 'ccdash.theme';
const QUICK_KEY = 'ccdash.quick';
const TEMPLATES_KEY = 'ccdash.templates';
const DRAFT_KEY = 'ccdash.draft.';

export const DEFAULT_QUICK = [
	'Continue',
	'Yes',
	'No',
	'Explain what you did',
	'Run the tests and report'
];

export type Template = { name: string; text: string };

function load<T>(key: string, fallback: T): T {
	if (!browser) return fallback;
	try {
		const raw = localStorage.getItem(key);
		return raw ? (JSON.parse(raw) as T) : fallback;
	} catch {
		return fallback;
	}
}

class Prefs {
	theme = $state<Theme>(browser ? ((localStorage.getItem(THEME_KEY) as Theme) ?? 'auto') : 'auto');
	quick = $state<string[]>(load(QUICK_KEY, DEFAULT_QUICK));
	templates = $state<Template[]>(load(TEMPLATES_KEY, []));

	setTheme(theme: Theme) {
		this.theme = theme;
		localStorage.setItem(THEME_KEY, theme);
		this.applyTheme();
	}

	cycleTheme() {
		const order: Theme[] = ['auto', 'light', 'dark'];
		this.setTheme(order[(order.indexOf(this.theme) + 1) % order.length]);
	}

	applyTheme() {
		if (!browser) return;
		const dark =
			this.theme === 'dark' ||
			(this.theme === 'auto' && matchMedia('(prefers-color-scheme: dark)').matches);
		document.documentElement.classList.toggle('dark', dark);
		document.documentElement.style.colorScheme = dark ? 'dark' : 'light';
	}

	setQuick(list: string[]) {
		this.quick = list.map((s) => s.trim()).filter(Boolean);
		localStorage.setItem(QUICK_KEY, JSON.stringify(this.quick));
	}

	setTemplates(list: Template[]) {
		this.templates = list.filter((t) => t.name.trim() && t.text.trim());
		localStorage.setItem(TEMPLATES_KEY, JSON.stringify(this.templates));
	}

	draft(session: string): string {
		return browser ? (localStorage.getItem(DRAFT_KEY + session) ?? '') : '';
	}

	setDraft(session: string, text: string) {
		if (text) localStorage.setItem(DRAFT_KEY + session, text);
		else localStorage.removeItem(DRAFT_KEY + session);
	}
}

export const prefs = new Prefs();

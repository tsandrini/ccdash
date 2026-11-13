// Preferences that stay on the device: theme, colour scheme and the machine
// whose wallpaper it follows, the sidebar, the board grid, composer drafts,
// quick replies and templates, the last host, model and permission mode a
// session was started with.

import { browser } from '$app/env';
import type { HostStatus, Palette } from './api/types';
import { WALLPAPER, isScheme, resolveScheme } from './theme';

export type Theme = 'auto' | 'light' | 'dark';

const THEME_KEY = 'ccdash.theme';
const SCHEME_KEY = 'ccdash.scheme';
const WALL_HOST_KEY = 'ccdash.wallhost';
const PALETTE_KEY = 'ccdash.palette';
const SIDEBAR_KEY = 'ccdash.sidebar';
const GRID_KEY = 'ccdash.grid';
const QUICK_KEY = 'ccdash.quick';
const TEMPLATES_KEY = 'ccdash.templates';
const DRAFT_KEY = 'ccdash.draft.';
const VIM_KEY = 'ccdash.vim';
const VIMRC_KEY = 'ccdash.vimrc';
const EDITOR_KEY = 'ccdash.editor';
const EDIT_HOST_KEY = 'ccdash.edithost';
const START_KEY = 'ccdash.start';
const GRAPH_KEY = 'ccdash.graph';
const REPORT_SIZE_KEY = 'ccdash.reportsize';

export const DEFAULT_QUICK = [
	'Continue',
	'Yes',
	'No',
	'Explain what you did',
	'Run the tests and report'
];

export type Template = { name: string; text: string };

/** Vim keys in the composer: `auto` is on with a mouse or trackpad, off on touch. */
export type VimMode = 'auto' | 'on' | 'off';

/** Line numbers in the composer: `auto` is relative (the current line
 * absolute, as Neovim's number + relativenumber) when vim is on, else none. */
export type Numbers = 'auto' | 'off' | 'absolute' | 'relative';
/** How the composer's editor looks and behaves, beyond vim. */
export type EditorLook = {
	numbers: Numbers;
	/** Headings, emphasis, code and links set apart as you type. */
	markdown: boolean;
	/** Long lines wrap (off: they scroll sideways, as `nowrap`). */
	wrap: boolean;
	/** With vim keys: `,` then two characters jumps to a match (hop.nvim). */
	hop: boolean;
	/** The label keys of the jump, nearest match first. */
	hopKeys: string;
	/** Yanks and deletes go to the system clipboard. */
	clipboard: boolean;
	/** What a bare `p` pastes: the clipboard (the browser may ask), or the
	 * register (the browser's own paste keys read the clipboard without asking). */
	paste: 'clipboard' | 'register';
};
export const DEFAULT_EDITOR: EditorLook = {
	numbers: 'auto',
	markdown: true,
	wrap: true,
	hop: true,
	hopKeys: 'asdfghjkl',
	clipboard: true,
	paste: 'clipboard'
};
export const isVimMode = (x: unknown): x is VimMode => x === 'auto' || x === 'on' || x === 'off';

/** What the composer's vim starts with: the owner's Neovim habits, as far
 * as the emulation follows (`:help map` syntax, one command per line,
 * `"` comments). Settings edits it. */
export const DEFAULT_VIMRC = `" The panel's vim, set up like your Neovim: one ex command per line.
imap jk <Esc>
imap kj <Esc>
vmap jk <Esc>
vmap kj <Esc>
nmap <Space>w :w<CR>
nmap <Space>q :q<CR>
nmap <Space>r :noh<CR>`;

/** What the start sheet picked last; empty strings are the host's defaults. */
export type StartChoice = { host: string; model: string; mode: string; effort: string };

/** The text size a framed report shows; the report's own switch sets it. */
export type ReportSize = 'small' | 'normal' | 'big';
export const REPORT_SIZES: readonly ReportSize[] = ['small', 'normal', 'big'];
export const isReportSize = (x: unknown): x is ReportSize =>
	typeof x === 'string' && (REPORT_SIZES as readonly string[]).includes(x);

export type GraphColour = 'kind' | 'project' | 'age' | 'links' | 'recalls';
export type GraphSize = 'recalls' | 'links' | 'same';
export type GraphLayout = 'force' | 'clusters' | 'timeline';
export type GraphPhysics = { repulsion: number; distance: number; gravity: number };
/** How the note graph is drawn, not what it is filtered to. */
export type GraphLook = {
	view: '2d' | '3d';
	colour: GraphColour;
	size: GraphSize;
	layout: GraphLayout;
	physics: GraphPhysics;
};

export const DEFAULT_GRAPH: GraphLook = {
	view: '2d',
	colour: 'kind',
	size: 'recalls',
	layout: 'force',
	physics: { repulsion: 40, distance: 30, gravity: 0.05 }
};

/** The wallpaper colours in use: kept, so the theme is there before the
 * hub answers and when no machine reports. */
export type Followed = { host: string; palette: Palette };

function load<T>(key: string, fallback: T): T {
	if (!browser) return fallback;
	try {
		const raw = localStorage.getItem(key);
		return raw ? (JSON.parse(raw) as T) : fallback;
	} catch {
		return fallback;
	}
}

const schemeOf = (id: string | null) => (isScheme(id) ? id : 'neutral');

class Prefs {
	#applied: string[] = [];
	theme = $state<Theme>(browser ? ((localStorage.getItem(THEME_KEY) as Theme) ?? 'auto') : 'auto');
	scheme = $state<string>(schemeOf(browser ? localStorage.getItem(SCHEME_KEY) : null));
	/** The machine the wallpaper scheme follows; empty, the first that reports. */
	wallHost = $state<string>(browser ? (localStorage.getItem(WALL_HOST_KEY) ?? '') : '');
	followed = $state.raw<Followed | null>(load<Followed | null>(PALETTE_KEY, null));
	/** The full sidebar on wide screens; off, a rail of icons (a narrow
	 * vertical monitor). */
	sidebar = $state<boolean>(!browser || localStorage.getItem(SIDEBAR_KEY) !== 'off');
	/** A light grid behind the project pages. */
	grid = $state<boolean>(browser && localStorage.getItem(GRID_KEY) === 'on');
	quick = $state<string[]>(load(QUICK_KEY, DEFAULT_QUICK));
	templates = $state<Template[]>(load(TEMPLATES_KEY, []));
	/** Vim keys in the composer. */
	vim = $state<VimMode>(
		browser && isVimMode(localStorage.getItem(VIM_KEY))
			? (localStorage.getItem(VIM_KEY) as VimMode)
			: 'off'
	);
	vimrc = $state<string>(
		browser ? (localStorage.getItem(VIMRC_KEY) ?? DEFAULT_VIMRC) : DEFAULT_VIMRC
	);
	editor = $state<EditorLook>({ ...DEFAULT_EDITOR, ...load<Partial<EditorLook>>(EDITOR_KEY, {}) });
	/** The machine whose editor "Edit in the editor" opens; empty picks the
	 * session's host, else the first connected one that can. */
	editHost = $state<string>(browser ? (localStorage.getItem(EDIT_HOST_KEY) ?? '') : '');
	start = $state<StartChoice>({
		host: '',
		model: '',
		mode: '',
		effort: '',
		...load<Partial<StartChoice>>(START_KEY, {})
	});
	graph = $state<GraphLook>({ ...DEFAULT_GRAPH, ...load<Partial<GraphLook>>(GRAPH_KEY, {}) });
	/** Reports in the panel's frame have no storage of their own (an opaque
	 * origin), so the pick made in one report is kept here for all. */
	reportSize = $state<ReportSize>(
		browser && isReportSize(localStorage.getItem(REPORT_SIZE_KEY))
			? (localStorage.getItem(REPORT_SIZE_KEY) as ReportSize)
			: 'normal'
	);

	setTheme(theme: Theme) {
		this.theme = theme;
		localStorage.setItem(THEME_KEY, theme);
		this.applyTheme();
	}

	applyTheme() {
		if (!browser) return;
		const wanted =
			this.theme === 'dark' ||
			(this.theme === 'auto' && matchMedia('(prefers-color-scheme: dark)').matches);
		const { dark, vars } = resolveScheme(this.scheme, wanted, this.followed?.palette ?? null);
		const root = document.documentElement;
		root.classList.toggle('dark', dark);
		root.style.colorScheme = dark ? 'dark' : 'light';
		root.dataset.scheme = this.scheme;
		for (const name of this.#applied) if (!vars?.[name]) root.style.removeProperty(name);
		for (const [name, value] of Object.entries(vars ?? {})) root.style.setProperty(name, value);
		this.#applied = Object.keys(vars ?? {});
		for (const meta of document.querySelectorAll<HTMLMetaElement>('meta[name=theme-color]')) {
			meta.dataset.stock ??= meta.content;
			meta.content = vars?.['--background'] ?? meta.dataset.stock;
		}
	}

	setScheme(scheme: string) {
		this.scheme = scheme;
		localStorage.setItem(SCHEME_KEY, scheme);
		this.applyTheme();
	}

	setReportSize(size: ReportSize) {
		this.reportSize = size;
		if (size === 'normal') localStorage.removeItem(REPORT_SIZE_KEY);
		else localStorage.setItem(REPORT_SIZE_KEY, size);
	}

	setWallHost(host: string, hosts: HostStatus[]) {
		this.wallHost = host;
		localStorage.setItem(WALL_HOST_KEY, host);
		this.takePalettes(hosts);
	}

	/** Takes what the followed machine reports, as the wallpaper changes. A
	 * machine that went quiet keeps its last colours. */
	takePalettes(hosts: HostStatus[]) {
		const host = this.wallHost
			? hosts.find((h) => h.name === this.wallHost && h.palette)
			: hosts.find((h) => h.palette);
		if (!host?.palette) return;
		const next = { host: host.name, palette: host.palette };
		if (JSON.stringify(next) === JSON.stringify(this.followed)) return;
		this.followed = next;
		localStorage.setItem(PALETTE_KEY, JSON.stringify(next));
		if (this.scheme === WALLPAPER) this.applyTheme();
	}

	setSidebar(on: boolean) {
		this.sidebar = on;
		localStorage.setItem(SIDEBAR_KEY, on ? 'on' : 'off');
	}

	setGrid(on: boolean) {
		this.grid = on;
		localStorage.setItem(GRID_KEY, on ? 'on' : 'off');
	}

	setQuick(list: string[]) {
		this.quick = list.map((s) => s.trim()).filter(Boolean);
		localStorage.setItem(QUICK_KEY, JSON.stringify(this.quick));
	}

	setTemplates(list: Template[]) {
		this.templates = list.filter((t) => t.name.trim() && t.text.trim());
		localStorage.setItem(TEMPLATES_KEY, JSON.stringify(this.templates));
	}

	setVim(mode: VimMode) {
		this.vim = mode;
		localStorage.setItem(VIM_KEY, mode);
	}

	setVimrc(text: string) {
		this.vimrc = text;
		localStorage.setItem(VIMRC_KEY, text);
	}

	setEditor(changes: Partial<EditorLook>) {
		this.editor = { ...this.editor, ...changes };
		localStorage.setItem(EDITOR_KEY, JSON.stringify(this.editor));
	}

	/** The line numbers the composer shows on this device. */
	get numbers(): Exclude<Numbers, 'auto'> {
		if (this.editor.numbers !== 'auto') return this.editor.numbers;
		return this.vimOn ? 'relative' : 'off';
	}

	setEditHost(host: string) {
		this.editHost = host;
		if (host) localStorage.setItem(EDIT_HOST_KEY, host);
		else localStorage.removeItem(EDIT_HOST_KEY);
	}

	/** Whether the composer uses vim keys on this device. */
	get vimOn(): boolean {
		if (this.vim !== 'auto') return this.vim === 'on';
		return browser && !matchMedia('(pointer: coarse)').matches;
	}

	setGraph(changes: Partial<GraphLook>) {
		this.graph = { ...this.graph, ...changes };
		localStorage.setItem(GRAPH_KEY, JSON.stringify(this.graph));
	}

	setStart(choice: StartChoice) {
		this.start = choice;
		localStorage.setItem(START_KEY, JSON.stringify(choice));
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

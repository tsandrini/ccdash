import tailwindcss from '@tailwindcss/vite';
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

/** The hub `npm run dev` proxies the API, the stream and buttons to. */
const hub = process.env.CCDASH_HUB ?? 'http://127.0.0.1:7837';

export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},
			// NOTE: a single-page app: the hub serves index.html for every route
			adapter: adapter({ fallback: 'index.html', strict: false }),
			// NOTE: the app registers the worker itself, on the built origin only
			serviceWorker: { register: false }
		})
	],
	server: {
		proxy: {
			'/api': { target: hub, changeOrigin: false },
			'/act': { target: hub, changeOrigin: false }
		}
	}
});

import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig, loadEnv } from 'vite';

// The built UI lands inside the Python package; FastAPI serves it with an index.html fallback.
const out = '../backend/src/ownhealth/static';
// `pnpm dev` proxies the API and MCP to a backend on :8080 (`uv run ownhealth serve`).
const backend = loadEnv('', '.', 'OWNHEALTH_').OWNHEALTH_BACKEND ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [sveltekit({ adapter: adapter({ pages: out, assets: out, fallback: 'index.html', strict: true }) })],
	server: { proxy: { '/api': backend, '/mcp': backend, '/healthz': backend } }
});

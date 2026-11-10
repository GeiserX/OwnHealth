// See https://svelte.dev/docs/kit/types#app.d.ts
declare global {
	namespace App {}
	/** ui/package.json's version, set by vite.config.ts. */
	const __APP_VERSION__: string;
}

export {};

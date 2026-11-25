// File upload and timing summary used by the dedicated imaging spec.
declare module 'node:fs' {
	export function openAsBlob(path: string, options: { type: string }): Promise<Blob>;
	export function appendFileSync(path: string, data: string): void;
}

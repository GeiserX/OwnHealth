// The only Node globals the e2e folder reads; it does not pull in @types/node for them.
declare const process: { env: Record<string, string | undefined> };
// The one Node call the e2e makes: psql inside the test Postgres container (connections, share links).
declare module 'node:child_process' {
	export function execFileSync(file: string, args: readonly string[], options: { encoding: 'utf8' }): string;
}

// The audit checks write the anchor file a backup would write, in the folder the config picks.
declare module 'node:fs' {
	export function mkdirSync(path: string, options: { recursive: true; mode?: number }): string | undefined;
	export function writeFileSync(path: string, data: string, options?: { mode?: number }): void;
}
declare module 'node:os' {
	export function tmpdir(): string;
}
// The CGM import checks upload the synthetic exports in the repository's fixtures/cgm folder.
declare module 'node:url' {
	export function fileURLToPath(url: URL): string;
}
// The GEDCOM checks hand the file picker a file they make in memory.
declare module 'node:buffer' {
	export const Buffer: { alloc(size: number, fill: number): Uint8Array; from(text: string, encoding: 'utf8'): Uint8Array };
}

// The only Node globals the e2e folder reads; it does not pull in @types/node for them.
declare const process: { env: Record<string, string | undefined> };
// The one Node call the e2e makes: psql inside the test Postgres container (connections, share links).
declare module 'node:child_process' {
	export function execFileSync(file: string, args: readonly string[], options: { encoding: 'utf8' }): string;
}

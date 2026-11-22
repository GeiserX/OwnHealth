// The only Node global the config reads; the e2e folder does not pull in @types/node for it.
declare const process: { env: Record<string, string | undefined> };

// The one Node call the connections checks make: psql inside the test Postgres container.
declare module 'node:child_process' {
	export function execFileSync(file: string, args: string[], options: { encoding: 'utf8' }): string;
}

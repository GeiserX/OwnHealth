// The only Node global the config reads; the e2e folder does not pull in @types/node for it.
declare const process: { env: Record<string, string | undefined> };

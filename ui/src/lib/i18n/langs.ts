// The UI language registry. Adding a language is one data PR:
// - its file next to this one (`de.ts`: `export const de: Table = { ... }`, every key of dict.en), and
//   its entry below. The Table type fails `pnpm run check` on a missing or unknown key, and
//   tests/langs.test.ts fails on an empty one;
// - its code in the server registry, `ownhealth_core::langs::LANGS` (a server test keeps the two lists
//   equal), its strings in data/ and its wording-lint patterns (the server tests name each one missing).
// en and es stay in dict.ts, where every UI slice adds its keys.
import { dict, type Key } from './dict.ts';

/** Every UI string of one language. */
export type Table = Readonly<Record<Key, string>>;

export interface LangEntry {
	/** ISO 639-1, the code the server and the URL (`?lang=`) use. */
	code: string;
	/** The picker button. */
	label: string;
	/** The language's own name for itself (the picker's tooltip). */
	name: string;
	/** The BCP 47 locale for numbers, dates and plurals. */
	intl: string;
	/** Not reviewed by a native speaker yet: the picker says so. */
	beta: boolean;
	table: Table;
}

/** The languages, in picker order. The first is the default of a new login on the server. */
export const LANGS = [
	{ code: 'es', label: 'ES', name: 'Español', intl: 'es-ES', beta: false, table: dict.es },
	{ code: 'en', label: 'EN', name: 'English', intl: 'en-GB', beta: false, table: dict.en }
] as const satisfies readonly LangEntry[];

export type Lang = (typeof LANGS)[number]['code'];

/** The languages MCP speaks (`api_token.lang`), es and en whatever LANGS holds (server `MCP_LANGS`). */
export type McpLang = 'es' | 'en';

/** A server text in every registry language: `{ es, en }`. */
export type Localized<T = string> = Record<Lang, T>;
/** The same under a stem: `Suffixed<'short'>` is `{ short_es, short_en }`. */
export type Suffixed<P extends string, T = string> = { [K in Lang as `${P}_${K}`]: T };

const byCode = new Map<string, (typeof LANGS)[number]>(LANGS.map((l) => [l.code, l]));

export function isLang(v: unknown): v is Lang {
	return typeof v === 'string' && byCode.has(v);
}

/** The strings of a registry language. */
export function tableOf(lang: Lang): Table {
	return byCode.get(lang)!.table;
}

/** The Intl locale of a registry language. */
export function intlLocale(lang: Lang): string {
	return byCode.get(lang)!.intl;
}

/** The language of a browser whose languages the registry lacks (server `BROWSER_DEFAULT`; a server
 * test keeps the two equal). */
export const BROWSER_DEFAULT: Lang = 'en';

/** The first language: `?lang=`, then the saved choice, then the browser's language, else BROWSER_DEFAULT. */
export function pickLang(query: string | null, saved: string | null, browser: string | undefined): Lang {
	if (isLang(query)) return query;
	if (isLang(saved)) return saved;
	const base = browser?.toLowerCase().split('-')[0];
	return isLang(base) ? base : BROWSER_DEFAULT;
}

/** What a table lacks against English: keys missing, keys with empty text, and keys English has not. */
export function missingKeys(table: Table): { missing: string[]; empty: string[]; extra: string[] } {
	const t = table as Record<string, string>;
	const en = Object.keys(dict.en);
	return {
		missing: en.filter((k) => !(k in t)),
		empty: en.filter((k) => k in t && !String(t[k]).trim()),
		extra: Object.keys(t).filter((k) => !(k in dict.en))
	};
}

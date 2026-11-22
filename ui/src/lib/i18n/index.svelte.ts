const browser = typeof window !== 'undefined';
import type { Key } from './dict';
import { intlLocale, pickLang, tableOf, type Lang } from './langs';

export type { Key, Lang };

function initialLang(): Lang {
	if (!browser) return 'en';
	return pickLang(new URLSearchParams(location.search).get('lang'), localStorage.getItem('oh.lang'), navigator.language);
}

/** Current UI language. Read `i18n.lang`; change it with `setLang`. */
export const i18n = $state<{ lang: Lang }>({ lang: initialLang() });

export function setLang(lang: Lang): void {
	i18n.lang = lang;
	if (browser) {
		localStorage.setItem('oh.lang', lang);
		document.documentElement.lang = lang;
	}
}

/** Translate a key in the current language (the key itself when no language has it). Every registry
 *  language has every key (langs.ts), so there is no fallback to English. */
export function t(key: Key | string): string {
	return (tableOf(i18n.lang) as Record<string, string>)[key] ?? key;
}

/** Format a number the way the current language writes it (decimal comma in Spanish). */
export function fmtNum(value: number, digits?: number): string {
	return new Intl.NumberFormat(intlLocale(i18n.lang), {
		minimumFractionDigits: digits,
		maximumFractionDigits: digits ?? 2
	}).format(value);
}

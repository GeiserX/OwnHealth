const browser = typeof window !== 'undefined';
import { dict, type Key, type Lang } from './dict';

export type { Key, Lang };

function initialLang(): Lang {
	if (!browser) return 'en';
	const q = new URLSearchParams(location.search).get('lang');
	if (q === 'es' || q === 'en') return q;
	const saved = localStorage.getItem('oh.lang');
	if (saved === 'es' || saved === 'en') return saved;
	return navigator.language?.toLowerCase().startsWith('es') ? 'es' : 'en';
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

/** Translate a key in the current language (falls back to English, then the key itself). */
export function t(key: Key | string): string {
	const table = dict[i18n.lang] as Record<string, string>;
	return table[key] ?? (dict.en as Record<string, string>)[key] ?? key;
}

/** Format a number the way the current language writes it (decimal comma in Spanish). */
export function fmtNum(value: number, digits?: number): string {
	return new Intl.NumberFormat(i18n.lang === 'es' ? 'es-ES' : 'en-GB', {
		minimumFractionDigits: digits,
		maximumFractionDigits: digits ?? 2
	}).format(value);
}

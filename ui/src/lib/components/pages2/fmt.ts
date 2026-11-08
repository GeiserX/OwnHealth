// Formatting helpers for the lane I-ui-2 pages.
import { i18n, t } from '#lib/i18n/index.svelte';

/** '2026-11-07' from an ISO date-time. */
export const day = (v: string | null | undefined): string => (v ? v.slice(0, 10) : '—');

/** '07:12:04' today, '11-06 22:10' on another day (the mockup's audit format), in local time. */
export function stamp(iso: string, now = new Date()): string {
	const d = new Date(iso);
	const pad = (n: number) => String(n).padStart(2, '0');
	const same = d.toDateString() === now.toDateString();
	return same
		? `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
		: `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** '4 min ago', 'yesterday', or the date. */
export function ago(iso: string | null | undefined, now = new Date()): string {
	if (!iso) return t('i2_never');
	const s = Math.max(0, (now.getTime() - new Date(iso).getTime()) / 1000);
	const rtf = new Intl.RelativeTimeFormat(i18n.lang, { numeric: 'auto' });
	if (s < 60) return rtf.format(0, 'second');
	if (s < 3600) return rtf.format(-Math.round(s / 60), 'minute');
	if (s < 86400) return rtf.format(-Math.round(s / 3600), 'hour');
	if (s < 2 * 86400) return rtf.format(-1, 'day');
	return day(iso);
}

/** Thousands with the language's separator: 1,840 / 1.840. */
export const int = (n: number | null | undefined): string =>
	n == null ? '—' : new Intl.NumberFormat(i18n.lang === 'es' ? 'es-ES' : 'en-GB', { useGrouping: 'always' } as Intl.NumberFormatOptions).format(n);

/** Text of an untrusted field, for display as plain text (Svelte escapes it). */
export const plain = (v: { text: string } | string | null | undefined): string => (v == null ? '' : typeof v === 'string' ? v : v.text);

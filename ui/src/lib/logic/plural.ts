/** The dict key for a count: `key_one` when the language says "one" and that key exists, else `key`. */
export function pluralKey(lang: string, n: number, key: string, has: (k: string) => boolean): string {
	const cat = new Intl.PluralRules(lang === 'es' ? 'es' : 'en').select(n);
	return cat === 'one' && has(key + '_one') ? key + '_one' : key;
}

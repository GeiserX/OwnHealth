// GEDCOM import (slice 24): what the browser does with the person's own tree file. The server reads
// only structure and never a name (ownhealth-core/src/gedcom.rs). The picker still shows names: this
// file reads each INDI record's first NAME here, in the browser, numbered the way the server numbers
// people (the n-th INDI record of the file), and never sends them anywhere.

/** The text of a tree file: UTF-16 when it starts with a byte order mark, UTF-8 otherwise. */
export function decodeTree(bytes: Uint8Array): string {
	const b0 = bytes[0];
	const b1 = bytes[1];
	const enc = b0 === 0xff && b1 === 0xfe ? 'utf-16le' : b0 === 0xfe && b1 === 0xff ? 'utf-16be' : 'utf-8';
	return new TextDecoder(enc).decode(bytes).replace(/^\uFEFF/, '');
}

/** `Alex /Rivera/` as a person reads it: `Alex Rivera`. */
export function cleanName(value: string): string | null {
	const name = value.replace(/\//g, ' ').replace(/\s+/g, ' ').trim();
	return name || null;
}

const LINE = /^(\d{1,2}) (?:@[^ ]+@ )?([A-Za-z0-9_]+)(?: (.*))?$/;

/** The first NAME of each INDI record, by position; null when a record has none. */
export function namesByPosition(text: string): (string | null)[] {
	const out: (string | null)[] = [];
	let inPerson = false;
	let named = false;
	for (const raw of text.split(/\r\n|\r|\n/)) {
		const m = LINE.exec(raw.replace(/^[ \t]+/, ''));
		if (!m) continue;
		const level = Number(m[1]);
		const tag = m[2];
		if (level === 0) {
			if (tag === 'TRLR') break;
			inPerson = tag === 'INDI';
			named = false;
			if (inPerson) out.push(null);
		} else if (inPerson && !named && level === 1 && tag === 'NAME' && m[3]) {
			out[out.length - 1] = cleanName(m[3]);
			named = true;
		}
	}
	return out;
}

export type PickPerson = { i: number; sex: string; born_year: number | null; deceased: boolean; death_age: number | null };

/**
 * The people a search shows in the picker: by a name the browser read, or by a birth year. Names are
 * used only when the browser counted the same people as the server (a file it decodes differently
 * shows no names rather than wrong ones).
 */
export function pickable<P extends PickPerson>(people: P[], names: (string | null)[], query: string, limit = 30): P[] {
	const useNames = names.length === people.length;
	const q = query.trim().toLowerCase();
	const hit = (p: P) => {
		if (!q) return true;
		if (p.born_year != null && String(p.born_year).startsWith(q)) return true;
		const n = useNames ? names[p.i] : null;
		return !!n && n.toLowerCase().includes(q);
	};
	return people.filter(hit).slice(0, limit);
}

/** The payload of a `family_proposal` review item. */
export type FamilyProposal = {
	relationship?: string;
	label?: Record<string, string>;
	sex_at_birth?: string | null;
	born_year?: number | null;
	deceased?: boolean;
	death_age?: number | null;
};

/**
 * The line a family proposal shows in the review queue: the relationship in the reader's language, the
 * birth year and the death. `word(key, vars)` is the page's tf().
 */
export function proposalTitle(p: FamilyProposal, lang: string, word: (key: string, vars?: Record<string, string | number>) => string): string {
	const rel = p.label?.[lang] ?? p.label?.en ?? p.relationship ?? '';
	const parts = [rel ? rel.charAt(0).toUpperCase() + rel.slice(1) : word('i2_kind_family_proposal')];
	if (p.born_year != null) parts.push(word('gd_born', { y: p.born_year }));
	if (p.death_age != null) parts.push(word('gd_died_at', { n: p.death_age }));
	else if (p.deceased) parts.push(word('gd_died'));
	return parts.join(' · ');
}

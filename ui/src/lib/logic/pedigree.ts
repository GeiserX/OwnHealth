// The pedigree drawing (v0.3 family history): our own layered layout, no graph library.
// Pure: the page passes the words in, so node --test can check the geometry and every text line.
//
// Each generation is a row. Inside a row, relatives sit by the `slot` their relationship row in
// data/family/relationships.yaml gives, pushed right when two would overlap. A couple is a line between
// the two symbols; their children hang from a sibship line under it (NSGC 2022 standard symbols:
// square male, circle female, diamond unknown, a slash for deceased, a fill when a condition is
// recorded, an arrow at the proband).
import type { Pedigree, PedigreeNode, Sex } from '#lib/api/family';

export const SLOT_PX = 10;
export const GAP = 120;
export const ROW = 150;
export const LEFT = 70;
export const TOP = 46;
export const HALF = 18;
/** Longest text line under a symbol, in characters. */
export const MAX_CHARS = 20;

export interface Words {
	/** The proband's line when names are hidden ("this profile"). */
	proband: string;
	/** A relative linked to a profile the viewer cannot read. */
	masked: string;
	/** "† {age}" style death mark. */
	died: (age: number | null) => string;
	/** "{n} more" when a relative has more conditions than fit. */
	more: (n: number) => string;
}

export interface Box {
	/** The relative's id; null for the proband. */
	id: string | null;
	x: number;
	y: number;
	sex: Sex | null;
	deceased: boolean;
	affected: boolean;
	proband: boolean;
	linked: boolean;
	masked: boolean;
	lines: string[];
	/** The full description for the tooltip and screen readers (never truncated). */
	title: string;
}

export interface Seg {
	x1: number;
	y1: number;
	x2: number;
	y2: number;
}

export interface Layout {
	width: number;
	height: number;
	gens: { y: number; label: string }[];
	nodes: Box[];
	segs: Seg[];
}

const ROMAN = ['I', 'II', 'III', 'IV', 'V', 'VI', 'VII'];

function clip(s: string): string {
	return s.length > MAX_CHARS ? s.slice(0, MAX_CHARS - 1).trimEnd() + '…' : s;
}

function conditionLine(c: PedigreeNode['conditions'][number], lang: 'es' | 'en'): string {
	const name = c.text ?? (lang === 'es' ? c.short_es : c.short_en) ?? c.kind;
	return c.onset_age == null ? name : `${name} ${c.onset_approx ? '~' : ''}${c.onset_age}`;
}

function conditionFull(c: PedigreeNode['conditions'][number], lang: 'es' | 'en'): string {
	const name = c.text ?? (lang === 'es' ? c.es : c.en) ?? c.kind;
	return c.onset_age == null ? name : `${name} (${c.onset_approx ? '~' : ''}${c.onset_age})`;
}

interface Item {
	node: PedigreeNode | null;
	gen: number;
	slot: number;
	union: string | null;
	child_of: string | null;
	seq: number;
}

export function layout(ped: Pedigree, lang: 'es' | 'en', hideNames: boolean, words: Words): Layout {
	const items: Item[] = [
		{ node: null, gen: 0, slot: ped.profile.slot, union: ped.profile.union, child_of: ped.profile.child_of, seq: -1 },
		...ped.nodes.map((n, i) => ({ node: n, gen: n.generation, slot: n.slot, union: n.union, child_of: n.child_of, seq: i }))
	];
	const gens = [...new Set(items.map((i) => i.gen))].sort((a, b) => a - b);
	const yOf = (g: number) => TOP + gens.indexOf(g) * ROW;
	// Each row keeps the relative nearest the proband's slot where its slot puts it and pushes the others
	// outwards, so the proband's own branch stays straight above and below them.
	const xs = new Map<Item, number>();
	for (const g of gens) {
		const row = items.filter((i) => i.gen === g).sort((a, b) => a.slot - b.slot || a.seq - b.seq);
		const dist = (i: Item) => (i.node ? Math.abs(i.slot - ped.profile.slot) : -1);
		const anchor = row.reduce((best, i, k) => (dist(i) < dist(row[best]) ? k : best), 0);
		for (let k = anchor; k < row.length; k++) {
			const prev = k === anchor ? -Infinity : (xs.get(row[k - 1]) ?? 0) + GAP;
			xs.set(row[k], Math.max(row[k].slot * SLOT_PX, prev));
		}
		for (let k = anchor - 1; k >= 0; k--) xs.set(row[k], Math.min(row[k].slot * SLOT_PX, (xs.get(row[k + 1]) ?? 0) - GAP));
	}
	const shift = LEFT - Math.min(...xs.values());
	for (const [k, v] of xs) xs.set(k, v + shift);

	const nodes: Box[] = items.map((it) => {
		const x = xs.get(it) ?? LEFT;
		const y = yOf(it.gen);
		if (!it.node) {
			const p = ped.profile;
			const head = hideNames ? words.proband : p.name;
			const lines = [clip(head)];
			if (p.age != null) lines.push(String(p.age));
			return { id: null, x, y, sex: p.sex_at_birth, deceased: p.deceased, affected: false, proband: true, linked: true, masked: false, lines, title: lines.join(', ') };
		}
		const n = it.node;
		const rel = n.label[lang];
		const named = !hideNames && n.name ? n.name : null;
		const lines: string[] = [clip(named ?? rel)];
		const full: string[] = named ? [rel, named] : [rel];
		if (n.masked) {
			lines.push(clip(words.masked));
			full.push(words.masked);
		} else {
			const life = n.deceased ? words.died(n.death_age) : n.age != null ? String(n.age) : '';
			if (life) {
				lines.push(life);
				full.push(life);
			}
			const conds = n.conditions.map((c) => conditionLine(c, lang));
			const shown = conds.length > 2 ? [...conds.slice(0, 1), words.more(conds.length - 1)] : conds;
			lines.push(...shown.map(clip));
			full.push(...n.conditions.map((c) => conditionFull(c, lang)));
		}
		return {
			id: n.id,
			x,
			y,
			sex: n.sex_at_birth,
			deceased: n.deceased,
			affected: !n.masked && n.conditions.length > 0,
			proband: false,
			linked: n.linked,
			masked: n.masked,
			lines,
			title: full.join(', ')
		};
	});

	const segs: Seg[] = [];
	const unions = [...new Set(items.flatMap((i) => [i.union, i.child_of]).filter((u): u is string => !!u))];
	for (const u of unions) {
		const members = items.filter((i) => i.union === u);
		const children = items.filter((i) => i.child_of === u);
		const mx = members.map((m) => xs.get(m) ?? 0);
		let drop: { x: number; y: number } | null = null;
		if (members.length >= 2) {
			const y = yOf(members[0].gen);
			const a = Math.min(...mx);
			const b = Math.max(...mx);
			segs.push({ x1: a + HALF, y1: y, x2: b - HALF, y2: y });
			drop = { x: (a + b) / 2, y };
		} else if (members.length === 1) {
			// one parent drawn: the line starts under their text, so it never crosses it
			const box = nodes[items.indexOf(members[0])];
			drop = { x: mx[0], y: box.y + 36 + (box.lines.length - 1) * 14 + 6 };
		}
		if (!children.length || (!drop && children.length < 2)) continue;
		const cy = yOf(children[0].gen);
		const barY = cy - HALF - 22;
		const cx = children.map((c) => xs.get(c) ?? 0);
		const lo = Math.min(...cx, drop?.x ?? Infinity);
		const hi = Math.max(...cx, drop?.x ?? -Infinity);
		if (drop) segs.push({ x1: drop.x, y1: drop.y, x2: drop.x, y2: barY });
		if (hi > lo) segs.push({ x1: lo, y1: barY, x2: hi, y2: barY });
		for (const x of cx) segs.push({ x1: x, y1: barY, x2: x, y2: cy - HALF });
	}

	const maxX = Math.max(...nodes.map((n) => n.x));
	const maxLines = Math.max(...nodes.filter((n) => n.y === yOf(gens[gens.length - 1])).map((n) => n.lines.length));
	return {
		width: maxX + 90,
		height: yOf(gens[gens.length - 1]) + HALF + 14 + maxLines * 14,
		gens: gens.map((g, i) => ({ y: yOf(g), label: ROMAN[i] ?? String(i + 1) })),
		// reading order, so the Tab key walks the tree generation by generation, left to right
		nodes: [...nodes].sort((a, b) => a.y - b.y || a.x - b.x),
		segs
	};
}

/** Household profiles as relatives of the founder: the rows "fill from household" adds. */
export function fromHousehold(
	profiles: { slug: string; relationship: string | null; sex_at_birth: Sex | null }[],
	onScreen: string,
	linked: Set<string>
): { relationship: string; sex_at_birth: Sex | null; linked_profile: string }[] {
	const code = (rel: string, sex: Sex | null): string | null => {
		if (rel === 'partner') return 'SPS';
		if (rel === 'child') return sex === 'female' ? 'DAU' : sex === 'male' ? 'SON' : 'CHILD';
		if (rel === 'parent') return sex === 'female' ? 'MTH' : sex === 'male' ? 'FTH' : 'PRN';
		return null;
	};
	return profiles
		.filter((p) => p.slug !== onScreen && !linked.has(p.slug))
		.map((p) => ({ relationship: code(p.relationship ?? '', p.sex_at_birth), sex_at_birth: p.sex_at_birth, linked_profile: p.slug }))
		.filter((r): r is { relationship: string; sex_at_birth: Sex | null; linked_profile: string } => r.relationship !== null);
}

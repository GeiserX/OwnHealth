// Round 3 regressions: markup that the browser reads differently from how it looks, and the
// small logic helpers behind the dashboard, audit and review wording.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { parse } from 'svelte/compiler';
import { auditActionKey, auditDetail, auditResult } from '../src/lib/logic/people.ts';
import { afterColon, sameNumber } from '../src/lib/logic/format.ts';

function svelteFiles(dir: string): string[] {
	const out: string[] = [];
	for (const name of readdirSync(dir)) {
		const p = join(dir, name);
		if (statSync(p).isDirectory()) out.push(...svelteFiles(p));
		else if (p.endsWith('.svelte')) out.push(p);
	}
	return out;
}

// The pattern attribute as the browser gets it. Svelte reads `{2}` inside a quoted attribute as an
// expression, so pattern="[A-Za-z]{2}" reaches the DOM as "[A-Za-z]2".
function patterns(file: string): { file: string; value: string }[] {
	const ast = parse(readFileSync(file, 'utf8'), { modern: true });
	const found: { file: string; value: string }[] = [];
	const walk = (n: unknown) => {
		if (!n || typeof n !== 'object') return;
		const node = n as Record<string, unknown>;
		if (node.type === 'Attribute' && node.name === 'pattern') {
			const v = node.value;
			const parts = Array.isArray(v) ? v : [v];
			let s = '';
			for (const p of parts as Record<string, unknown>[]) {
				if (p.type === 'Text') s += p.data as string;
				else {
					const e = p.expression as Record<string, unknown>;
					// Only literals can be checked here; anything else is left out of the value.
					if (e.type === 'Literal') s += String(e.value);
				}
			}
			found.push({ file, value: s });
		}
		for (const k of Object.keys(node)) if (k !== 'parent') walk(node[k]);
	};
	walk(ast.fragment);
	return found;
}

test('country inputs: the pattern reaching the browser accepts ES and refuses E2', () => {
	const root = new URL('../src', import.meta.url).pathname;
	const all = svelteFiles(root).flatMap(patterns);
	const country = all.filter((p) => /family\/\+page|LoginScreen/.test(p.file));
	assert.equal(country.length, 2, 'family and first-run setup both have a country pattern');
	for (const p of country) {
		assert.equal(p.value, '[A-Za-z]{2}', p.file);
		const re = new RegExp(`^(?:${p.value})$`, 'v');
		assert.ok(re.test('ES'), `${p.file}: ES must be valid`);
		assert.ok(!re.test('E2'), `${p.file}: E2 must be refused`);
	}
	// No pattern anywhere may be cut by a stray expression.
	for (const p of all) assert.doesNotThrow(() => new RegExp(p.value, 'v'), p.file);
});

const words: Record<string, string> = {
	i2_sens_ui: 'Mostrar en la app',
	i2_sens_ag: 'Compartir con agentes',
	i2_on: 'Sí',
	i2_off: 'No',
	i2_lock_on: 'Bloqueado',
	i2_ad_nothing: 'nada',
	i2_rt_state: 'Resumen de salud',
	me_k_immunization: 'Vacuna'
};
const w = { t: (k: string) => words[k] ?? k, scope: (s: string) => (s === 'read:genome' ? 'Genoma' : s), n: (k: string, n: number) => `${n} ${k}` };

test('audit: consent and lock changes read as sentences, never as a repr', () => {
	assert.equal(auditDetail({ action: 'consent_change', args_summary: 'ui=True agents=None' }, w), 'Mostrar en la app: sí');
	assert.equal(auditDetail({ action: 'consent_change', args_summary: 'ui=None;agents=False' }, w), 'Compartir con agentes: no');
	assert.equal(auditDetail({ action: 'lock_change', args_summary: 'read:genome' }, w), 'Bloqueado: Genoma');
	assert.equal(auditDetail({ action: 'lock_change', args_summary: '' }, w), 'Bloqueado: nada');
	assert.equal(
		auditDetail({ action: 'token_create', args_summary: 'scopes=read:labs,read:summary;profiles=2;days=30' }, w),
		'2 i2_ad_scopes · 2 i2_ad_profiles · 30 i2_ad_days'
	);
});

test('audit: resource types use words, a hash never shows, an unknown type stays as it is', () => {
	assert.equal(auditDetail({ action: 'read', resource_type: 'state', resource_ids: ['a'] }, w), 'Resumen de salud');
	assert.equal(auditDetail({ action: 'write', resource_type: 'immunization', resource_ids: ['a', 'b'] }, w), 'Vacuna ×2');
	assert.equal(auditDetail({ action: 'read', args_summary: 'h:abc', resource_type: 'novel' }, w), 'novel');
	assert.equal(auditDetail({ action: 'read' }, w), '—');
});

test('audit: page actions get a dict key, agent tools keep their name; lookup codes are results', () => {
	assert.equal(auditActionKey({ via: 'ui', tool: 'inbox', action: 'source.create' }), 'i2_act_source_create');
	assert.equal(auditActionKey({ via: 'mcp', tool: 'get_trend', action: 'tools/call:get_trend' }), null);
	assert.equal(auditResult('rsid_unknown').key, 'i2_res_rsid_unknown');
	assert.equal(auditResult('withheld_minor').tone, 'p-info');
});

test('every audit action, resource and result key exists in both languages', async () => {
	const { dict } = await import('../src/lib/i18n/dict.ts');
	const en = Object.keys(dict.en).filter((k) => /^i2_(act|rt|res)_/.test(k));
	assert.ok(en.length > 40);
	for (const k of en) assert.ok(k in dict.es, k);
	for (const k of Object.keys(dict.es).filter((k) => /^i2_(act|rt|res)_/.test(k))) assert.ok(k in dict.en, k);
});

test('MCP & access: each tool lists the scopes the server checks', () => {
	const toolsDir = new URL('../../server/crates/ownhealth-mcp/src/tools/', import.meta.url).pathname;
	const server = new Map<string, { any: string[]; all: string[] }>();
	const list = (s: string) => [...s.matchAll(/"([a-z_:]+)"/g)].map((m) => m[1]);
	for (const f of readdirSync(toolsDir).filter((f) => f.endsWith('.rs'))) {
		const src = readFileSync(join(toolsDir, f), 'utf8');
		for (const m of src.matchAll(/name: "([a-z_]+)",[\s\S]*?any_scope: &(\[[^\]]*\]|[A-Z_]+),\s*all_scopes: &\[([^\]]*)\]/g))
			server.set(m[1], { any: m[2].startsWith('[') ? list(m[2]) : [], all: list(m[3]) });
	}
	const page = readFileSync(new URL('../src/routes/mcp-access/+page.svelte', import.meta.url), 'utf8');
	const rows = [...page.matchAll(/\{ name: '([a-z_]+)', scopes: \[([^\]]*)\](, any: true)?/g)];
	assert.equal(rows.length, 10);
	for (const [, name, scopes, any] of rows) {
		if (name === 'get_source') continue; // any read scope, said in words
		const spec = server.get(name);
		assert.ok(spec, name);
		const ui = [...scopes.matchAll(/'([a-z_:]+)'/g)].map((m) => m[1]).sort();
		const alternatives = spec.all.length === 0 && spec.any.length > 1;
		assert.equal(!!any, alternatives, `${name}: any-of or all-of`);
		assert.deepEqual(ui, [...new Set([...spec.all, ...spec.any])].sort(), name);
	}
});

test('review: a printed and a stored value that differ only in notation count as the same number', () => {
	assert.ok(sameNumber('1,05', '1.05'));
	assert.ok(sameNumber('245', 245));
	assert.ok(!sameNumber('1,05', 93.4));
	assert.ok(!sameNumber('<0,5', 0.5));
	assert.ok(!sameNumber('1,05', null));
});

test('Spanish: the sentence after a colon starts in lowercase, acronyms keep their capitals', () => {
	assert.equal(afterColon('No se pudo conectar', 'es'), 'no se pudo conectar');
	assert.equal(afterColon('TLS rechazado', 'es'), 'TLS rechazado');
	assert.equal(afterColon('Could not connect', 'en'), 'Could not connect');
	assert.equal(afterColon('', 'es'), '');
});

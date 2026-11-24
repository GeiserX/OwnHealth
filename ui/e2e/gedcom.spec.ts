// A family tree file on the Family screen (slice 24): the person picks themselves in the tree by the
// names their browser reads from the file, the relatives wait in Import & review with no name, Accept
// adds one to the family history and Dismiss adds none. The upload never carries the file's own name.
// Synthetic trees of the Rivera family (fixtures/gedcom), in each language.
import { expect, test, type Page } from '@playwright/test';
import { Buffer } from 'node:buffer';
import { fileURLToPath } from 'node:url';

const fixture = (name: string) => fileURLToPath(new URL(`../../fixtures/gedcom/${name}`, import.meta.url));

const CASES = [
	{
		lang: 'en',
		file: 'rivera-551.ged',
		search: 'Alex',
		pick: 'Alex Rivera Vexlorn',
		done: '14 relatives wait in Import & review as proposals.',
		accept: 'Father · born in 1955 · died at 64',
		dismiss: 'Mother · born in 1958',
		added: { relationship: 'FTH', born_year: 1955, death_age: 64 }
	},
	{
		lang: 'es',
		file: 'rivera-70.ged',
		search: '',
		pick: 'Persona 4',
		done: '2 familiares esperan en Importar y revisar como propuestas.',
		accept: 'Padre · nacimiento en 2000 · murió a los 22 años',
		dismiss: 'Madre',
		added: { relationship: 'FTH', born_year: 2000, death_age: 22 }
	}
] as const;

/** Alex Rivera on screen: the profile buttons of the import screen set it for every page. */
async function asAlex(page: Page, lang: string) {
	await page.goto(`/import-review?lang=${lang}`);
	await page.getByRole('button', { name: 'Alex Rivera', exact: true }).click();
}

for (const c of CASES) {
	test(`a tree proposes relatives and Accept adds one with no name, in ${c.lang}`, async ({ page }) => {
		const errors: string[] = [];
		page.on('pageerror', (e) => errors.push(e.message));
		// the browser does not hand a multipart file body to Playwright, so the page notes the name each
		// gedcom upload gives its file just before it goes out
		await page.addInitScript(() => {
			const send = window.fetch;
			const w = window as unknown as { gdSent: string[] };
			w.gdSent = [];
			window.fetch = (input, init) => {
				const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
				if (url.includes('/family/gedcom') && init?.body instanceof FormData) {
					const f = init.body.get('file');
					w.gdSent.push(f instanceof File ? f.name : '');
				}
				return send(input, init);
			};
		});
		type Row = { relationship: string; born_year: number | null; death_age: number | null; display_name: string | null };
		const family = async () => ((await (await page.request.get('/api/profiles/alex/family')).json()).items as Row[]);
		await asAlex(page, c.lang);
		const before = await family();
		await page.goto(`/family?lang=${c.lang}`);
		const card = page.getByRole('region', { name: c.lang === 'en' ? 'Import a family tree' : 'Importar un árbol genealógico' });
		await card.locator('input[type=file]').setInputFiles(fixture(c.file));
		const picker = card.getByRole('group');
		await expect(picker).toBeVisible({ timeout: 30_000 });
		if (c.search) await picker.getByRole('searchbox').fill(c.search);
		await picker.getByRole('radio', { name: new RegExp(`^${c.pick}\\b`) }).check();
		await card.getByRole('button', { name: c.lang === 'en' ? 'Propose relatives' : 'Proponer familiares' }).click();
		await expect(card.getByRole('status')).toContainText(c.done, { timeout: 30_000 });
		// the file went twice (preview, import), each time without its own name
		const sent = await page.evaluate(() => (window as unknown as { gdSent: string[] }).gdSent);
		expect(sent).toEqual(['tree.ged', 'tree.ged']);

		await card.getByRole('link').click();
		await expect(page).toHaveURL(/\/import-review/);
		const row = (title: string) => page.locator('tr', { has: page.getByText(title, { exact: true }) });
		await expect(row(c.accept)).toBeVisible({ timeout: 30_000 });
		await row(c.accept).getByRole('button', { name: c.lang === 'en' ? 'Accept' : 'Aceptar' }).click();
		await expect(row(c.accept)).toHaveCount(0);
		await row(c.dismiss).first().getByRole('button', { name: c.lang === 'en' ? 'Dismiss' : 'Descartar' }).click();

		// one relative more: the accepted father, with no name; the dismissed mother is not there
		const after = await family();
		expect(after.length).toBe(before.length + 1);
		const added = after.filter((m) => m.relationship === c.added.relationship && m.born_year === c.added.born_year);
		expect(added).toHaveLength(1);
		expect(added[0]).toMatchObject({ death_age: c.added.death_age, display_name: null });
		expect(after.filter((m) => m.relationship === 'MTH').length).toBe(before.filter((m) => m.relationship === 'MTH').length);
		expect(errors).toEqual([]);
	});
}

test('a file that is not a family tree says so in the reader’s language', async ({ page }) => {
	await asAlex(page, 'es');
	await page.goto('/family?lang=es');
	const card = page.getByRole('region', { name: 'Importar un árbol genealógico' });
	// the folder's README: text, but not a family tree
	await card.locator('input[type=file]').setInputFiles(fixture('README.md'));
	await expect(card.getByRole('alert')).toHaveText('Este archivo no es un árbol genealógico GEDCOM.');
	await expect(card.getByRole('group')).toHaveCount(0);
});

/** Count the gedcom uploads the page sends. */
async function countUploads(page: Page) {
	await page.addInitScript(() => {
		const send = window.fetch;
		const w = window as unknown as { gdSent: number };
		w.gdSent = 0;
		window.fetch = (input, init) => {
			const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
			if (url.includes('/family/gedcom')) w.gdSent += 1;
			return send(input, init);
		};
	});
}

test('a file over 10 MiB is refused before the browser reads or sends it', async ({ page }) => {
	await countUploads(page);
	await asAlex(page, 'en');
	await page.goto('/family?lang=en');
	const card = page.getByRole('region', { name: 'Import a family tree' });
	await card.locator('input[type=file]').setInputFiles({ name: 'big.ged', mimeType: 'application/octet-stream', buffer: Buffer.alloc(11 * 1024 * 1024, 0x20) });
	await expect(card.getByRole('alert')).toHaveText('A family tree file is at most 10 MiB.');
	expect(await page.evaluate(() => (window as unknown as { gdSent: number }).gdSent)).toBe(0);
});

test('a long name wraps on a phone and the birth year stays in view', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await asAlex(page, 'es');
	await page.goto('/family?lang=es');
	const card = page.getByRole('region', { name: 'Importar un árbol genealógico' });
	const name = 'Rosalind Ilsabet Quorvantis María de los Ángeles /Núñez Fennickwold de la Vexlorn/';
	const tree = `0 HEAD\n1 GEDC\n2 VERS 5.5.1\n1 CHAR UTF-8\n0 @I1@ INDI\n1 NAME ${name}\n1 SEX F\n1 BIRT\n2 DATE 1961\n0 TRLR\n`;
	await card.locator('input[type=file]').setInputFiles({ name: 'long.ged', mimeType: 'text/plain', buffer: Buffer.from(tree, 'utf8') });
	const picker = card.getByRole('group');
	await expect(picker).toBeVisible({ timeout: 30_000 });
	const list = picker.locator('ul');
	const line = list.locator('li label').first().locator('span.dim');
	await expect(line).toContainText('nacimiento en 1961');
	const [l, d] = [await list.boundingBox(), await line.boundingBox()];
	expect(l && d && d.x + d.width <= l.x + l.width + 1).toBe(true);
	expect(await list.evaluate((e) => e.scrollWidth <= e.clientWidth)).toBe(true);
});

import { expect, test } from '@playwright/test';
import { execFileSync } from 'node:child_process';

test('a household owner can opt in to the session report in English and Spanish', async ({ page }) => {
	const db = process.env.OWNHEALTH_E2E_DB ?? '';
	expect(db).toMatch(/^oh_e2e_[a-z0-9]+$/);

	await page.goto('/mcp-access?lang=en');
	const report = page.getByRole('region', { name: 'Agent session report' });
	let consent = report.getByRole('checkbox', { name: 'Collect agent session statistics' });
	await expect(consent).not.toBeChecked();
	await expect(report.getByText(/30 minutes/)).toBeVisible();
	await expect.soft(report.getByText(/Denied also counts calls refused because a daily limit was used up/)).toBeVisible();
	// The keyboard keeps its place: focus stays on the box through the save.
	await consent.focus();
	await page.keyboard.press('Space');
	await expect(report.getByText('No recorded sessions yet.')).toBeVisible();
	await expect.soft(consent).toBeFocused();

	// An older session that fell short, then a newer one with no shortfall: both show their rows.
	execFileSync('docker', ['exec', 'ownhealth-testpg', 'psql', '-U', 'postgres', '-d', db, '-v', 'ON_ERROR_STOP=1', '-qc',
		"insert into audit_event (ts, actor_kind, actor_id, token_id, via, action, tool, result_kind, not_assessed_n, arg_bytes) " +
		"select now() - interval '2 hours', 'token', user_id, gen_random_uuid(), 'mcp', 'tools/call:get_genome_findings', 'get_genome_findings', 'budget_cut', 2, 64 " +
		"from profile where slug = 'alex'; " +
		"insert into audit_event (actor_kind, actor_id, token_id, via, action, tool, result_kind, not_assessed_n, arg_bytes) " +
		"select 'token', user_id, gen_random_uuid(), 'mcp', 'tools/call:list_profiles', 'list_profiles', 'ok', 0, 2 " +
		"from profile where slug = 'alex'"], { encoding: 'utf8' });
	await page.reload();
	await expect(report.getByRole('row').filter({ hasText: 'list_profiles' })).toBeVisible();
	await expect.soft(report.getByRole('row').filter({ hasText: 'get_genome_findings' })).toContainText('64');
	await expect(report.getByRole('columnheader', { name: 'Budget cuts' }).first()).toBeVisible();

	await page.goto('/mcp-access?lang=es');
	const informe = page.getByRole('region', { name: 'Informe de sesiones de agentes' });
	consent = informe.getByRole('checkbox', { name: 'Recopilar estadísticas de sesiones de agentes' });
	await expect(consent).toBeChecked();
	await expect(informe.getByText(/30 minutos/)).toBeVisible();
	await expect(informe.getByRole('row').filter({ hasText: 'list_profiles' })).toBeVisible();
	// A desktop screen shows every column without a sideways scroll.
	const hiddenColumns = await informe.locator('.tscroll').evaluateAll((els) => els.filter((e) => e.scrollWidth > e.clientWidth).length);
	expect.soft(hiddenColumns).toBe(0);
	await consent.focus();
	await page.keyboard.press('Space');
	await expect(informe.getByText('La recopilación está desactivada.')).toBeVisible();
	await expect.soft(consent).toBeFocused();
	await expect(informe.getByText('get_genome_findings')).toHaveCount(0);
});

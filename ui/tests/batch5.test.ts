// Unit tests for the people and access screens: the step-up password loop, the grants matrix heads,
// the invite link's origin, the audit log's rows and words, the token form's checks and the plain
// words for the server's birth date and country refusals.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
	askUntilRight,
	auditActor,
	auditOut,
	auditResult,
	auditRows,
	auditWhat,
	byOrder,
	byScopeOrder,
	foreignOrigin,
	lastSignIn,
	liveConsents,
	matrixHeads,
	matrixRowLabel,
	nameTaken,
	todayIso,
	tokenProblem
} from '../src/lib/logic/people.ts';
import { errKey } from '../src/lib/logic/errors.ts';

test('step-up: a wrong password asks again with the error, a right one finishes, Cancel stops', async () => {
	const asked: (string | null)[] = [];
	const answers = ['wrong', 'right'];
	const tried: string[] = [];
	const ok = await askUntilRight(
		async (err) => {
			asked.push(err);
			return answers.shift() ?? null;
		},
		async (pw) => {
			tried.push(pw);
			if (pw !== 'right') throw Object.assign(new Error('wrong password'), { status: 401 });
		},
		(e) => ((e as { status?: number }).status === 401 ? 'Wrong password.' : null)
	);
	assert.equal(ok, true);
	assert.deepEqual(tried, ['wrong', 'right']);
	assert.deepEqual(asked, [null, 'Wrong password.']);

	const cancelled = await askUntilRight(async () => null, async () => {}, () => null);
	assert.equal(cancelled, false);

	// Anything other than a wrong password closes the dialog and reaches the caller.
	await assert.rejects(
		askUntilRight(async () => 'x', async () => { throw new Error('server down'); }, () => null),
		/server down/
	);
});

test('grants matrix: first names, the full name when two first names collide', () => {
	const heads = matrixHeads([
		{ slug: 'alex', display_name: 'Alex Rivera' },
		{ slug: 'lucia', display_name: 'Lucía Rivera' },
		{ slug: 'lucia-2', display_name: 'Lucía Ortega' }
	]);
	assert.deepEqual(heads.map((h) => [h.label, h.dup]), [['Alex', false], ['Lucía Rivera', true], ['Lucía Ortega', true]]);
});

test('grants matrix rows: the full name when the first name is another person, (you) on the signed-in row', () => {
	const members = [
		{ user_id: 'u1', display_name: 'Alex Rivera', is_me: true },
		{ user_id: 'u2', display_name: 'Carmen Ortega Prueba', is_me: false },
		{ user_id: 'u3', display_name: 'Sam Rivera', is_me: false }
	];
	const profiles = [
		{ slug: 'alex', display_name: 'Alex Rivera' },
		{ slug: 'carmen', display_name: 'Carmen Ortega' }
	];
	assert.deepEqual(
		members.map((m) => matrixRowLabel(m, members, profiles, 'you')),
		['Alex (you)', 'Carmen Ortega Prueba', 'Sam']
	);
});

test('add relative: an exact name match is noticed, spacing and case ignored', () => {
	const ps = [{ display_name: 'Lucía Rivera' }];
	assert.equal(nameTaken('  lucía   rivera ', ps), true);
	assert.equal(nameTaken('Lucía', ps), false);
	assert.equal(nameTaken('', ps), false);
});

test('invite link: another origin is named, the same one is not', () => {
	assert.equal(foreignOrigin('http://127.0.0.1:8080/enroll?code=x', 'http://127.0.0.1:8111'), 'http://127.0.0.1:8080');
	assert.equal(foreignOrigin('http://127.0.0.1:8111/enroll?code=x', 'http://127.0.0.1:8111'), null);
	assert.equal(foreignOrigin('not a url', 'http://x'), null);
});

test('today in local time, consents still live, the last way in', () => {
	assert.equal(todayIso(new Date(2026, 0, 5, 23, 30)), '2026-01-05');
	const now = new Date('2026-11-10T10:00:00Z');
	const list = [
		{ id: 'a', revoked_at: null, expires_at: '2027-01-01T00:00:00Z' },
		{ id: 'b', revoked_at: '2026-11-01T00:00:00Z', expires_at: '2027-01-01T00:00:00Z' },
		{ id: 'c', revoked_at: null, expires_at: '2026-11-09T00:00:00Z' }
	];
	assert.deepEqual(liveConsents(list, now).map((c) => c.id), ['a']);
	assert.equal(liveConsents(null).length, 0);
	assert.equal(lastSignIn(1, false, false), true);
	assert.equal(lastSignIn(1, true, false), false);
	assert.equal(lastSignIn(2, false, false), false);
});

test('audit log: agents only by default, plain result words, the app name instead of its URL', () => {
	const rows = [
		{ via: 'ui', actor: 'Alex Rivera', action: 'dashboard', tool: null, result_kind: null, rows_out: 0, bytes_out: 0 },
		{ via: 'mcp', actor: 'https://agent.rivera.example/client.json (token)', action: 'mcp', tool: 'get_source', result_kind: 'budget_cut', rows_out: 0, bytes_out: 2280 },
		{ via: 'api', actor: 'Alex Rivera', action: 'token_create', tool: null, result_kind: null, rows_out: null, bytes_out: null }
	];
	assert.deepEqual(auditRows(rows, 'agents').map((r) => r.tool), ['get_source']);
	assert.equal(auditRows(rows, 'all').length, 3);
	assert.deepEqual(auditResult('budget_cut'), { key: 'i2_res_budget_cut', tone: 'p-watch' });
	assert.equal(auditResult('denied').tone, 'p-out');
	assert.equal(auditResult('error').tone, 'p-out');
	assert.equal(auditResult('queued').tone, 'p-info');
	assert.equal(auditResult(null).tone, 'p-ok');
	const apps = [{ client_id: 'https://agent.rivera.example/client.json', name: 'Agente de ejemplo' }];
	assert.equal(auditActor(rows[1].actor, apps).label, 'Agente de ejemplo');
	assert.equal(auditActor('Claude Code · MacMini (token)', apps).label, 'Claude Code · MacMini');
	assert.equal(auditWhat(rows[1]), 'get_source');
	assert.equal(auditWhat(rows[2]), 'token_create');
	assert.deepEqual(auditOut(rows[1]), { kb: 2.28 });
	assert.deepEqual(auditOut({ rows_out: 4, bytes_out: 900 }), { rows: 4 });
});

test('scopes and profiles: the token form order, the household order', () => {
	const s = byScopeOrder(['write:propose', 'read:genome_raw', 'read:labs', 'read:summary'], (x) => x);
	assert.deepEqual(s, ['read:summary', 'read:labs', 'read:genome_raw', 'write:propose']);
	const p = byOrder([{ id: 'c' }, { id: 'a' }, { id: 'z' }, { id: 'b' }], (x) => x.id, ['a', 'b', 'c']);
	assert.deepEqual(p.map((x) => x.id), ['a', 'b', 'c', 'z']);
});

test('token form: a name, a profile and a scope, and 1 to 365 days', () => {
	assert.equal(tokenProblem(' ', ['alex'], ['read:labs'], 90), 'i2_tok_need_label');
	assert.equal(tokenProblem('x', [], ['read:labs'], 90), 'i2_tok_need');
	assert.equal(tokenProblem('x', ['alex'], ['read:labs'], 0), 'i2_tok_need_days');
	assert.equal(tokenProblem('x', ['alex'], ['read:labs'], 400), 'i2_tok_need_days');
	assert.equal(tokenProblem('x', ['alex'], ['read:labs'], null), 'i2_tok_need_days');
	assert.equal(tokenProblem('x', ['alex'], ['read:labs'], 90), null);
});

test('the server refusing a birth date or a country reads in plain words', () => {
	const has = (k: string) => k !== 'err_bad_input';
	assert.equal(errKey('bad_input', 'The birth date is in the future.', has), 'err_birth_future');
	assert.equal(errKey('bad_input', 'The birth date is before 1900.', has), 'err_birth_old');
	assert.equal(errKey('bad_input', 'The country is two letters, for example ES or GB.', has), 'err_country');
});

test('a locked PDF reads in plain words wherever its job error shows', () => {
	const has = () => true;
	assert.equal(errKey(null, 'pdf_password: the PDF is locked with a password. Save a copy without the password and upload that.', has), 'ir_pdf_password');
});

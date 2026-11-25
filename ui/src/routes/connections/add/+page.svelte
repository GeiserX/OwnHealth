<script lang="ts">
	// The add-connection wizard (v1.0, slice 19): pick a US vendor of data/connectors/connectors.yaml,
	// find its server (the US provider directory for vendors with many servers, the vendor's own list
	// otherwise), register an app at the vendor with the redirect URI and scopes shown here, save the
	// app's client id (and secret, sealed by the server and never shown again), then connect through the
	// one launch route. Every vendor says it is not verified live. Logic in ./wizard.ts.
	import '#lib/components/pages2/pages2.css';
	import { ApiError } from '#lib/api/core';
	import { getConnectorProfiles, launchConnector, listConnectorClients, openBrands, saveConnectorClient, searchBrands } from '#lib/api/connections';
	import type { BrandDirectory, BrandHit, ConnectorProfiles, SavedClient } from '#lib/api/types';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import PhaseChip from '#lib/components/PhaseChip.svelte';
	import NotReady from '#lib/components/pages1/NotReady.svelte';
	import { Loader, errRaw, errText, tf } from '#lib/components/pages1/util.svelte';
	import { dict } from '#lib/i18n/dict';
	import { t } from '#lib/i18n/index.svelte';
	import { copyText, manageable } from '#lib/logic/inbox';
	import { app } from '#lib/state/app.svelte';
	import { clientFor, defaultLabel, hitsOf, needsSecret, saveIssuer, scopeLine, serverPick, tokenFacts, typedServer } from './wizard';

	const profiles = new Loader<ConnectorProfiles>();
	const clients = new Loader<SavedClient[]>();
	const loadClients = () => clients.run(async () => (await listConnectorClients()).clients);
	$effect(() => {
		profiles.run(getConnectorProfiles);
		loadClients();
	});

	const has = (k: string) => k in dict.en;
	// Blue Button refuses any address but its two v3 servers: its bad_issuer says so, not "not https"
	const codeKey = (code: string) => (code === 'bad_issuer' && vendor?.kind === 'bluebutton' ? 'cw_err_bluebutton_server' : 'con_err_' + code);
	const failure = (e: unknown) => ({
		text: e instanceof ApiError && has(codeKey(e.code)) ? tf(codeKey(e.code), { code: e.code }) : errText(e),
		raw: errRaw(e)
	});

	let vendorKey = $state<string | null>(null);
	const vendor = $derived(profiles.data?.vendors.find((v) => v.key === vendorKey) ?? null);

	// The server: a directory hit, one of the vendor's servers, or an address the person typed.
	let directory = $state<BrandDirectory | null>(null);
	let opened = $state(false);
	let q = $state('');
	let hits = $state<BrandHit[] | null>(null);
	let searching = $state(false);
	let searchError = $state<{ text: string; raw: string } | null>(null);
	let choice = $state(''); // 'hit:<endpoint>' | 'srv:<url>' | 'other'
	let facility = $state<string | null>(null);
	let other = $state('');
	const server = $derived(choice === 'other' ? typedServer(other) : choice ? choice.slice(4) : null);

	// The client: the saved one for this server, or the form that saves one.
	const client = $derived(vendor && server ? clientFor(clients.data ?? [], vendor, server) : null);
	let replacing = $state(false);
	let clientId = $state('');
	let secret = $state('');
	let saving = $state(false);
	let saveError = $state<{ text: string; raw: string } | null>(null);
	let saved = $state<'secret' | 'public' | null>(null);

	// The connection: whose record, its name, the launch.
	const mine = $derived(manageable(app.me?.profiles ?? []));
	let profileId = $state('');
	const whose = $derived(profileId || (mine.find((p) => p.is_self) ?? mine[0])?.id || '');
	let label = $state('');
	let launching = $state(false);
	let launchError = $state<{ text: string; raw: string } | null>(null);

	let copied = $state<{ what: string; ok: boolean } | null>(null);
	let redirectEl: HTMLElement | undefined = $state();
	let scopesEl: HTMLElement | undefined = $state();
	async function copy(text: string, what: string, box: HTMLElement | undefined) {
		copied = { what, ok: await copyText(typeof navigator === 'undefined' ? null : navigator.clipboard, text, box) };
	}

	async function pickVendor(key: string) {
		vendorKey = key;
		const v = profiles.data?.vendors.find((x) => x.key === key);
		hits = null;
		q = '';
		other = '';
		facility = null;
		searchError = null;
		replacing = false;
		clientId = '';
		secret = '';
		saveError = null;
		saved = null;
		launchError = null;
		copied = null;
		choice = v && serverPick(v) === 'list' && v.servers.length ? 'srv:' + v.servers[0] : '';
		// the first open makes the household's directory source, so only this wizard contacts US vendors
		if (v?.directory && !opened) {
			opened = true;
			try {
				directory = (await openBrands()).directory;
			} catch (e) {
				searchError = failure(e);
			}
		}
	}

	async function search(e: SubmitEvent) {
		e.preventDefault();
		if (!vendor || !q.trim()) return;
		searching = true;
		searchError = null;
		try {
			const found = await searchBrands(q.trim(), 50);
			directory = found.directory;
			hits = hitsOf(found.results, vendor);
		} catch (err) {
			searchError = failure(err);
		} finally {
			searching = false;
		}
	}

	function pickHit(h: BrandHit) {
		choice = 'hit:' + h.endpoint;
		facility = h.name;
	}

	async function save(e: SubmitEvent) {
		e.preventDefault();
		if (!vendor || !server) return;
		saving = true;
		saveError = null;
		try {
			const confidential = needsSecret(vendor);
			await saveConnectorClient({
				vendor: vendor.key,
				client_id: clientId.trim(),
				client_secret: confidential ? secret : undefined,
				issuer: saveIssuer(vendor, server)
			});
			saved = confidential ? 'secret' : 'public';
			replacing = false;
			clientId = '';
			await loadClients();
		} catch (err) {
			saveError = failure(err);
		} finally {
			// the secret leaves the page whatever the answer: a retry pastes it again
			secret = '';
			saving = false;
		}
	}

	async function connect(e: SubmitEvent) {
		e.preventDefault();
		if (!vendor || !server || !client || !whose) return;
		launching = true;
		launchError = null;
		try {
			const { authorize_url } = await launchConnector(vendor.kind, {
				profile_id: whose,
				client_id: client.id,
				issuer: server,
				label: label.trim() || defaultLabel(server, facility) || undefined
			});
			// the vendor's sign-in; its callback brings the browser back to Connections
			location.assign(authorize_url);
		} catch (err) {
			launchError = failure(err);
			launching = false;
		}
	}
</script>

{#snippet copyHand(what: string)}
	{#if copied?.what === what && !copied.ok}<div class="note" role="alert" style="color:var(--watch)">{t('cw_copy_hand')}</div>{/if}
{/snippet}

<PageHeader title={t('cw_title')} sub={t('cw_sub')}>
	{#snippet right()}
		<PhaseChip phase="v1.0" />
		<a class="btn" href="/connections">{t('cw_back')}</a>
	{/snippet}
</PageHeader>

{#if profiles.error}
	<NotReady error={profiles.error} onretry={() => profiles.run(getConnectorProfiles)} />
{:else if !profiles.data}
	<div class="empty">{t('loading')}</div>
{:else}
	{@const p = profiles.data}
	<section class="card step" aria-labelledby="cw-vendor">
		<h3 id="cw-vendor">{t('cw_vendor')}</h3>
		<div class="dim d">{t('cw_vendor_d')}</div>
		<div class="vendors" role="radiogroup" aria-labelledby="cw-vendor">
			{#each p.vendors as v (v.key)}
				<label class="vd" class:on={vendorKey === v.key} data-vendor={v.key}>
					<input type="radio" name="vendor" value={v.key} checked={vendorKey === v.key} onchange={() => pickVendor(v.key)} />
					<span class="vn">{v.name}</span>
					<span class="dim vt">{t(v.client_type === 'confidential' ? 'cw_confidential' : 'cw_public')}</span>
					{#if !v.verified_live}<span class="pill p-watch"><i aria-hidden="true"></i>{t('cw_not_verified')}</span>{/if}
				</label>
			{/each}
		</div>
	</section>

	{#if vendor}
		{#if !vendor.verified_live}<div class="note" role="note" style="color:var(--watch);margin:0 0 10px">{t('cw_not_verified_d')}</div>{/if}

		<section class="card step" aria-labelledby="cw-server">
			<h3 id="cw-server">{t('cw_server')}</h3>
			{#if serverPick(vendor) === 'directory'}
				<div class="dim d">{t('cw_server_dir_d')}</div>
				<form class="p2row" onsubmit={search}>
					<label class="grow">{t('cw_search')}<input class="p2in" bind:value={q} maxlength="120" autocomplete="off" /></label>
					<button class="btn" type="submit" disabled={searching || !q.trim()}>{t('cw_search_go')}</button>
				</form>
				{#if directory && !directory.ready}<div class="note">{t('cw_dir_wait')}</div>{:else if directory?.stale}<div class="note" style="color:var(--watch)">{t('cw_dir_stale')}</div>{/if}
				{#if searchError}<div class="p2err" role="alert" title={searchError.raw}>{searchError.text}</div>{/if}
				{#if hits && !hits.length}<div class="note" role="status">{t('cw_search_none')}</div>{/if}
				{#if hits?.length}
					<fieldset class="list">
						<legend class="sr">{t('cw_search')}</legend>
						{#each hits as h (h.endpoint + h.name)}
							<label class="opt"><input type="radio" name="server" checked={choice === 'hit:' + h.endpoint && facility === h.name} onchange={() => pickHit(h)} /> <span>{h.name}{#if h.state}<span class="dim">&nbsp;· {h.state}</span>{/if}</span><span class="mono dim srv">{h.endpoint}</span></label>
						{/each}
					</fieldset>
				{/if}
			{:else}
				<div class="dim d">{t('cw_server_list_d')}</div>
				<fieldset class="list">
					<legend class="sr">{t('cw_server')}</legend>
					{#each vendor.servers as s (s)}
						<label class="opt"><input type="radio" name="server" checked={choice === 'srv:' + s} onchange={() => ((choice = 'srv:' + s), (facility = null))} /> <span class="mono srv">{s}</span></label>
					{/each}
				</fieldset>
			{/if}
			<label class="opt"><input type="radio" name="server" checked={choice === 'other'} onchange={() => ((choice = 'other'), (facility = null))} /> <span>{t('cw_other')}</span></label>
			{#if choice === 'other'}
				<label class="sub-l">{t('cw_other_d')}<input class="p2in mono" bind:value={other} maxlength="2048" autocomplete="off" inputmode="url" placeholder="https://" /></label>
			{/if}
			{#if server}<div class="note" role="status">{tf('cw_server_picked', { server })}</div>{/if}
		</section>

		<section class="card step" aria-labelledby="cw-register">
			<h3 id="cw-register">{tf('cw_register', { vendor: vendor.name })}</h3>
			<ol class="steps">
				<li>{t('cw_step_open')} <a href={vendor.registration} target="_blank" rel="noopener noreferrer">{t('cw_open_reg')} ↗</a></li>
				<li>{t(vendor.client_type === 'confidential' ? 'cw_step_type_confidential' : 'cw_step_type_public')}</li>
				<li>
					{t('cw_step_redirect')}
					<div class="cp">
						<pre class="code" bind:this={redirectEl} data-testid="cw-redirect">{p.redirect_uri}</pre>
						<button class="btn sm" type="button" onclick={() => copy(p.redirect_uri, 'redirect', redirectEl)}>{copied?.what === 'redirect' && copied.ok ? t('i2_copied') : t('i2_copy')}</button>
					</div>
					{@render copyHand('redirect')}
				</li>
				<li>
					{t('cw_step_scopes')}
					<div class="cp">
						<pre class="code" bind:this={scopesEl} data-testid="cw-scopes">{scopeLine(vendor)}</pre>
						<button class="btn sm" type="button" onclick={() => vendor && copy(scopeLine(vendor), 'scopes', scopesEl)}>{copied?.what === 'scopes' && copied.ok ? t('i2_copied') : t('i2_copy')}</button>
					</div>
					{@render copyHand('scopes')}
				</li>
				<li>{t('cw_step_paste')}</li>
			</ol>
			<div class="facts">
				<b>{t('cw_tokens')}</b>
				<ul>
					{#each tokenFacts(vendor) as f (f.key)}<li>{tf(f.key, f.vars)}</li>{/each}
				</ul>
			</div>
		</section>

		{#if server}
			<section class="card step" aria-labelledby="cw-client">
				<h3 id="cw-client">{t('cw_client')}</h3>
				{#if saved}<div class="p2ok" role="status">{t(saved === 'secret' ? 'cw_saved' : 'cw_saved_public')}</div>{/if}
				{#if client && !replacing}
					<div class="p2row">
						<span>{tf('cw_have_client', { client_id: client.client_id })}{#if client.confidential}&nbsp;{t('cw_have_client_secret')}{/if}</span>
						<button class="btn sm" type="button" onclick={() => ((replacing = true), (saved = null))}>{t('cw_replace')}</button>
					</div>
				{:else if clients.data}
					<form class="p2form" novalidate onsubmit={save} aria-labelledby="cw-client">
						{#if client}<div class="note" style="color:var(--watch)">{t('cw_replace_d')}</div>{/if}
						<label>{t('cw_client_id')}<input class="p2in mono" bind:value={clientId} maxlength="200" autocomplete="off" spellcheck="false" /></label>
						{#if needsSecret(vendor)}
							<label>{t('cw_client_secret')}<input class="p2in mono" type="password" bind:value={secret} maxlength="1000" autocomplete="off" spellcheck="false" /></label>
							<div class="dim d">{t('cw_secret_d')}</div>
						{/if}
						{#if saveError}<div class="p2err" role="alert" title={saveError.raw}>{saveError.text}</div>{/if}
						<div class="p2row">
							<button class="btn pri" type="submit" disabled={saving || !clientId.trim() || (needsSecret(vendor) && !secret)}>{t('cw_save')}</button>
							{#if client}<button class="btn" type="button" onclick={() => ((replacing = false), (secret = ''), (clientId = ''), (saveError = null))}>{t('cw_cancel')}</button>{/if}
						</div>
					</form>
				{:else if clients.error}
					<NotReady error={clients.error} onretry={loadClients} />
				{/if}
			</section>
		{/if}

		{#if server && client && !replacing}
			<section class="card step" aria-labelledby="cw-connect">
				<h3 id="cw-connect">{t('cw_connect')}</h3>
				{#if !mine.length}
					<div class="note">{t('cw_no_profiles')}</div>
				{:else}
					<form class="p2form" novalidate onsubmit={connect} aria-labelledby="cw-connect">
						<label>{t('cw_profile')}
							<select class="p2in" value={whose} onchange={(e) => (profileId = e.currentTarget.value)}>
								{#each mine as m (m.id)}<option value={m.id}>{m.display_name}</option>{/each}
							</select>
						</label>
						<label>{t('cw_label')}<input class="p2in" bind:value={label} maxlength="120" placeholder={defaultLabel(server, facility)} /></label>
						{#if launchError}<div class="p2err" role="alert" title={launchError.raw}>{launchError.text}</div>{/if}
						<div class="p2row">
							<button class="btn pri" type="submit" disabled={launching}>{tf('cw_go', { vendor: vendor.name })}</button>
							<span class="dim d">{t('cw_go_d')}</span>
						</div>
					</form>
				{/if}
			</section>
		{/if}
	{/if}
{/if}

<style>
	.step {
		display: grid;
		gap: 8px;
		margin-bottom: 10px;
	}
	.step h3 {
		margin: 0;
	}
	.d {
		font-size: 12px;
	}
	.vendors {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
		gap: 8px;
	}
	.vd {
		display: grid;
		gap: 3px;
		align-content: start;
		border: 1px solid var(--line2);
		border-radius: 10px;
		padding: 8px 10px;
		cursor: pointer;
		position: relative;
	}
	.vd.on {
		border-color: var(--accent);
	}
	.vd:has(input:focus-visible) {
		outline: 2px solid var(--accent);
		outline-offset: 1px;
	}
	.vd input {
		position: absolute;
		opacity: 0;
		pointer-events: none;
	}
	.vn {
		font-weight: 600;
	}
	.vt {
		font-size: 11.5px;
	}
	.vd .pill {
		justify-self: start;
		color: var(--watch);
	}
	.list {
		border: 0;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
	}
	.sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
	.opt {
		display: flex;
		gap: 8px;
		align-items: baseline;
		flex-wrap: wrap;
		font-size: 12.5px;
		cursor: pointer;
	}
	.srv {
		font-size: 11px;
		overflow-wrap: anywhere;
		min-width: 0;
	}
	.grow {
		flex: 1 1 240px;
		display: grid;
		gap: 3px;
		font-size: 11.5px;
		color: var(--muted);
	}
	.sub-l {
		display: grid;
		gap: 3px;
		font-size: 11.5px;
		color: var(--muted);
		margin-left: 22px;
	}
	.steps {
		margin: 0;
		padding-left: 20px;
		display: grid;
		gap: 8px;
		font-size: 12.5px;
	}
	.cp {
		display: flex;
		gap: 8px;
		align-items: flex-start;
		margin-top: 4px;
	}
	.cp pre {
		flex: 1 1 auto;
		min-width: 0;
		margin: 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		user-select: all;
	}
	.facts {
		font-size: 12px;
	}
	.facts ul {
		margin: 4px 0 0;
		padding-left: 18px;
	}
</style>

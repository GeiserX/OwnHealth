<script lang="ts">
	import { invalidateAll } from '$app/navigation';
	import PageHeader from '#lib/components/PageHeader.svelte';
	import { tf } from '#lib/components/pages1/util.svelte';
	import { t } from '#lib/i18n/index.svelte';
	import { copyText } from '#lib/logic/inbox';
	import type { PageData } from './$types';

	let { data }: { data: PageData } = $props();

	// Default scopes and authorization settings in the Open Wearables 0.11.0 pin.
	const vendors = {
		oura: {
			name: 'Oura',
			env: 'OURA',
			portal: 'https://cloud.ouraring.com/',
			terms: 'https://cloud.ouraring.com/legal/api-agreement',
			scopes: 'personal daily heartrate workout session spo2 ring_configuration heart_health',
			publicHttps: true
		},
		polar: {
			name: 'Polar',
			env: 'POLAR',
			portal: 'https://admin.polaraccesslink.com/',
			terms: 'https://www.polar.com/en/legal/polar-api-agreement',
			scopes: 'accesslink.read_all',
			publicHttps: false
		},
		whoop: {
			name: 'WHOOP',
			env: 'WHOOP',
			portal: 'https://developer-dashboard.whoop.com/',
			terms: 'https://developer.whoop.com/api-terms-of-use/',
			scopes: 'offline read:cycles read:sleep read:recovery read:workout',
			publicHttps: false
		},
		withings: {
			name: 'Withings',
			env: 'WITHINGS',
			portal: 'https://developer.withings.com/',
			terms: 'https://media.withings.com/web/legal/legal-pdf/api-terms-of-use/wo-en-api-terms-of-use-20180912.pdf',
			scopes: 'user.info,user.metrics,user.activity',
			publicHttps: true
		}
	};

	function vendorFor(id: string) {
		return Object.hasOwn(vendors, id) ? vendors[id as keyof typeof vendors] : undefined;
	}

	const setup = $derived(data.setup);
	const provider = $derived(setup?.providers.find((p) => p.id === data.id));
	const vendor = $derived(provider && vendorFor(provider.id));
	const docs = 'https://github.com/GeiserX/OwnHealth/blob/main/deploy/README.md#wearables';
	const authorize = $derived(provider
		? `docker compose exec -T ow-app python -c 'import json, sys, urllib.request; print(json.load(urllib.request.urlopen(f"http://127.0.0.1:8000/api/v1/oauth/${provider.id}/authorize?user_id={sys.argv[1]}"))["authorization_url"])' OPEN_WEARABLES_USER_UUID`
		: '');

	let copied = $state<{ id: string; ok: boolean } | null>(null);
	async function copy(field: HTMLTextAreaElement) {
		const ok = await copyText(navigator.clipboard, field.value);
		if (!ok) {
			field.focus();
			field.select();
		}
		copied = { id: field.id, ok };
	}
</script>

{#snippet copyField(id: string, label: string, copyLabel: string, value: string)}
	<div class="copy-field">
		<label for={id}>{label}</label>
		<div class="copy-row">
			<textarea {id} class="mono" rows="2" {value} readonly spellcheck="false"></textarea>
			<button class="btn sm" aria-label={copyLabel} onclick={() => {
				const field = document.getElementById(id);
				if (field instanceof HTMLTextAreaElement) void copy(field);
			}}>{t('i2_copy')}</button>
		</div>
		{#if copied?.id === id}
			<div class="dim" role="status">{t(copied.ok ? 'i2_copied' : 'ow_setup_copy_hand')}</div>
		{/if}
	</div>
{/snippet}

{#if !setup && data.forbidden}
	<PageHeader title={t('ow_setup_index')}>
		{#snippet right()}<a class="btn" href="/connections">{t('con_nav')}</a>{/snippet}
	</PageHeader>
	<p class="p2err" role="alert">{t('ow_setup_forbidden')}</p>
{:else if !setup}
	<PageHeader title={t('ow_setup_index')} />
	<p class="p2err" role="alert">{t('ow_setup_load_failed')}</p>
	<button class="btn" onclick={() => invalidateAll()}>{t('ow_setup_retry')}</button>
{:else if data.id && (!provider || !vendor)}
	<PageHeader title={t('ow_setup_missing')}>
		{#snippet right()}<a class="btn" href="/connections/wearables">{t('ow_setup_providers')}</a>{/snippet}
	</PageHeader>
{:else if !provider || !vendor}
	<PageHeader title={t('ow_setup_index')} sub={t('ow_setup_intro')}>
		{#snippet right()}<a class="btn" href="/connections">{t('con_nav')}</a>{/snippet}
	</PageHeader>
	<ul class="choices" aria-label={t('ow_setup_providers')} role="list">
		{#each setup.providers as item (item.id)}
			{@const choice = vendorFor(item.id)}
			{#if choice}
				<li><a class="card" href={`/connections/wearables/${item.id}`}>{choice.name}</a></li>
			{/if}
		{/each}
	</ul>
{:else}
	<PageHeader title={tf('ow_setup_connect', { vendor: vendor.name })}>
		{#snippet right()}<a class="btn" href="/connections/wearables">{t('ow_setup_providers')}</a>{/snippet}
	</PageHeader>
	<div class="guide">
		<div class="policy">
			<span class="pill" class:p-watch={provider.mcp === 'withheld'} class:p-ok={provider.mcp === 'served'}>
				{t(provider.mcp === 'withheld' ? 'ow_setup_withheld' : 'ow_setup_served')}
			</span>
			<a href={vendor.terms} target="_blank" rel="noreferrer">{tf('ow_setup_terms', { vendor: vendor.name })}</a>
			<p class="dim">{t(provider.mcp === 'withheld' ? 'ow_setup_withheld_d' : 'ow_setup_served_d')}</p>
		</div>
		<ol class="card steps">
			<li>
				<h2>{t('ow_setup_enable')}</h2>
				<p>{t('ow_setup_enable_d')} <a href={docs} target="_blank" rel="noreferrer">{t('ow_setup_deploy_docs')}</a></p>
				{#if setup.origin_fix}
					<p class="p2err" role="alert">{tf('ow_setup_origin_fix', { setting: setup.origin_fix.setting })}</p>
					{#if setup.origin_fix.value !== null}
						<code class="mono">{setup.origin_fix.setting}={setup.origin_fix.value}</code>
					{/if}
				{:else}
					<code class="mono">API_BASE_URL={setup.api_base_url}</code>
				{/if}
				{#if vendor.publicHttps}
					<p class="dim">{tf('ow_setup_https', { vendor: vendor.name })}</p>
				{/if}
			</li>
			<li>
				<h2>{t('ow_setup_register')}</h2>
				<p>{t('ow_setup_register_d')} <a href={vendor.portal} target="_blank" rel="noreferrer">{tf('ow_setup_portal', { vendor: vendor.name })}</a></p>
				{#if provider.redirect_uri}
					{@render copyField('ow-redirect', t('ow_setup_redirect'), t('ow_setup_copy_redirect'), provider.redirect_uri)}
				{/if}
				{@render copyField('ow-scopes', t('ow_setup_scopes'), t('ow_setup_copy_scopes'), vendor.scopes)}
				<p>{tf('ow_setup_credentials', { id: `${vendor.env}_CLIENT_ID`, secret: `${vendor.env}_CLIENT_SECRET` })}</p>
			</li>
			<li>
				<h2>{t('ow_setup_authorize')}</h2>
				<p>{t('ow_setup_authorize_d')}</p>
				<pre class="mono"><code>{authorize}</code></pre>
			</li>
			<li>
				<h2>{t('ow_setup_link')}</h2>
				<p>{t('ow_setup_link_d')} <a href={docs} target="_blank" rel="noreferrer">{t('ow_setup_deploy_docs')}</a></p>
			</li>
		</ol>
	</div>
{/if}

<style>
	.choices {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(145px, 1fr));
		gap: 12px;
		max-width: 820px;
		list-style: none;
		padding: 0;
	}
	.choices a {
		display: block;
		color: var(--text);
		font-size: 15px;
		font-weight: 600;
		text-decoration: none;
	}
	.choices a:hover,
	.choices a:focus-visible { border-color: var(--accent); }
	.guide { max-width: 820px; }
	.policy { margin: 0 0 16px; }
	.policy .pill {
		display: inline-block;
		margin: 0 10px 0 0;
	}
	.policy p { margin: 6px 0 0; }
	.steps {
		margin: 0;
		padding: 10px 20px 10px 41px;
	}
	.steps li { padding: 10px 0 15px 5px; }
	.steps li + li { border-top: 1px solid var(--line); }
	.steps li::marker {
		color: var(--dim);
		font-weight: 650;
	}
	h2 {
		font-size: 15px;
		margin: 0 0 6px;
	}
	p { line-height: 1.5; margin: 0 0 10px; }
	p:last-child { margin-bottom: 0; }
	code { overflow-wrap: anywhere; }
	pre {
		margin: 0;
		border: 1px solid var(--line);
		border-radius: 6px;
		padding: 8px;
		white-space: pre-wrap;
	}
	.copy-field { margin: 12px 0; }
	label { display: block; margin-bottom: 4px; font-weight: 600; }
	.copy-row {
		display: flex;
		gap: 8px;
		align-items: flex-start;
	}
	textarea {
		flex: 1;
		min-width: 0;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--bg);
		color: var(--text);
		padding: 6px 8px;
		resize: vertical;
	}
</style>

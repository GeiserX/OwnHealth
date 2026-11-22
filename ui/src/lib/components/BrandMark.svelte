<!-- The OwnHealth mark: the teal O and the navy H of the Figtree Tide logo on its light plate
     (docs/images/icon.svg). The plate is part of the drawing because the navy H vanishes on the dark UI.
     The viewBox crops the 512 px logo to the letters, so they fill the box at 28 px. -->
<script lang="ts">
	let { size = 28 }: { size?: number } = $props();
</script>

<svg class="brandmark" width={size} height={size} viewBox="24 24 464 464" aria-hidden="true" focusable="false"
	><rect x="24" y="24" width="464" height="464" rx="132" fill="#eef4f6" /><path d="m158.7 358.3q-29 0-51.2-13.1t-34.9-36.1-12.6-52.7q0-29.9 12.6-52.9t34.7-36.1 50.8-13.1 50.8 13.1 34.8 36.1 12.6 52.9q0 29.7-12.5 52.7t-34.5 36.1-50.6 13.1zm0-41.8q15.8 0 27.9-7.6t18.7-21.3 6.7-31.2q0-17.8-6.9-31.4t-19.1-21.5q-12-7.8-27.9-7.8t-27.9 7.8-19 21.5-7 31.4q0 17.5 7 31.2t19.3 21.3 28.2 7.6z" fill="#0e7490" /><path d="m279 354.9v-197.8h45.4v76.2h82.2v-75.9h45.4v197.5h-45.4v-79.8h-82.2v79.8z" fill="#0f1e2e" /></svg
>

<style>
	.brandmark {
		display: block;
	}
</style>

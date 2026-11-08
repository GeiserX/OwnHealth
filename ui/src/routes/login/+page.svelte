<script lang="ts">
	// Login, first run and recovery. The screen also shows wherever the app is not logged in, once the
	// root layout renders LoginScreen for a 401 (see .lanes/I-ui-2-requests.md).
	import LoginScreen from '#lib/components/pages2/LoginScreen.svelte';
</script>

<LoginScreen ondone={() => (location.href = '/')} />

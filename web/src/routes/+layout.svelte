<script lang="ts">
	import '../app.css';
	import favicon from '$lib/assets/favicon.svg';
	import { onMount, untrack } from 'svelte';
	import AppShell from '$lib/components/shell/AppShell.svelte';
	import CommandPalette from '$lib/components/shell/CommandPalette.svelte';
	import { Toaster } from '$lib/components/ui/sonner/index.js';
	import { connection } from '$lib/stores/connection.svelte';
	import { layout } from '$lib/stores/layout.svelte';
	import { palette } from '$lib/stores/palette.svelte';
	import { workspace } from '$lib/stores/workspace.svelte';
	import { toast } from '$lib/stores/toast';
	import type { LayoutData } from './$types';

	let { children, data }: { children: import('svelte').Snippet; data: LayoutData } = $props();

	// Hydrate the Navigator's topic list from SSR load data (and on client nav).
	// `setThemes` both reads and writes the store's `topics` state, so the mutation
	// is untracked — the effect depends only on `data.themes`, never on the store
	// state it writes (which would otherwise be an infinite update loop).
	$effect(() => {
		const themes = data.themes;
		untrack(() => workspace.setThemes(themes));
	});

	// One-time client wiring: responsive tracking, SSE stream, ⌘K shortcut.
	onMount(() => {
		layout.mount();
		connection.connect();
		if (data.themesError) {
			toast.error(`工作区加载失败:${data.themesError}`);
		}

		const onKey = (e: KeyboardEvent) => {
			if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
				e.preventDefault();
				palette.toggle();
			} else if (e.key === 'Escape' && palette.open) {
				palette.hide();
			}
		};
		window.addEventListener('keydown', onKey);

		return () => {
			window.removeEventListener('keydown', onKey);
			layout.destroy();
			connection.disconnect();
		};
	});
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
	<title>AI-Write</title>
</svelte:head>

<AppShell>
	{@render children()}
</AppShell>

<CommandPalette />
<Toaster position="bottom-right" />

<script lang="ts">
	import { connection, type ConnState } from '$lib/stores/connection.svelte';

	// Top-bar realtime indicator (ui-ux-design-draft §5). Three semantic states
	// driven by the SSE store: live / reconnecting / offline.
	const meta: Record<ConnState, { label: string; color: string; pulse: boolean }> = {
		connecting: { label: 'connecting', color: 'var(--color-warn)', pulse: true },
		live: { label: 'live', color: 'var(--color-live)', pulse: true },
		reconnecting: { label: 'reconnecting', color: 'var(--color-warn)', pulse: true },
		offline: { label: 'offline', color: 'var(--color-ink-ghost)', pulse: false }
	};

	const m = $derived(meta[connection.state]);
</script>

<div
	class="border-rule bg-card text-ink-soft flex h-[30px] items-center gap-[8px] rounded-[3px] border px-[12px] font-mono text-[12px]"
	title="实时连接状态(SSE)"
	role="status"
	aria-live="polite"
>
	<span
		class="h-[8px] w-[8px] rounded-full"
		style:background={m.color}
		style:animation={m.pulse ? 'softpulse 1.7s ease-in-out infinite' : 'none'}
	></span>
	{m.label}
</div>

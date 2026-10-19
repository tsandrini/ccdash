<script lang="ts">
	// A message between sessions, or from the owner, at its time.
	import type { Message, Session } from '#lib/api/types.js';
	import { fmtStamp } from '#lib/format.js';
	import { live } from '#lib/live/store.svelte.js';
	import { label } from '#lib/session.js';

	let {
		message: m,
		session: s,
		pending = false
	}: { message: Message; session: Session; pending?: boolean } = $props();

	const sent = $derived(m.from_session === s.id);
	const other = $derived(
		sent
			? live.sessions.get(m.to_session)
			: m.from_session
				? live.sessions.get(m.from_session)
				: undefined
	);
	const who = $derived(
		sent
			? `To ${other ? label(other) : m.to_session.slice(0, 8)}${m.to_host && live.multiHost ? ` on ${m.to_host}` : ''}`
			: `From ${m.from_session ? (other ? label(other) : m.from_label) : m.from_label}${m.from_host && live.multiHost ? ` on ${m.from_host}` : ''}`
	);
	const status = $derived(
		pending
			? 'waiting for the next prompt'
			: sent
				? m.read_at
					? 'read'
					: m.delivered_at
						? 'delivered'
						: 'waiting for its next prompt'
				: m.read_at
					? 'read with the prompt'
					: 'delivered with the prompt'
	);
	const link = $derived(sent ? m.to_session : m.from_session);
</script>

<div class="flex flex-col gap-1 {sent ? 'items-end' : 'items-start'} {pending ? 'opacity-80' : ''}">
	<span class="text-xs text-muted-foreground">
		<span class="font-medium">Message</span>
		{#if link && other}<a class="underline" href="/sessions/{link}">{who}</a>{:else}{who}{/if}
		· {fmtStamp(m.sent_at)} · {status}
	</span>
	<div
		class="max-w-[90%] rounded-lg border border-dashed bg-muted px-3 py-2 text-sm whitespace-pre-wrap"
	>
		{m.text}
	</div>
</div>

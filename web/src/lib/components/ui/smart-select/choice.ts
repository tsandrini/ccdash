/** One row of a {@link SmartSelect}. */
export type Choice = {
	/** What the select's value becomes; unique within the list. */
	value: string;
	/** The name shown in the row and, once picked, in the trigger. */
	label: string;
	/** A few words beside the name: what the choice is for. */
	hint?: string;
	/** A short figure at the row's end: a count, an id, an address. */
	meta?: string;
	/** Heading the row sits under; rows of one group must be adjacent. */
	group?: string;
	/** More words the search matches, never shown. */
	keywords?: string[];
	disabled?: boolean;
};

/**
 * Whether a choice answers a search: every word of the query is found in
 * its name, value, hint, figure, group or keywords, in any order.
 *
 * @param choice - The row.
 * @param query - What was typed; blank matches everything.
 * @returns True when the row stays listed.
 *
 * @example
 * matches({ value: 'claude-opus-5-5', label: 'Opus 5.5' }, 'opus 5') // true
 */
export function matches(choice: Choice, query: string): boolean {
	const words = query.toLowerCase().split(/\s+/).filter(Boolean);
	if (!words.length) return true;
	const hay = [
		choice.label,
		choice.value,
		choice.hint,
		choice.meta,
		choice.group,
		...(choice.keywords ?? [])
	]
		.filter(Boolean)
		.join(' ')
		.toLowerCase();
	return words.every((w) => hay.includes(w));
}

// The note side panel: opened from the board and the graph, keeping a
// stack so links inside a note go back.

type Ref = { ws: string; slug: string };

class NotePanel {
	open = $state(false);
	current = $state<Ref | null>(null);
	stack = $state<Ref[]>([]);

	show(ws: string, slug: string) {
		if (this.open && this.current) this.stack = [...this.stack, this.current];
		else this.stack = [];
		this.current = { ws, slug };
		this.open = true;
	}

	back() {
		const previous = this.stack.at(-1);
		if (!previous) return;
		this.stack = this.stack.slice(0, -1);
		this.current = previous;
	}

	close() {
		this.open = false;
		this.stack = [];
	}
}

export const notePanel = new NotePanel();

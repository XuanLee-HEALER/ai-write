/**
 * Command-palette open state (⌘K / Ctrl-K). A tiny global so the top-bar entry
 * button and the keyboard shortcut both drive the same shadcn command dialog.
 */
class PaletteStore {
	open = $state(false);
	toggle() {
		this.open = !this.open;
	}
	show() {
		this.open = true;
	}
	hide() {
		this.open = false;
	}
}

export const palette = new PaletteStore();

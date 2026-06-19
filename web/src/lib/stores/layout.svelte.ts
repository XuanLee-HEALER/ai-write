/**
 * Responsive App-Shell state — the primitives later stages reuse.
 *
 * Implements the three-region collapse ladder from `ui-ux-design-draft.md §4.2`:
 * the Navigator is persistent on desktop, an overlay drawer at `md`, and a
 * full-screen drawer (paired with a bottom tab bar) at `sm`/`xs`; the Inspector
 * collapses to a segmented control on mobile. A single matchMedia-driven
 * breakpoint feeds derived booleans so components stay declarative.
 */
import { browser } from '$app/environment';

/** Form-factor breakpoints (px), aligned with the Tailwind tokens in app.css
 * and the ladder in ui-ux-design-draft §4.1. */
export const BP = { sm: 480, md: 768, lg: 1024, xl: 1280, xxl: 1680 } as const;

export type Breakpoint = 'xs' | 'sm' | 'md' | 'lg' | 'xl' | '2xl';

/** Mobile bottom-tab destinations (`ui-ux-design-draft.md §4.2`). */
export type MobileTab = 'workspace' | 'surface';

function classify(w: number): Breakpoint {
	if (w < BP.sm) return 'xs';
	if (w < BP.md) return 'sm';
	if (w < BP.lg) return 'md';
	if (w < BP.xl) return 'lg';
	if (w < BP.xxl) return 'xl';
	return '2xl';
}

class LayoutStore {
	/** The current breakpoint, tracked from the viewport width. */
	bp = $state<Breakpoint>('xl');
	/** Whether the Navigator drawer is open (md and below). */
	navOpen = $state(false);
	/** Whether the user manually collapsed the persistent Navigator (xl/2xl). */
	navCollapsed = $state(false);
	/** Active mobile bottom tab. */
	mobileTab = $state<MobileTab>('surface');

	#mounted = false;
	#onResize = () => {
		this.bp = classify(window.innerWidth);
	};

	/** Begin tracking the viewport. Idempotent; no-op during SSR. */
	mount() {
		if (!browser || this.#mounted) return;
		this.#mounted = true;
		this.#onResize();
		window.addEventListener('resize', this.#onResize, { passive: true });
	}

	/** Stop tracking the viewport. */
	destroy() {
		if (!browser || !this.#mounted) return;
		this.#mounted = false;
		window.removeEventListener('resize', this.#onResize);
	}

	openNav() {
		this.navOpen = true;
		this.mobileTab = 'workspace';
	}
	closeNav() {
		this.navOpen = false;
		if (this.mobileTab === 'workspace') this.mobileTab = 'surface';
	}
	toggleNav() {
		this.navOpen ? this.closeNav() : this.openNav();
	}

	/** Navigator is a persistent rail (xl / 2xl). */
	get navPersistent() {
		return (this.bp === 'xl' || this.bp === '2xl' || this.bp === 'lg') && !this.navCollapsed;
	}
	/** Navigator collapses to an overlay/full-screen drawer (md and below). */
	get navIsDrawer() {
		return this.bp === 'md' || this.bp === 'sm' || this.bp === 'xs';
	}
	/** The Inspector collapses into a segmented control inside the work surface. */
	get inspectorSegmented() {
		return this.bp === 'sm' || this.bp === 'xs';
	}
	/** Phone form factors get the bottom tab bar. */
	get hasBottomTabs() {
		return this.bp === 'sm' || this.bp === 'xs';
	}
}

/** The app-wide responsive layout store. Mounted from the root layout. */
export const layout = new LayoutStore();

/**
 * Thin wrapper over svelte-sonner so business code calls one app API for the
 * transient confirmations described in `ui-ux-design-draft.md §5` (toast =
 * momentary confirmation, distinct from the retraceable activity stream).
 */
import { toast as sonner } from 'svelte-sonner';

export const toast = {
	/** A neutral success/confirmation toast. */
	show(message: string): void {
		sonner.success(message);
	},
	/** An informational toast. */
	info(message: string): void {
		sonner(message);
	},
	/** An error toast (surfaces an `{error}` from the API). */
	error(message: string): void {
		sonner.error(message);
	}
};

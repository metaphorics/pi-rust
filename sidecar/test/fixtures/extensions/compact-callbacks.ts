/**
 * Fixture: drives `ctx.compact()` callback correlation. The trigger-compact
 * command queues a compact whose onComplete/onError append observable
 * entries tagged with the command args, and the session_before_compact
 * handler cancels only when the customInstructions say so — letting one
 * fixture exercise both the success and the cancelled-failure paths.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("trigger-compact", {
		description: "Queue a compact with correlated callbacks",
		handler: async (args, ctx) => {
			ctx.compact({
				...(args !== "" ? { customInstructions: args } : {}),
				onComplete: (result) => {
					pi.appendEntry("compact-complete", {
						tag: args,
						summary: result.summary,
						firstKeptEntryId: result.firstKeptEntryId,
						tokensBefore: result.tokensBefore,
						estimatedTokensAfter: result.estimatedTokensAfter ?? null,
						details: result.details ?? null,
					});
				},
				onError: (error) => {
					pi.appendEntry("compact-error", { tag: args, message: error.message });
				},
			});
		},
	});

	pi.on("session_before_compact", (event) => {
		if (event.customInstructions === "cancel-me") {
			return { cancel: true };
		}
		return undefined;
	});

	pi.on("session_compact", () => {
		// Subscribing keeps session_compact in subscribedEvents so the host
		// forwards it — mirrors a real extension awaiting its compact.
	});
}

/**
 * Fixture: a tool that registers a follow-up tool and activates it DURING
 * execution (pi's dynamic-tools corpus pattern / regression 6162). pi's own
 * wrapRegisteredTool computes `addedToolNames` by diffing active tools
 * around execute — over the bridge that diff runs against the state mirror,
 * so this exercises active-tool preservation AND the addedToolNames relay.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "load_more_tools",
		label: "Load More Tools",
		description: "Register a follow-up tool and make it active",
		parameters: Type.Object({}),
		execute: async () => {
			pi.registerTool({
				name: "after_load",
				label: "After Load",
				description: "Tool introduced mid-batch by load_more_tools",
				parameters: Type.Object({}),
				execute: async () => ({ content: [{ type: "text", text: "after" }], details: {} }),
			});
			pi.setActiveTools([...pi.getActiveTools(), "after_load"]);
			return { content: [{ type: "text", text: "loaded" }], details: {} };
		},
	});
}

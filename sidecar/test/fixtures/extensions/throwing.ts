/**
 * Fixture: handlers that throw, exercising pi's error asymmetry —
 * before_agent_start errors are caught and reported (agent continues),
 * while tool_call errors propagate uncaught by design (runner.ts).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

export default function (pi: ExtensionAPI) {
	pi.on("before_agent_start", () => {
		throw new Error("fixture before_agent_start failure");
	});

	pi.on("tool_call", (event) => {
		if (event.toolName === "exploding_gate") {
			throw new Error("fixture tool_call failure");
		}
		return undefined;
	});
}

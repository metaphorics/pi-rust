/**
 * Fixture: an unmodified pi extension exercising the registration surface —
 * tool (with streaming partials), command, flag, shortcut, and a spread of
 * event handlers (blocking and fire-and-forget).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "echo_tool",
		label: "Echo",
		description: "Echo the input text back",
		parameters: Type.Object({
			text: Type.String({ description: "Text to echo" }),
		}),
		execute: async (_toolCallId, params, _signal, onUpdate) => {
			onUpdate?.({ content: [{ type: "text", text: "working..." }], details: { stage: "half" } });
			return {
				content: [{ type: "text", text: `echo: ${params.text}` }],
				details: { length: params.text.length },
			};
		},
	});

	pi.registerCommand("simple-cmd", {
		description: "Record command invocations",
		handler: async (args) => {
			pi.appendEntry("simple-cmd-ran", { args });
		},
	});
	pi.registerFlag("simple-verbose", {
		description: "Verbose fixture flag",
		type: "boolean",
		default: false,
	});

	pi.registerShortcut("ctrl+alt+p", {
		description: "Fixture shortcut",
		handler: () => {
			pi.appendEntry("simple-shortcut-ran", {});
		},
	});
	pi.on("agent_start", () => {
		// Fire-and-forget handler; used by ordering assertions.
	});

	pi.on("input", (event) => {
		if (event.text.startsWith("rewrite:")) {
			return { action: "transform", text: event.text.replace("rewrite:", "rewritten:") };
		}
		if (event.text === "swallow") {
			return { action: "handled" };
		}
		return { action: "continue" };
	});

	pi.on("tool_call", (event) => {
		if (event.toolName === "forbidden_tool") {
			return { block: true, reason: "fixture forbids this tool" };
		}
		return undefined;
	});
	pi.on("session_before_compact", () => {
		return { cancel: true };
	});
}

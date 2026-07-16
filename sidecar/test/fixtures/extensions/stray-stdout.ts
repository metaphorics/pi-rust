/**
 * Fixture: an extension that prints to stdout/stderr at load time and inside
 * a handler. DELIBERATE console/stdout use — this file is the test subject
 * for the stdio-guard invariant (stray prints land in the virtual terminal
 * sink, never on the protocol channel).
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

// eslint-disable-next-line no-console -- deliberate: stdout-isolation test subject
console.log("stray load-time stdout");
process.stdout.write("stray load-time write\n");

export default function (pi: ExtensionAPI) {
	pi.on("agent_start", () => {
		// eslint-disable-next-line no-console -- deliberate: stdout-isolation test subject
		console.log("stray handler stdout");
		process.stderr.write("stray handler stderr\n");
	});
}

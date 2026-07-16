/**
 * Fixture: extension with its own committed node_modules dependency plus
 * imports from @earendil-works/pi-coding-agent — proves jiti resolves BOTH
 * the extension-local dependency and the single pinned pi package copy
 * (module identity, risk R1).
 */

import { SessionManager, defineTool } from "@earendil-works/pi-coding-agent";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
// Extension-local dependency (committed under this fixture's node_modules).
import { stamp } from "fixture-dep";
import { Type } from "typebox";

const identityTool = defineTool({
	name: "identity_probe",
	label: "Identity Probe",
	description: "Report jiti module-identity facts from inside the extension",
	parameters: Type.Object({
		text: Type.String(),
	}),
	execute: async (_toolCallId, params, _signal, _onUpdate, ctx) => {
		// Module identity check: the SessionManager class the extension sees
		// must be the same constructor the host's session mirror instantiates.
		const sameClass = ctx.sessionManager instanceof SessionManager;
		return {
			content: [{ type: "text", text: stamp(`${params.text}:${sameClass}`) }],
			details: { sameClass },
		};
	},
});

export default function (pi: ExtensionAPI) {
	pi.registerTool(identityTool);
}

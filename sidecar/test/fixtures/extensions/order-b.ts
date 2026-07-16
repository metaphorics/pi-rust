/** Fixture (pairs with order-a.ts): second extension in handler order. */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

interface OrderGlobal {
	__piFixtureOrder?: string[];
}

const globals = globalThis as OrderGlobal;

export default function (pi: ExtensionAPI) {
	pi.on("before_agent_start", () => {
		globals.__piFixtureOrder ??= [];
		globals.__piFixtureOrder.push("b:before_agent_start");
		return undefined;
	});

	pi.on("agent_start", () => {
		globals.__piFixtureOrder ??= [];
		globals.__piFixtureOrder.push("b:agent_start");
	});
}

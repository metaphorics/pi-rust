/**
 * Fixture (pairs with order-b.ts): records handler execution order into a
 * process-global array so tests can assert pi's sequential handler
 * semantics across extensions and events.
 */

import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

interface OrderGlobal {
	__piFixtureOrder?: string[];
}

const globals = globalThis as OrderGlobal;

export default function (pi: ExtensionAPI) {
	pi.on("before_agent_start", async () => {
		globals.__piFixtureOrder ??= [];
		globals.__piFixtureOrder.push("a:before_agent_start:start");
		// Yield so an interleaving bug would surface as b starting early.
		await Promise.resolve();
		globals.__piFixtureOrder.push("a:before_agent_start:end");
		return undefined;
	});

	pi.on("agent_start", () => {
		globals.__piFixtureOrder ??= [];
		globals.__piFixtureOrder.push("a:agent_start");
	});
}

/**
 * Fixture: drives the ExtensionUIContext surface — widgets (factory and
 * static), footer with FooterDataProvider, custom focusable components,
 * dialogs, terminal-input listeners, autocomplete stacking, status, and a
 * tool with custom renderers. Commands trigger each behavior so tests can
 * invoke them via command/execute.
 */

import type { Component, TUI } from "@earendil-works/pi-tui";
import type { ExtensionAPI, ReadonlyFooterDataProvider, Theme } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";

class CounterWidget implements Component {
	count = 0;
	disposed = false;
	private readonly tui: TUI;
	private readonly theme: Theme;

	constructor(tui: TUI, theme: Theme) {
		this.tui = tui;
		this.theme = theme;
	}

	render(width: number): string[] {
		return [this.theme.fg("accent", `count=${this.count} w=${width}`)];
	}

	handleInput(data: string): void {
		if (data === "+") {
			this.count += 1;
			this.tui.requestRender();
		}
	}

	invalidate(): void {}

	dispose(): void {
		this.disposed = true;
	}
}

export default function (pi: ExtensionAPI) {
	pi.registerTool({
		name: "render_tool",
		label: "Render Tool",
		description: "Tool with custom call/result renderers",
		parameters: Type.Object({ subject: Type.String() }),
		execute: async (_toolCallId, params) => {
			return { content: [{ type: "text", text: `ran ${params.subject}` }], details: { subject: params.subject } };
		},
		renderCall: (args, theme) => ({
			render: (width: number) => [theme.fg("accent", `calling ${args.subject} @${width}`)],
			invalidate: () => {},
		}),
		renderResult: (result, options, theme) => ({
			render: () => [theme.fg("success", `${options.expanded ? "full" : "brief"}: ${result.details?.subject ?? "?"}`)],
			invalidate: () => {},
		}),
	});

	pi.registerCommand("show-widget", {
		handler: async (_args, ctx) => {
			ctx.ui.setWidget("counter", (tui, theme) => new CounterWidget(tui, theme), { placement: "belowEditor" });
		},
	});

	pi.registerCommand("show-static-widget", {
		handler: async (_args, ctx) => {
			ctx.ui.setWidget("banner", ["line one", "line two"]);
		},
	});

	pi.registerCommand("clear-widget", {
		handler: async (_args, ctx) => {
			ctx.ui.setWidget("counter", undefined);
		},
	});

	pi.registerCommand("show-footer", {
		handler: async (_args, ctx) => {
			ctx.ui.setStatus("fixture", "status-live");
			ctx.ui.setFooter((_tui: TUI, theme: Theme, footerData: ReadonlyFooterDataProvider) => ({
				render: () => [theme.fg("muted", `footer:${footerData.getExtensionStatuses().get("fixture") ?? "none"}`)],
				invalidate: () => {},
			}));
		},
	});

	pi.registerCommand("set-status", {
		handler: async (args, ctx) => {
			ctx.ui.setStatus("fixture", args);
		},
	});

	pi.registerCommand("ask-select", {
		handler: async (args, ctx) => {
			const choice = await ctx.ui.select("Pick one", ["alpha", "beta"], {
				...(args === "timed" ? { timeout: 1500 } : {}),
			});
			pi.appendEntry("select-result", { choice: choice ?? null });
		},
	});

	pi.registerCommand("listen-input", {
		handler: async (_args, ctx) => {
			const unsubscribe = ctx.ui.onTerminalInput((data) => {
				if (data === "\u0010") return { consume: true };
				if (data === "x") return { data: "y" };
				return undefined;
			});
			pi.appendEntry("listening", { active: typeof unsubscribe === "function" });
		},
	});

	pi.registerCommand("stack-autocomplete", {
		handler: async (_args, ctx) => {
			ctx.ui.addAutocompleteProvider((current) => ({
				...current,
				getSuggestions: async () => ({
					items: [{ value: "fixture-item", label: "Fixture Item" }],
					prefix: "fi",
				}),
			}));
		},
	});

	pi.registerCommand("open-custom", {
		handler: async (_args, ctx) => {
			const result = await ctx.ui.custom<string>((tui, theme, _keybindings, done) => ({
				render: () => [theme.fg("accent", "custom body")],
				invalidate: () => {},
				handleInput: (data: string) => {
					if (data === "\r") done("confirmed");
					else tui.requestRender();
				},
			}));
			pi.appendEntry("custom-result", { result });
		},
	});

	pi.registerCommand("notify-things", {
		handler: async (_args, ctx) => {
			ctx.ui.notify("hello there", "warning");
			ctx.ui.setWorkingMessage("crunching");
			ctx.ui.setWorkingVisible(false);
			ctx.ui.setWorkingIndicator({ frames: ["|", "/"], intervalMs: 120 });
			ctx.ui.setTitle("fixture title");
			ctx.ui.setEditorText("drafted");
			ctx.ui.setToolsExpanded(true);
			pi.appendEntry("editor-text", { text: ctx.ui.getEditorText(), expanded: ctx.ui.getToolsExpanded() });
		},
	});

	pi.registerCommand("theme-things", {
		handler: async (_args, ctx) => {
			const catalog = ctx.ui.getAllThemes().map((entry) => entry.name);
			const current = ctx.ui.theme;
			const styled = current.fg("accent", "sample");
			pi.appendEntry("theme-info", { catalog, styled });
		},
	});
}

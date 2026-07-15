import { existsSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

interface ModelLike {
	id: string;
	name: string;
	api: string;
	provider: string;
	baseUrl: string;
	reasoning: boolean;
	input: string[];
	cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
	contextWindow: number;
	maxTokens: number;
	[key: string]: unknown;
}

interface ResolvedCatalogRoot {
	/** Package root that contains either src/ or dist/ catalog sources. */
	root: string;
	/** Absolute path to models.generated.{ts,js}. */
	generatedFile: string;
	/** Absolute path to providers/ directory holding *.models.{ts,js}. */
	providersDir: string;
	/** File extension of provider model modules (".ts" | ".js"). */
	modelsExt: ".ts" | ".js";
}

function resolveCatalogLayout(root: string): ResolvedCatalogRoot | null {
	const srcGenerated = join(root, "src/models.generated.ts");
	if (existsSync(srcGenerated)) {
		return {
			root,
			generatedFile: srcGenerated,
			providersDir: join(root, "src/providers"),
			modelsExt: ".ts",
		};
	}
	const distGenerated = join(root, "dist/models.generated.js");
	if (existsSync(distGenerated)) {
		return {
			root,
			generatedFile: distGenerated,
			providersDir: join(root, "dist/providers"),
			modelsExt: ".js",
		};
	}
	return null;
}

function findReferenceRoot(): ResolvedCatalogRoot {
	const configured = process.env.PI_AI_REF;
	if (configured) {
		const candidate = isAbsolute(configured) ? configured : resolve(process.cwd(), configured);
		const layout = resolveCatalogLayout(candidate);
		if (!layout) {
			throw new Error(
				`PI_AI_REF does not contain src/models.generated.ts or dist/models.generated.js: ${candidate}`,
			);
		}
		return layout;
	}

	// Dev path first: local .references checkout (src/, may include uncommitted catalog edits).
	const documentedRelative = resolve(import.meta.dir, "../../../.references/pi/packages/ai");
	const documentedLayout = resolveCatalogLayout(documentedRelative);
	if (documentedLayout) return documentedLayout;

	let current = import.meta.dir;
	while (dirname(current) !== current) {
		const candidate = join(current, ".references/pi/packages/ai");
		const layout = resolveCatalogLayout(candidate);
		if (layout) return layout;
		current = dirname(current);
	}

	const workstationFallback = "/home/alpha/exp/pi-rust/.references/pi/packages/ai";
	const workstationLayout = resolveCatalogLayout(workstationFallback);
	if (workstationLayout) return workstationLayout;

	// CI / hermetic path: published package installed under scripts/ (or repo root).
	for (const candidate of [
		resolve(import.meta.dir, "node_modules/@earendil-works/pi-ai"),
		resolve(import.meta.dir, "../node_modules/@earendil-works/pi-ai"),
	]) {
		const layout = resolveCatalogLayout(candidate);
		if (layout) return layout;
	}

	throw new Error(
		"Unable to locate pi-ai catalog sources (src/ or dist/); set PI_AI_REF or install @earendil-works/pi-ai@0.80.7 under scripts/",
	);
}

function rustString(value: string): string {
	return JSON.stringify(value);
}

function rustRawString(value: string): string {
	let hashes = "";
	while (value.includes(`"${hashes}`)) hashes += "#";
	return `r${hashes}"${value}"${hashes}`;
}

function stableJson(value: unknown): string {
	return JSON.stringify(value, (_key, item: unknown) => {
		if (item === null || Array.isArray(item) || typeof item !== "object") return item;
		return Object.fromEntries(
			Object.entries(item as Record<string, unknown>).sort(([left], [right]) =>
				left < right ? -1 : left > right ? 1 : 0,
			),
		);
	});
}

const layout = findReferenceRoot();
const generatedSource = await Bun.file(layout.generatedFile).text();
// Both models.generated.ts and the published dist .js keep `"provider": X_MODELS,` literals.
const canonicalProviders = new Set(
	[...generatedSource.matchAll(/^\s*"([^"]+)":\s*[A-Z0-9_]+_MODELS,/gm)].map((match) => match[1]),
);
const modelsGlob = `*${layout.modelsExt === ".ts" ? ".models.ts" : ".models.js"}`;
const files = [...new Bun.Glob(modelsGlob).scanSync({ cwd: layout.providersDir, absolute: true })].sort();

const models: ModelLike[] = [];
for (const file of files) {
	const base = file.slice(file.lastIndexOf("/") + 1);
	const provider = base.slice(0, -`.models${layout.modelsExt}`.length);
	if (!canonicalProviders.has(provider)) continue;
	// Specifier is discovered from the canonical provider registry at runtime
	// (src/*.models.ts locally, dist/*.models.js from the published package) —
	// a static import cannot enumerate the complete generated catalog.
	const module = (await import(pathToFileURL(file).href)) as Record<string, unknown>;
	const catalog = Object.values(module).find(
		(value): value is Record<string, ModelLike> =>
			typeof value === "object" && value !== null && !Array.isArray(value),
	);
	if (!catalog) throw new Error(`No model catalog export found in ${file}`);
	for (const model of Object.values(catalog)) models.push(model);
}

models.sort((a, b) => {
	const providerOrder = a.provider < b.provider ? -1 : a.provider > b.provider ? 1 : 0;
	if (providerOrder !== 0) return providerOrder;
	return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
});
if (!models.some((model) => model.provider === "anthropic")) throw new Error("Anthropic catalog is empty");
if (!models.some((model) => model.provider === "openai")) throw new Error("OpenAI catalog is empty");

const lines = [
	"// @generated by scripts/gen-models.ts; do not edit.",
	"use crate::models::ModelEntry;",
	"",
	"pub static MODELS: &[ModelEntry] = &[",
];
for (const model of models) {
	const raw = stableJson(model);
	lines.push("    ModelEntry {");
	lines.push(`        provider: ${rustString(model.provider)},`);
	lines.push(`        id: ${rustString(model.id)},`);
	lines.push(`        name: ${rustString(model.name)},`);
	lines.push(`        api: ${rustString(model.api)},`);
	lines.push(`        base_url: ${rustString(model.baseUrl)},`);
	lines.push(`        reasoning: ${model.reasoning},`);
	lines.push(`        context_window: ${model.contextWindow},`);
	lines.push(`        max_tokens: ${model.maxTokens},`);
	lines.push(`        raw_json: ${rustRawString(raw)},`);
	lines.push("    },");
}
lines.push("];", "");

const output = resolve(import.meta.dir, "../crates/pi-ai/src/models_generated.rs");
await Bun.write(output, lines.join("\n"));
// CI zero-diff gate: bun scripts/gen-models.ts && git diff --exit-code crates/pi-ai/src/models_generated.rs
process.stdout.write(
	`Generated ${models.length} models from ${canonicalProviders.size} providers (${layout.modelsExt} catalog at ${layout.root}) → ${output}\n`,
);

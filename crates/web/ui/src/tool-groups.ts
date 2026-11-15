// ── Tool-call grouping ───────────────────────────────────────
//
// Runs of consecutive tool cards (`.msg.exec-card`) in the message list fold
// into one collapsible row — "Used 6 tools · calc, browser" — instead of a
// card per call. Works as a post-processor on #messages, so the live
// streaming path and history rendering keep appending cards exactly as
// before; card lookups by id or `.exec-card` selector still find them inside
// the group. Approval prompts and assistant text between calls stay visible
// and simply start a new group after them.

const CHEVRON =
	'<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="m9 6 6 6-6 6"/></svg>';

function isCard(node: Element | null): node is HTMLElement {
	return !!node && node.classList.contains("msg") && node.classList.contains("exec-card");
}

function isGroup(node: Element | null): node is HTMLElement {
	return !!node && node.classList.contains("tool-group");
}

function createGroup(): HTMLElement {
	const group = document.createElement("div");
	group.className = "tool-group";
	const head = document.createElement("button");
	head.type = "button";
	head.className = "tool-group-head";
	head.setAttribute("aria-expanded", "false");
	// Safe: static markup plus text set via textContent below.
	head.innerHTML =
		'<span class="tool-group-dot" aria-hidden="true"></span><span class="tool-group-label"></span><span class="tool-group-names"></span>' +
		`<span class="tool-group-chevron">${CHEVRON}</span>`;
	const body = document.createElement("div");
	body.className = "tool-group-body";
	head.addEventListener("click", () => {
		const open = group.classList.toggle("open");
		head.setAttribute("aria-expanded", String(open));
	});
	group.append(head, body);
	return group;
}

function toolName(card: HTMLElement): string {
	const cmd = card.querySelector("[data-cmd]")?.textContent?.trim() || "";
	const first = cmd.split(/\s+/)[0] || "tool";
	// mcp__server__tool → server · tool_search stays as is
	const mcp = /^mcp__([^_]+)__(.+)$/.exec(first);
	return mcp ? (mcp[1] ?? first) : first;
}

function setText(el: Element, text: string): void {
	if (el.textContent !== text) el.textContent = text;
}

function refreshLabel(group: HTMLElement): void {
	const cards = Array.from(group.querySelectorAll<HTMLElement>(":scope > .tool-group-body > .msg.exec-card"));
	const running = cards.some((c) => c.classList.contains("running"));
	const failed = cards.filter((c) => c.classList.contains("exec-err")).length;
	const n = cards.length;
	const label = group.querySelector(".tool-group-label");
	const names = group.querySelector(".tool-group-names");
	// Only write when the text changes: every write is a subtree mutation that
	// would re-trigger the observer, regrouping (and forcing layout) every frame.
	if (label) {
		setText(
			label,
			running
				? `Working · ${n} step${n === 1 ? "" : "s"}`
				: `Used ${n} tool${n === 1 ? "" : "s"}${failed ? ` · ${failed} failed` : ""}`,
		);
	}
	if (names) {
		const unique = [...new Set(cards.map(toolName).filter((t) => t !== "tool_search"))];
		setText(names, unique.length ? unique.slice(0, 3).join(", ") + (unique.length > 3 ? "…" : "") : "");
	}
	group.classList.toggle("running", running);
	group.classList.toggle("has-error", failed > 0);
}

/** Fold every top-level tool card into the group before it (or a new one). */
function regroup(box: HTMLElement): void {
	for (const child of Array.from(box.children)) {
		if (!isCard(child)) continue;
		let prev = child.previousElementSibling;
		if (!isGroup(prev)) {
			const group = createGroup();
			box.insertBefore(group, child);
			prev = group;
		}
		prev.querySelector(".tool-group-body")?.appendChild(child);
	}
	// Merge groups that became adjacent (e.g. a thinking indicator between
	// two calls was removed), then refresh every label.
	for (const group of Array.from(box.querySelectorAll<HTMLElement>(":scope > .tool-group"))) {
		const prev = group.previousElementSibling;
		if (isGroup(prev)) {
			const body = prev.querySelector(".tool-group-body");
			for (const card of Array.from(group.querySelectorAll(":scope > .tool-group-body > *"))) body?.appendChild(card);
			group.remove();
		}
	}
	for (const group of Array.from(box.querySelectorAll<HTMLElement>(":scope > .tool-group"))) {
		const body = group.querySelector(".tool-group-body");
		if (!body?.children.length) {
			group.remove();
			continue;
		}
		refreshLabel(group);
	}
}

let observed: HTMLElement | null = null;
let boxObserver: MutationObserver | null = null;

function watchBox(box: HTMLElement): void {
	if (observed === box) return;
	boxObserver?.disconnect();
	observed = box;
	let queued = false;
	const schedule = (): void => {
		if (queued) return;
		queued = true;
		requestAnimationFrame(() => {
			queued = false;
			if (box.isConnected) regroup(box);
		});
	};
	regroup(box);
	// Direct children (new cards) and card class changes (running → ok/err).
	boxObserver = new MutationObserver(schedule);
	boxObserver.observe(box, { childList: true, subtree: true, attributes: true, attributeFilter: ["class"] });
}

/** Attach to the chat's message list, following route changes. */
export function initToolGroups(): void {
	const find = (): void => {
		const box = document.getElementById("messages");
		if (box) watchBox(box);
	};
	find();
	const page = document.getElementById("pageContent");
	if (page) new MutationObserver(find).observe(page, { childList: true });
}

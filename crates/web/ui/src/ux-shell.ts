// ── App shell refresh (mobile-first) ─────────────────────────
//
// Adds the chrome the redesigned shell needs on top of the server-rendered
// index.html: a single mobile header (menu · chat title · new chat), a
// left drawer around the sessions panel with New chat / Settings / Sign out,
// and the stylesheet that restyles both. Done at runtime so the template
// stays untouched and the UI ships entirely as web assets.

import { closeSessions, isMobile, toggleSessions } from "./mobile";

const ICON_NEW_CHAT =
	'<svg width="21" height="21" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>';
const ICON_CLOSE =
	'<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><path d="M18 6 6 18M6 6l12 12"/></svg>';
const ICON_PLUS =
	'<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M12 5v14M5 12h14"/></svg>';
const ICON_MENU =
	'<svg width="22" height="22" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true"><path d="M4 7h16M4 12h16M4 17h10"/></svg>';

function el<K extends keyof HTMLElementTagNameMap>(
	tag: K,
	attrs: Record<string, string>,
	html = "",
): HTMLElementTagNameMap[K] {
	const node = document.createElement(tag);
	for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
	// Safe: only static, hardcoded markup is passed in.
	if (html) node.innerHTML = html;
	return node;
}

function addStylesheet(): void {
	if (document.getElementById("uxShellCss")) return;
	const base = document.querySelector<HTMLLinkElement>('link[href*="css/mobile.css"]');
	if (!base) return;
	const link = el("link", {
		id: "uxShellCss",
		rel: "stylesheet",
		href: base.href.replace("css/mobile.css", "css/ux.css"),
	});
	document.head.appendChild(link);
}

function startNewChat(): void {
	document.getElementById("newSessionBtn")?.click();
	if (isMobile()) closeSessions();
}

function buildHeader(): void {
	const header = document.querySelector("body > header");
	if (!header || document.getElementById("mobileChatTitle")) return;
	const menuBtn = document.getElementById("mobileMenuBtn");
	if (menuBtn) {
		// Swap the icon-font burger for the drawn one used across the shell.
		menuBtn.innerHTML = ICON_MENU;
		menuBtn.setAttribute("aria-label", "Open chat list");
	}
	const title = el("span", { id: "mobileChatTitle", class: "mobile-chat-title" });
	const newChat = el(
		"button",
		{ id: "mobileNewChatBtn", class: "mobile-header-btn", type: "button", title: "New chat", "aria-label": "New chat" },
		ICON_NEW_CHAT,
	);
	newChat.addEventListener("click", startNewChat);
	const palette = document.getElementById("commandPaletteBtn");
	header.insertBefore(title, palette);
	header.insertBefore(newChat, palette);
}

function buildDrawer(): void {
	const panel = document.getElementById("sessionsPanel");
	if (!panel || panel.querySelector(".drawer-head")) return;

	const head = el("div", { class: "drawer-head" });
	head.appendChild(el("span", { class: "drawer-brand" }, "moltis"));
	const close = el(
		"button",
		{ class: "mobile-header-btn", type: "button", "aria-label": "Close chat list" },
		ICON_CLOSE,
	);
	close.addEventListener("click", () => closeSessions());
	head.appendChild(close);

	const newChat = el("button", { class: "drawer-new-chat", type: "button" }, `${ICON_PLUS}<span>New chat</span>`);
	newChat.addEventListener("click", startNewChat);

	panel.prepend(newChat);
	panel.prepend(head);

	const foot = el("div", { class: "drawer-foot" });
	const settings = el(
		"button",
		{ class: "drawer-foot-item", type: "button" },
		'<span class="icon icon-settings" aria-hidden="true"></span><span>Settings</span>',
	);
	settings.addEventListener("click", () => {
		closeSessions();
		document.getElementById("mobileMenuSettingsBtn")?.click();
	});
	const signOut = el(
		"button",
		{ class: "drawer-foot-item drawer-foot-danger", type: "button" },
		'<span class="icon icon-logout" aria-hidden="true"></span><span>Sign out</span>',
	);
	signOut.addEventListener("click", () => document.getElementById("mobileMenuLogoutBtn")?.click());
	foot.append(settings, signOut);
	panel.appendChild(foot);
}

/** Menu button opens the chat drawer directly on phones (no intermediate popup). */
function rewireMenuButton(): void {
	const menuBtn = document.getElementById("mobileMenuBtn");
	if (!menuBtn) return;
	menuBtn.addEventListener(
		"click",
		(e) => {
			if (!isMobile()) return;
			// Capture phase: stop app.tsx's popup toggle from also running.
			e.stopImmediatePropagation();
			e.preventDefault();
			toggleSessions();
		},
		true,
	);
}

/** Mirror the active session's name into the mobile header. */
function syncTitle(): void {
	const title = document.getElementById("mobileChatTitle");
	if (!title) return;
	let queued = false;
	const update = (): void => {
		queued = false;
		const name = document.querySelector("#sessionNameMount .chat-session-name");
		title.textContent = location.pathname.startsWith("/chats") ? (name?.textContent || "").trim() : "";
		adoptToolbarControls();
	};
	const schedule = (): void => {
		if (queued) return;
		queued = true;
		requestAnimationFrame(update);
	};
	update();
	// Only structural changes under the page (route swaps, header re-renders),
	// coalesced to one read per frame so streaming replies stay cheap.
	const page = document.getElementById("pageContent");
	if (page) new MutationObserver(schedule).observe(page, { subtree: true, childList: true });
	window.addEventListener("popstate", schedule);
}

// ── Chat menu (⋯) ────────────────────────────────────────────
//
// The chat toolbar's controls (sandbox, image, MCP, debug, context, archive,
// fork, share, save, delete) move into one menu: a dropdown from a ⋯ button
// on desktop, a bottom sheet opened from the chat title on phones. The
// Preact mounts are moved, not re-rendered, so their state and handlers stay
// intact; each new chat page brings fresh mounts, which are adopted again.

const ICON_MORE =
	'<svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><circle cx="5" cy="12" r="1.8"/><circle cx="12" cy="12" r="1.8"/><circle cx="19" cy="12" r="1.8"/></svg>';

const MENU_ITEM_IDS = [
	"sandboxToggle",
	"sandboxImageBtn",
	"mcpToggleBtn",
	"debugPanelBtn",
	"fullContextBtn",
	"sessionActionsMount",
] as const;

let chatMenu: HTMLElement | null = null;
let chatMenuBackdrop: HTMLElement | null = null;

function closeChatMenu(): void {
	chatMenu?.classList.remove("open");
	chatMenuBackdrop?.classList.remove("open");
}

function toggleChatMenu(anchor?: HTMLElement): void {
	if (!(chatMenu && chatMenuBackdrop)) return;
	if (chatMenu.classList.contains("open")) {
		closeChatMenu();
		return;
	}
	if (anchor && !isMobile()) {
		const r = anchor.getBoundingClientRect();
		chatMenu.style.top = `${r.bottom + 6}px`;
		chatMenu.style.right = `${Math.max(8, window.innerWidth - r.right)}px`;
	} else {
		chatMenu.style.top = "";
		chatMenu.style.right = "";
	}
	chatMenu.classList.add("open");
	chatMenuBackdrop.classList.add("open");
}

function buildChatMenu(): void {
	if (chatMenu) return;
	chatMenuBackdrop = el("div", { class: "chat-menu-backdrop" });
	chatMenuBackdrop.addEventListener("click", closeChatMenu);
	chatMenu = el("div", { class: "chat-menu", role: "menu", "aria-label": "Chat options" });
	// Any action inside the menu closes it (rename/delete dialogs open on top).
	chatMenu.addEventListener("click", (e) => {
		if ((e.target as HTMLElement).closest("button, a")) setTimeout(closeChatMenu, 0);
	});
	document.body.append(chatMenuBackdrop, chatMenu);
	document.addEventListener("keydown", (e) => {
		if (e.key === "Escape") closeChatMenu();
	});
	document.getElementById("mobileChatTitle")?.addEventListener("click", () => toggleChatMenu());
}

/** Move a freshly rendered chat page's toolbar controls into the menu. */
function adoptToolbarControls(): void {
	const toolbar = document.querySelector<HTMLElement>(".chat-toolbar");
	if (!(toolbar && chatMenu) || toolbar.dataset.uxAdopted === "1") return;
	toolbar.dataset.uxAdopted = "1";
	chatMenu.replaceChildren();
	for (const id of MENU_ITEM_IDS) {
		const node = document.getElementById(id);
		if (node && !chatMenu.contains(node)) chatMenu.appendChild(node);
	}
	const more = el(
		"button",
		{ class: "chat-menu-btn", type: "button", "aria-label": "Chat options", title: "Chat options" },
		ICON_MORE,
	);
	more.addEventListener("click", (e) => {
		e.stopPropagation();
		toggleChatMenu(more);
	});
	toolbar.appendChild(more);
}

/** Let the on-screen keyboard resize the layout instead of covering the composer. */
function keyboardResizesContent(): void {
	const meta = document.querySelector<HTMLMetaElement>('meta[name="viewport"]');
	if (meta && !meta.content.includes("interactive-widget")) {
		meta.content = `${meta.content}, interactive-widget=resizes-content`;
	}
}

export function initUxShell(): void {
	addStylesheet();
	keyboardResizesContent();
	document.documentElement.classList.add("ux-shell");
	buildHeader();
	buildDrawer();
	rewireMenuButton();
	buildChatMenu();
	syncTitle();
}

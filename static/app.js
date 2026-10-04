"use strict";

const $ = (id) => document.getElementById(id);

const el = {
    form: $("search"),
    url: $("url"),
    clearBtn: $("clearBtn"),
    pasteBtn: $("pasteBtn"),
    goBtn: $("goBtn"),
    error: $("error"),
    skeleton: $("skeleton"),
    card: $("card"),
    thumb: $("thumb"),
    duration: $("duration"),
    title: $("title"),
    uploader: $("uploader"),
    picker: $("picker"),
    tabs: [$("tabVideo"), $("tabAudio")],
    options: $("options"),
    moreBtn: $("moreBtn"),
    downloadBtn: $("downloadBtn"),
    job: $("job"),
    jobLabel: $("jobLabel"),
    jobPercent: $("jobPercent"),
    bar: $("bar"),
    barFill: $("barFill"),
    jobText: $("jobText"),
    saveBtn: $("saveBtn"),
    cancelBtn: $("cancelBtn"),
    backBtn: $("backBtn"),
    empty: $("empty"),
    version: $("version"),
    updateBtn: $("updateBtn"),
    toasts: $("toasts"),
};

const ACTIVE = ["starting", "downloading", "processing"];
const APP_TITLE = document.title;
const PREF_KEY = "ytdl-web:quality";

const state = {
    info: null, // the looked-up video, with its url
    kind: "video", // tab shown in the picker
    selected: null, // chosen `format` value
    showAll: false, // every format, not just one per resolution
    lookup: null, // AbortController while a lookup runs
    status: null, // latest server status
    watched: null, // job seen running in this tab, so its outcome is shown
    dismissed: null, // job whose outcome the user closed
    mine: null, // job started from this tab, saved automatically
    saved: new Set(),
    mediaKey: null,
};

// ---------- helpers

async function api(path, { method = "GET", body, signal } = {}) {
    const headers = { "X-Ytdl": "1" };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    const res = await fetch(path, {
        method,
        headers,
        signal,
        body: body === undefined ? undefined : JSON.stringify(body),
    });
    let data = null;
    if ((res.headers.get("content-type") || "").includes("application/json")) {
        data = await res.json().catch(() => null);
    }
    if (!res.ok) throw new Error(data?.error || `Request failed (${res.status})`);
    return data;
}

function bytes(n) {
    if (!n) return "0 B";
    const units = ["B", "KB", "MB", "GB", "TB"];
    const i = Math.min(Math.floor(Math.log(n) / Math.log(1024)), units.length - 1);
    const v = n / 1024 ** i;
    return `${v.toFixed(i === 0 || v >= 100 ? 0 : 1)} ${units[i]}`;
}

function clock(total) {
    total = Math.round(total);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = String(total % 60).padStart(2, "0");
    return h ? `${h}:${String(m).padStart(2, "0")}:${s}` : `${m}:${s}`;
}

function findLink(text) {
    return (text || "").match(/https?:\/\/[^\s<>"']+/i)?.[0] || null;
}

function make(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text != null) node.textContent = text;
    return node;
}

function toast(message, type = "info", ms = 5000) {
    const t = make("div", `toast ${type}${message.length > 60 ? " long" : ""}`, message);
    el.toasts.appendChild(t);
    requestAnimationFrame(() => t.classList.add("show"));
    setTimeout(() => {
        t.classList.remove("show");
        setTimeout(() => t.remove(), 300);
    }, ms);
}

function showError(message) {
    el.error.textContent = message || "";
    el.error.hidden = !message;
}

function loadPref() {
    try {
        return JSON.parse(localStorage.getItem(PREF_KEY));
    } catch {
        return null;
    }
}

function savePref(choice) {
    try {
        const kind = choice.kind === "audio" ? "audio" : "video";
        localStorage.setItem(PREF_KEY, JSON.stringify({ kind, quality: choice.quality, detail: choice.detail }));
    } catch {
        // Private mode or storage disabled: just don't remember.
    }
}

const tabOf = (choice) => (choice.kind === "audio" ? "audio" : "video");
const selectedChoice = () => state.info?.formats.find((f) => f.format === state.selected) || null;
const isActive = (s) => !!s && ACTIVE.includes(s.phase);

// ---------- looking up a link

async function lookup(raw) {
    const url = findLink(raw) || raw.trim();
    if (!url) return;
    if (!/^https?:\/\//i.test(url)) {
        showError("Paste a full link, starting with https://");
        return;
    }
    el.url.value = url;
    showError("");
    state.lookup?.abort();
    const controller = new AbortController();
    state.lookup = controller;
    render();
    try {
        const info = await api("api/info", { method: "POST", body: { url }, signal: controller.signal });
        state.info = { ...info, url };
        if (state.status && !isActive(state.status)) state.dismissed = state.status.job;
        chooseDefault();
        renderPicker();
        el.url.blur();
    } catch (err) {
        if (err.name !== "AbortError") {
            state.info = null;
            showError(err.message);
        }
    } finally {
        if (state.lookup === controller) state.lookup = null;
        render();
    }
}

// Start from what was picked last time (e.g. "1080p · MP4 · H.264").
function chooseDefault() {
    const formats = state.info.formats;
    const pref = loadPref();
    const hasAudio = formats.some((f) => tabOf(f) === "audio");
    // Audio-only sites (e.g. SoundCloud) have nothing but "Best" under Video.
    const hasVideo = formats.some((f) => f.kind === "video");
    state.kind = hasAudio && (pref?.kind === "audio" || !hasVideo) ? "audio" : "video";
    const pool = formats.filter((f) => tabOf(f) === state.kind);
    const match =
        pref &&
        (pool.find((f) => f.quality === pref.quality && f.detail === pref.detail) ||
            pool.find((f) => f.quality === pref.quality));
    state.selected = (match || pool[0])?.format ?? null;
    state.showAll = !!match && !match.primary;
}

function renderPicker() {
    const formats = state.info.formats;
    const hasAudio = formats.some((f) => tabOf(f) === "audio");
    el.tabs[1].disabled = !hasAudio;
    for (const tab of el.tabs) tab.setAttribute("aria-selected", String(tab.dataset.kind === state.kind));

    const pool = formats.filter((f) => tabOf(f) === state.kind);
    const hidden = pool.filter((f) => !f.primary).length;
    el.moreBtn.hidden = hidden === 0;
    el.moreBtn.textContent = state.showAll ? "Show fewer formats" : `Show all formats (${hidden} more)`;
    const rows = pool
        .filter((f) => state.showAll || f.primary)
        .map((f) => {
            const row = make("label", "option");
            const input = make("input");
            input.type = "radio";
            input.name = "quality";
            input.value = f.format;
            input.checked = f.format === state.selected;
            input.addEventListener("change", () => {
                state.selected = f.format;
                savePref(f);
                renderDownloadButton();
            });
            const main = make("span", "option-main");
            main.append(make("span", "quality", f.quality), make("span", "detail", f.detail));
            row.append(input, make("span", "radio"), main, make("span", "option-size", f.size ? `~${bytes(f.size)}` : ""));
            return row;
        });
    el.options.replaceChildren(...rows);
    renderDownloadButton();
}

function renderDownloadButton() {
    const choice = selectedChoice();
    el.downloadBtn.replaceChildren();
    if (!choice) {
        el.downloadBtn.textContent = "Download";
        return;
    }
    el.downloadBtn.append(`Download ${choice.kind === "best" ? "best quality" : choice.quality}`);
    if (choice.size) el.downloadBtn.append(make("span", "size", `· ~${bytes(choice.size)}`));
}

function switchTab(kind) {
    if (state.kind === kind || !state.info) return;
    state.kind = kind;
    const pool = state.info.formats.filter((f) => tabOf(f) === kind);
    if (!pool.some((f) => f.format === state.selected)) state.selected = pool[0]?.format ?? null;
    renderPicker();
}

// ---------- downloading

async function startDownload() {
    const choice = selectedChoice();
    if (!choice || isActive(state.status)) return;
    el.downloadBtn.disabled = true;
    try {
        const label = choice.kind === "best" ? "Best quality" : `${choice.quality} · ${choice.detail}`;
        const res = await api("api/download", {
            method: "POST",
            body: {
                url: state.info.url,
                format: choice.format,
                title: state.info.title,
                thumbnail: state.info.thumbnail,
                label,
            },
        });
        state.mine = res.job;
        state.watched = res.job;
    } catch (err) {
        toast(err.message, "error");
    } finally {
        render();
    }
}

async function cancelDownload() {
    el.cancelBtn.disabled = true;
    try {
        await api("api/cancel", { method: "POST" });
    } catch {
        // Already over; the event stream shows how it ended.
    }
}

// Save through a download link, so the browser streams the file to disk
// instead of holding it in memory, and the page (with its event stream)
// stays put. A failed download can't be detected from here, so check
// first that the file is still there.
async function saveFile(job) {
    try {
        const res = await fetch(`api/file/${job}`, { method: "HEAD" });
        if (!res.ok) throw new Error();
    } catch {
        toast("This download is no longer available.", "error");
        return;
    }
    const link = make("a");
    link.href = `api/file/${job}`;
    link.download = "";
    link.hidden = true;
    document.body.append(link);
    link.click();
    link.remove();
}

function closeOutcome() {
    const s = state.status;
    if (!s) return;
    state.dismissed = s.job;
    // After a success, start over; after a failure, keep the video to retry.
    if (s.phase === "delivered" || s.phase === "finished") {
        state.info = null;
        el.url.value = "";
        el.url.focus();
    }
    render();
}

// ---------- rendering

function render() {
    const s = state.status;
    const active = isActive(s);
    if (active) state.watched = s.job;
    const showJob =
        !!s &&
        s.job !== state.dismissed &&
        (active || s.phase === "finished" || (s.job === state.watched && ["delivered", "failed", "cancelled"].includes(s.phase)));
    const loading = !!state.lookup;

    el.url.disabled = active;
    el.goBtn.disabled = active || loading;
    el.goBtn.textContent = loading ? "Looking up…" : "Get video";
    el.clearBtn.hidden = active || !el.url.value;
    el.pasteBtn.hidden = active || !!el.url.value || !navigator.clipboard?.readText;

    el.skeleton.hidden = !loading;
    el.card.hidden = loading || !(showJob || state.info);
    el.empty.hidden = loading || showJob || !!state.info;

    if (!el.card.hidden) {
        if (showJob) {
            const sameVideo = state.info && state.info.title === s.title;
            setMedia(s.title, s.thumbnail, sameVideo ? state.info.uploader : "", sameVideo ? state.info.duration : null);
            renderJob(s);
        } else {
            const i = state.info;
            setMedia(i.title, i.thumbnail, i.uploader, i.duration);
            el.downloadBtn.disabled = active;
        }
        el.picker.hidden = showJob;
        el.job.hidden = !showJob;
    }
    document.title = tabTitle(s, active);
}

function setMedia(title, thumbnail, uploader, duration) {
    const key = JSON.stringify([title, thumbnail, uploader, duration]);
    if (key === state.mediaKey) return;
    state.mediaKey = key;
    el.title.textContent = title || "Untitled";
    el.uploader.textContent = uploader || "";
    el.uploader.hidden = !uploader;
    if (thumbnail) el.thumb.src = thumbnail;
    else el.thumb.removeAttribute("src");
    el.thumb.parentElement.hidden = !thumbnail;
    el.duration.textContent = duration != null ? clock(duration) : "";
    el.duration.hidden = duration == null;
}

function renderJob(s) {
    el.jobLabel.textContent = s.label;
    el.jobText.classList.remove("error");
    const wasIndeterminate = el.bar.classList.contains("indeterminate");
    el.bar.className = "bar";
    let width = 0;
    let percent = "";
    let text = "";
    const actions = { save: false, cancel: false, back: null };

    switch (s.phase) {
        case "starting":
            el.bar.classList.add("indeterminate");
            text = "Starting…";
            actions.cancel = true;
            break;
        case "downloading": {
            const stream = s.parts > 1 ? `${s.part === 1 ? "Video" : "Audio"} (${s.part} of ${s.parts}) · ` : "";
            if (s.total) {
                width = Math.min(100, (s.downloaded / s.total) * 100);
                percent = `${Math.floor(width)}%`;
                const speed = s.speed ? ` · ${bytes(s.speed)}/s` : "";
                const eta = s.eta != null ? ` · ${clock(s.eta)} left` : "";
                text = `${stream}${bytes(s.downloaded)} of ${bytes(s.total)}${speed}${eta}`;
            } else {
                el.bar.classList.add("indeterminate");
                text = `${stream}${bytes(s.downloaded)}`;
            }
            actions.cancel = true;
            break;
        }
        case "processing":
            el.bar.classList.add("indeterminate");
            text = s.parts > 1 ? "Merging video and audio…" : "Finishing…";
            actions.cancel = true;
            break;
        case "finished":
            el.bar.classList.add("done");
            width = 100;
            percent = "Ready";
            text = `${s.file} · ${bytes(s.size)}`;
            actions.save = true;
            actions.back = "Done";
            if (s.job === state.mine && !state.saved.has(s.job)) {
                state.saved.add(s.job);
                saveFile(s.job);
            }
            break;
        case "delivered":
            el.bar.classList.add("done");
            width = 100;
            percent = "Saved";
            text = `${s.file} · ${bytes(s.size)} — check your downloads.`;
            actions.back = "Download another";
            break;
        case "failed":
            el.bar.classList.add("failed");
            width = 100;
            percent = "Failed";
            text = s.error || "Unknown error";
            el.jobText.classList.add("error");
            actions.back = "Back";
            break;
        case "cancelled":
            percent = "Cancelled";
            text = "The download was stopped and its files removed.";
            actions.back = "Back";
            break;
    }

    // Jump straight to the real width instead of shrinking from the animation.
    const jump = wasIndeterminate && !el.bar.classList.contains("indeterminate");
    if (jump) el.barFill.style.transition = "none";
    el.barFill.style.width = `${width}%`;
    if (jump) {
        void el.barFill.offsetWidth;
        el.barFill.style.transition = "";
    }
    el.jobPercent.textContent = percent;
    el.jobText.textContent = text;
    el.saveBtn.hidden = !actions.save;
    el.saveBtn.href = `api/file/${s.job}`;
    el.cancelBtn.hidden = !actions.cancel;
    if (!actions.cancel) el.cancelBtn.disabled = false;
    el.backBtn.hidden = !actions.back;
    el.backBtn.textContent = actions.back || "";
}

function tabTitle(s, active) {
    if (active && s.phase === "downloading" && s.total) {
        return `${Math.floor((s.downloaded / s.total) * 100)}% · ${APP_TITLE}`;
    }
    if (active) return `Downloading… · ${APP_TITLE}`;
    if (s?.phase === "finished" && s.job !== state.dismissed) return `Ready · ${APP_TITLE}`;
    return APP_TITLE;
}

// ---------- footer

async function loadVersion() {
    try {
        const { version } = await api("api/version");
        el.version.textContent = `yt-dlp ${version}`;
    } catch {
        el.version.textContent = "";
    }
}

async function checkUpdate() {
    if (el.updateBtn.disabled) return;
    el.updateBtn.disabled = true;
    el.updateBtn.textContent = "Checking…";
    try {
        const res = await api("api/update", { method: "POST" });
        el.version.textContent = `yt-dlp ${res.version}`;
        toast(res.message);
    } catch (err) {
        toast(err.message, "error", 8000);
    } finally {
        el.updateBtn.disabled = false;
        el.updateBtn.textContent = "Check for update";
    }
}

// ---------- supported sites

const sites = {
    dialog: $("sites"),
    search: $("sitesSearch"),
    list: $("sitesList"),
    count: $("sitesCount"),
    empty: $("sitesEmpty"),
    groups: null, // [{ name, desc, more: [..], text }]
};

// `yt-dlp --extractor-descriptions` prints "name", "name: description" or
// "name: [login] description". Entries like "youtube:playlist" are folded
// into their site.
function parseSites(text) {
    const groups = new Map();
    for (const line of text.split("\n")) {
        if (!line.trim()) continue;
        const split = line.indexOf(": ");
        const name = (split < 0 ? line : line.slice(0, split)).trim();
        const desc = split < 0 ? "" : line.slice(split + 2).replace(/^\[[^\]]*\]\s*/, "").trim();
        const colon = name.indexOf(":");
        const base = colon > 0 ? name.slice(0, colon) : name;
        const key = base.toLowerCase();
        let g = groups.get(key);
        if (!g) {
            g = { name: base, desc: "", more: [], text: "" };
            groups.set(key, g);
        }
        if (colon > 0) g.more.push(name.slice(colon + 1));
        else if (desc) g.desc = desc;
        g.text += ` ${name} ${desc}`.toLowerCase();
    }
    return [...groups.values()].sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: "base" }));
}

function highlight(text, query) {
    const i = query ? text.toLowerCase().indexOf(query) : -1;
    if (i < 0) return [text];
    const mark = make("mark", null, text.slice(i, i + query.length));
    return [text.slice(0, i), mark, text.slice(i + query.length)];
}

function renderSites() {
    if (!sites.groups) return;
    const query = sites.search.value.trim().toLowerCase();
    const terms = query.split(/\s+/).filter(Boolean);
    const matches = sites.groups.filter((g) => terms.every((t) => g.text.includes(t)));
    // Sites whose name matches come first.
    if (query) matches.sort((a, b) => !a.name.toLowerCase().includes(query) - !b.name.toLowerCase().includes(query));
    const items = matches.map((g) => {
        const li = make("li");
        li.append(make("div", "site-name"));
        li.firstChild.append(...highlight(g.name, query));
        if (g.desc && g.desc.toLowerCase() !== g.name.toLowerCase()) li.append(make("div", "site-desc", g.desc));
        if (g.more.length) {
            const shown = g.more.slice(0, 6).join(", ");
            li.append(make("div", "site-more", `Also: ${shown}${g.more.length > 6 ? `, +${g.more.length - 6} more` : ""}`));
        }
        return li;
    });
    sites.list.replaceChildren(...items);
    sites.list.scrollTop = 0;
    sites.count.textContent = query
        ? `${matches.length} of ${sites.groups.length} sites`
        : `${sites.groups.length} sites`;
    sites.empty.hidden = matches.length > 0;
    sites.empty.textContent = `No site matches “${sites.search.value.trim()}”. yt-dlp may still handle the link with its generic extractor, so try it anyway.`;
}

async function openSites(e) {
    e.preventDefault();
    sites.search.value = "";
    sites.dialog.showModal();
    sites.search.focus();
    if (sites.groups) {
        renderSites();
        return;
    }
    sites.count.textContent = "Loading…";
    sites.list.replaceChildren();
    try {
        const res = await fetch("api/sites");
        if (!res.ok) throw new Error(`Request failed (${res.status})`);
        sites.groups = parseSites(await res.text());
        renderSites();
    } catch (err) {
        sites.count.textContent = "";
        sites.empty.hidden = false;
        sites.empty.textContent = `Couldn't load the list: ${err.message}`;
    }
}

for (const link of document.querySelectorAll(".sites-link")) link.addEventListener("click", openSites);
sites.search.addEventListener("input", renderSites);
$("sitesClose").addEventListener("click", () => sites.dialog.close());
// A click on the backdrop lands on the dialog element itself.
sites.dialog.addEventListener("click", (e) => {
    if (e.target === sites.dialog) sites.dialog.close();
});

// ---------- wiring

el.form.addEventListener("submit", (e) => {
    e.preventDefault();
    lookup(el.url.value);
});

// A pasted link replaces whatever was in the box and is looked up right away.
el.url.addEventListener("paste", (e) => {
    const link = findLink(e.clipboardData?.getData("text"));
    if (!link) return;
    e.preventDefault();
    lookup(link);
});

el.url.addEventListener("input", () => {
    showError("");
    render();
});

el.clearBtn.addEventListener("click", () => {
    el.url.value = "";
    showError("");
    state.lookup?.abort();
    if (!isActive(state.status)) {
        state.info = null;
        if (state.status) state.dismissed = state.status.job;
    }
    render();
    el.url.focus();
});

el.pasteBtn.addEventListener("click", async () => {
    try {
        const text = await navigator.clipboard.readText();
        const link = findLink(text);
        if (link) lookup(link);
        else toast("There's no link on the clipboard.");
    } catch {
        toast("Couldn't read the clipboard. Paste into the box instead.");
        el.url.focus();
    }
});

for (const tab of el.tabs) tab.addEventListener("click", () => switchTab(tab.dataset.kind));
el.moreBtn.addEventListener("click", () => {
    state.showAll = !state.showAll;
    if (!state.showAll && !selectedChoice()?.primary) {
        state.selected = state.info.formats.find((f) => tabOf(f) === state.kind && f.primary)?.format ?? null;
    }
    renderPicker();
});
el.downloadBtn.addEventListener("click", startDownload);
el.cancelBtn.addEventListener("click", cancelDownload);
el.backBtn.addEventListener("click", closeOutcome);
el.saveBtn.addEventListener("click", (e) => {
    e.preventDefault();
    if (state.status) saveFile(state.status.job);
});
el.updateBtn.addEventListener("click", checkUpdate);

// EventSource reconnects by itself after network errors; the first message
// after that brings the page up to date. If the browser closes it for good
// (e.g. an aborted navigation), open a new one.
function connectEvents() {
    const events = new EventSource("api/events");
    events.onmessage = (e) => {
        state.status = JSON.parse(e.data);
        render();
    };
    events.onerror = () => {
        if (events.readyState === EventSource.CLOSED) setTimeout(connectEvents, 2000);
    };
}
connectEvents();

// Links shared to the installed app (Android share sheet), or opened as
// ?url=… (e.g. from an iOS Shortcut).
const params = new URLSearchParams(location.search);
const shared = findLink(params.get("url")) || findLink(params.get("text"));
if (location.search) history.replaceState(null, "", location.pathname);

render();
loadVersion();
if (shared) lookup(shared);

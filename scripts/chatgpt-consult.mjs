// OB browser transport, invoked only by ob.py's project-bound MCP tool.
// The worker owns the SQLite registry and serializes browser access.
const fs = await import("node:fs/promises");
const args = globalThis.CONSULT_ARGS;
const { project_id: project, project_label: label, question, request_key: requestKey, url: known,
  journal, timeout_ms: timeout = 600000, collect = false } = args;
if (!/^[0-9A-HJKMNP-TV-Z]{26}$/.test(project) || !/^[a-f0-9]{32}$/.test(requestKey) || !journal || !question) {
  throw new Error("OB requires project_id, question and journal from its worker");
}
const task = await taskSpace("ChatGPT 決策顧問");
let progress;
try { progress = JSON.parse(await fs.readFile(journal, "utf8")); }
catch (e) { if (e.code !== "ENOENT") throw e; }
async function save(update) {
  progress = { ...progress, ...update, project_id: project };
  const tmp = journal + ".tmp";
  await fs.writeFile(tmp, JSON.stringify(progress), { mode: 0o600 });
  await fs.rename(tmp, journal);
}
if (progress?.project_id && progress.project_id !== project) throw new Error("wrong_project_journal");
if (progress?.phase === "done") {
  console.log("OB_RESULT " + JSON.stringify(progress));
} else {
  if (progress?.phase === "dispatching" && !collect) throw new Error("delivery_unknown: collect only, do not resend");
  if (collect && !progress?.url) throw new Error("delivery_unknown: inspect original tab manually; no known conversation URL");
  const url = progress?.url || known;
  if (url && !/^https:\/\/chatgpt\.com\/c\/[a-zA-Z0-9-]+$/.test(url)) throw new Error("invalid_conversation_url");
  const tabs = await task.tabs();
  const mine = url ? tabs.find(t => t.url.split("?")[0] === url) : null;
  let page;
  if (mine) page = mine.label ? task.page(mine.label) : await task.adopt(mine.page);
  else {
    // Do not adopt a generic ChatGPT home tab: another project/user might be composing there.
    page = await task.newPage();
    await page.goto(url || "https://chatgpt.com/");
    if (!url) await save({ phase: "opened", page: page.label, url: null });
  }
  await page.waitForSelector("#prompt-textarea", { state: "visible", timeout: 60000 });
  if (!collect) {
    const state = await page.evaluate(() => ({
      count: document.querySelectorAll('[data-message-author-role="assistant"]').length,
      busy: !!document.querySelector('[data-testid="stop-button"]'),
      draft: document.querySelector('#prompt-textarea')?.innerText?.trim() || ""
    }));
    if (state.busy || state.draft) throw new Error("conversation_busy_or_has_draft");
    const intro = known ? "" : `OB｜${label}｜${project}\n這是此 project ID 的固定諮詢對話。只使用此專案提供的脈絡；回答是建議，不是操作授權。\n\n`;
    await page.snapshot();
    await page.click("#prompt-textarea", { label: "focus OB project composer" });
    await save({ phase: "preparing", before: state.count, url: url || null, page: page.label });
    await page.keyboard.insertText(`[OB request=${requestKey}]\n` + intro + question);
    await page.waitForSelector('[data-testid="send-button"]', { state: "visible", timeout: 30000 });
    // Journal BEFORE sending. A crash from here onwards must never blindly resend.
    await save({ phase: "dispatching", before: state.count, url: url || null, page: page.label });
    await page.click('[data-testid="send-button"]', { label: "send OB project question" });
    await page.waitForFunction(() => location.pathname.startsWith("/c/"), undefined, { timeout: 60000 });
    await save({ phase: "sent", url: (await page.url()).split("?")[0] });
  }
  await page.waitForFunction(
    n => document.querySelectorAll('[data-message-author-role="assistant"]').length > n,
    progress.before, { timeout }
  );
  const boundAnswer = marker => {
    if (document.querySelector('[data-testid="stop-button"]')) return "";
    const all = [...document.querySelectorAll('[data-message-author-role]')];
    const index = all.findIndex(n => n.getAttribute('data-message-author-role') === 'user' && n.innerText.includes(marker));
    if (index < 0) return "";
    // Pair with this request, never an unrelated later answer after a manual message.
    for (const node of all.slice(index + 1)) {
      if (node.getAttribute('data-message-author-role') === 'user') break;
      if (node.getAttribute('data-message-author-role') === 'assistant') {
        const turn = node.closest('[data-turn="assistant"]');
        if (!turn?.querySelector('[data-testid="copy-turn-action-button"]')) return "";
        return node.innerText.trim();
      }
    }
    return "";
  };
  await page.waitForFunction(boundAnswer, `[OB request=${requestKey}]`, { timeout });
  const answer = await page.evaluate(boundAnswer, `[OB request=${requestKey}]`);
  if (!answer) throw new Error("empty_answer");
  await save({ phase: "done", answer, url: (await page.url()).split("?")[0] });
  console.log("OB_RESULT " + JSON.stringify(progress));
}
// Keep this shared space and all project tabs for subsequent OB requests.

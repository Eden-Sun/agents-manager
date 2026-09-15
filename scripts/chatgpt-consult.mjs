// 問 ChatGPT（ego lite 裡的「ChatGPT 決策顧問」task space），每個專案一個固定對話、一個分頁，不重複開。
// 用法見 docs/CHATGPT-CONSULT.md。直接跑請走 scripts/chatgpt-consult.sh；本檔由 `ego-browser nodejs` 執行。
// ego 的 nodejs 不繼承呼叫端的環境變數，所以 .sh 會在檔頭插一行 `globalThis.CONSULT_ARGS = {...}` 帶參數：
//   CONSULT_PROJECT        專案名（AG Man 的 project label；同一個名字永遠回到同一個對話）
//   CONSULT_QUESTION_FILE  問題全文所在的檔案（UTF-8）
//   CONSULT_TIMEOUT_MS     等回答的上限，預設 600000（10 分鐘）
//   CONSULT_NEW=1          丟掉這個專案記住的對話，重開一個（對話壞掉或脈絡太亂時才用）
const fs = await import("node:fs/promises");
const path = await import("node:path");
const os = await import("node:os");

const SPACE = "ChatGPT 決策顧問";
const DATA = path.join(os.homedir(), ".config/agents-manager");
const REGISTRY = path.join(DATA, "chatgpt-consult.json");

const args = globalThis.CONSULT_ARGS ?? {};
const project = String(args.CONSULT_PROJECT || "").trim();
const questionFile = String(args.CONSULT_QUESTION_FILE || "");
const timeout = Number(args.CONSULT_TIMEOUT_MS || 600_000);
if (!project || !questionFile) {
  console.error("chatgpt-consult: 需要 CONSULT_PROJECT 與 CONSULT_QUESTION_FILE（請用 scripts/chatgpt-consult.sh）");
  process.exit(2);
}
const question = (await fs.readFile(questionFile, "utf8")).trim();
if (!question) {
  console.error("chatgpt-consult: 問題是空的");
  process.exit(2);
}

async function readRegistry() {
  try {
    return JSON.parse(await fs.readFile(REGISTRY, "utf8"));
  } catch {
    return {};
  }
}
async function writeRegistry(reg) {
  const tmp = `${REGISTRY}.tmp-${process.pid}`;
  await fs.writeFile(tmp, JSON.stringify(reg, null, 2) + "\n");
  await fs.rename(tmp, REGISTRY);
}

// 同一個專案一次只問一題：兩個 agent 同時打進同一個對話，回答會對不上題。
const lockDir = path.join(DATA, `chatgpt-consult.${Buffer.from(project).toString("hex").slice(0, 40)}.lock`);
const lockDeadline = Date.now() + timeout;
for (;;) {
  try {
    await fs.mkdir(lockDir);
    break;
  } catch {
    const st = await fs.stat(lockDir).catch(() => null);
    // 超過上限還在的鎖是上一個問到一半被殺掉留下的。
    if (st && Date.now() - st.mtimeMs > timeout + 60_000) {
      await fs.rm(lockDir, { recursive: true, force: true });
      continue;
    }
    if (Date.now() > lockDeadline) {
      console.error(`chatgpt-consult: ${project} 的對話一直有人在問，等了 ${timeout}ms 還沒輪到`);
      process.exit(3);
    }
    await new Promise((r) => setTimeout(r, 2000));
  }
}
const unlock = () => fs.rm(lockDir, { recursive: true, force: true });

try {
  const task = await taskSpace(SPACE);
  const reg = await readRegistry();
  if (String(args.CONSULT_NEW) === "1") delete reg[project];
  const known = reg[project]?.url || null;

  // 找這個專案已經開著的分頁；沒有就開一個（有記住的對話就回到那個對話，不另開新對話）。
  const tabs = await task.tabs();
  const convId = known ? known.split("/c/")[1] : null;
  let page = null;
  const mine = convId ? tabs.find((t) => t.url.includes(`/c/${convId}`)) : null;
  if (mine) {
    page = mine.label ? task.page(mine.label) : await task.adopt(mine.page);
  } else {
    // 空白頁、或還沒送出任何訊息的 ChatGPT 首頁可以直接拿來用，不必多開一個分頁。
    const blank = tabs.find((t) => t.label && /^(chrome:\/\/newtab|about:blank|https:\/\/chatgpt\.com\/?$)/.test(t.url));
    page = blank ? task.page(blank.label) : await task.newPage();
    await page.goto(known || "https://chatgpt.com/");
  }
  await page.waitForSelector("#prompt-textarea", { state: "visible", timeout: 60_000 });

  const before = await page.evaluate(() => document.querySelectorAll('[data-message-author-role="assistant"]').length);
  const intro = known
    ? ""
    : `（這是「${project}」專案的固定諮詢對話：agents-manager 裡的 agent 會把這個專案的決策問題都問在這裡，請沿用前面的脈絡。）\n\n`;
  await page.click("#prompt-textarea", { label: "focus ChatGPT composer" });
  await page.keyboard.insertText(intro + question);
  await page.waitForSelector('[data-testid="send-button"]', { state: "visible", timeout: 30_000 });
  await page.click('[data-testid="send-button"]', { label: "send question" });

  // 回答結束＝新的 assistant 訊息出現、而且停止鈕消失。
  await page.waitForFunction(
    (n) => document.querySelectorAll('[data-message-author-role="assistant"]').length > n,
    before,
    { timeout },
  );
  await page.waitForFunction(() => !document.querySelector('[data-testid="stop-button"]'), undefined, { timeout });
  await page.waitForTimeout(1500);
  const answer = await page.evaluate(() => {
    const all = document.querySelectorAll('[data-message-author-role="assistant"]');
    return all.length ? all[all.length - 1].innerText.trim() : "";
  });

  const url = await page.url();
  if (url.includes("/c/")) {
    reg[project] = { url: url.split("?")[0], label: page.label, updated_at: new Date().toISOString() };
    await writeRegistry(reg);
  }
  console.log(`[chatgpt-consult] project=${project} conversation=${reg[project]?.url ?? url} tab=${page.label} space=${task.spaceId}`);
  console.log(answer || "(沒有讀到回答，請到 ego 的「ChatGPT 決策顧問」分頁看)");
} finally {
  await unlock();
}
// 不呼叫 task.finish()：這個 space 與分頁要留著給下一次（見 docs/CHATGPT-CONSULT.md「不要關」）。

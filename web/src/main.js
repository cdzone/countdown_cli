// Display only: sends the conversation to POST /agui with @ag-ui/client and
// shows each AG-UI event as it arrives. All decisions (tools, 「查不到」,
// thinking removal) are made by the agent, never here.
import { HttpAgent } from "@ag-ui/client";

const log = document.getElementById("log");
const form = document.getElementById("ask");
const input = document.getElementById("question");
const agent = new HttpAgent({ url: "/agui" });
const threadId = crypto.randomUUID();
const messages = [];

function card(className, text) {
  const div = document.createElement("div");
  div.className = className;
  div.textContent = text;
  log.appendChild(div);
  return div;
}

function show(event, ui) {
  switch (event.type) {
    case "TEXT_MESSAGE_START":
      ui.text = card("assistant", "");
      break;
    case "TEXT_MESSAGE_CONTENT":
      ui.text.textContent += event.delta;
      messages.push({ id: event.messageId, role: "assistant", content: ui.text.textContent });
      break;
    case "TOOL_CALL_START":
      ui.tools[event.toolCallId] = card("tool", `工具调用：${event.toolCallName}`);
      break;
    case "TOOL_CALL_ARGS":
      ui.tools[event.toolCallId].textContent += `\n参数：${event.delta}`;
      break;
    case "TOOL_CALL_RESULT": {
      const box = ui.tools[event.toolCallId] ?? card("tool", "工具调用");
      let result;
      try {
        result = JSON.parse(event.content);
      } catch {
        result = { ok: true, result: event.content };
      }
      box.textContent += result.ok
        ? `\n结果：${result.result}`
        : `\n查不到：${result.reason}`;
      if (!result.ok) box.classList.add("failed");
      if (result.request_id) box.textContent += `\nrequest_id：${result.request_id}`;
      break;
    }
    case "RUN_ERROR":
      card("error", `出错：${event.message}`);
      break;
  }
}

form.addEventListener("submit", (e) => {
  e.preventDefault();
  const question = input.value.trim();
  if (!question) return;
  input.value = "";
  card("user", question);
  messages.push({ id: crypto.randomUUID(), role: "user", content: question });
  const ui = { text: null, tools: {} };
  form.querySelector("button").disabled = true;
  const done = () => (form.querySelector("button").disabled = false);
  agent
    .run({
      threadId,
      runId: crypto.randomUUID(),
      messages: [...messages],
      tools: [],
      context: [],
      state: {},
      forwardedProps: {},
    })
    .subscribe({
      next: (event) => show(event, ui),
      error: (err) => {
        card("error", `连接出错：${err.message ?? err}`);
        done();
      },
      complete: done,
    });
});

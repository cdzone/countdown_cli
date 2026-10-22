// Generates tests/fixtures/agui-events.json: one sample of each of the ten
// AG-UI events the agent sends, validated against @ag-ui/core 1.0.3 schemas.
// Run once after changing an event: `cd web && npm ci && npm run gen-fixtures`.
// The Rust test only reads the JSON file, so `cargo test` needs no node.
import { readFileSync, writeFileSync } from "node:fs";
import * as schemas from "@ag-ui/core/schemas";

const coreVersion = JSON.parse(
  readFileSync(new URL("../node_modules/@ag-ui/core/package.json", import.meta.url)),
).version;

// Same values as the Rust test `events_match_the_ag_ui_core_samples`.
const samples = [
  ["RunStartedEventSchema", { type: "RUN_STARTED", threadId: "t1", runId: "r1" }],
  ["RunFinishedEventSchema", { type: "RUN_FINISHED", threadId: "t1", runId: "r1" }],
  ["RunErrorEventSchema", { type: "RUN_ERROR", message: "模型网关出错", code: "model_error" }],
  ["TextMessageStartEventSchema", { type: "TEXT_MESSAGE_START", messageId: "m1", role: "assistant" }],
  ["TextMessageContentEventSchema", { type: "TEXT_MESSAGE_CONTENT", messageId: "m1", delta: "答案" }],
  ["TextMessageEndEventSchema", { type: "TEXT_MESSAGE_END", messageId: "m1" }],
  ["ToolCallStartEventSchema", { type: "TOOL_CALL_START", toolCallId: "c1", toolCallName: "list_countdowns" }],
  ["ToolCallArgsEventSchema", { type: "TOOL_CALL_ARGS", toolCallId: "c1", delta: "{}" }],
  ["ToolCallEndEventSchema", { type: "TOOL_CALL_END", toolCallId: "c1" }],
  [
    "ToolCallResultEventSchema",
    { type: "TOOL_CALL_RESULT", messageId: "m2", toolCallId: "c1", content: '{"ok":true}', role: "tool" },
  ],
];

const events = samples.map(([name, sample]) => {
  const schema = schemas[name];
  const parsed = schema.parse(sample);
  // The schemas keep unknown keys, so check names against the schema shape:
  // a misspelt field must fail here, not pass through.
  const known = Object.keys(schema.shape);
  for (const key of Object.keys(sample)) {
    if (!known.includes(key)) throw new Error(`${name}: unknown field ${key}`);
  }
  return parsed;
});

const out = {
  source: `@ag-ui/core@${coreVersion} (schemas)`,
  generated_by: "cd web && npm ci && npm run gen-fixtures",
  events,
};
writeFileSync(
  new URL("../../tests/fixtures/agui-events.json", import.meta.url),
  JSON.stringify(out, null, 2) + "\n",
);
console.log(`wrote ${events.length} events from ${out.source}`);

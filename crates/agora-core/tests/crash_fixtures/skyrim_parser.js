// A small, real parser for the synthetic log beside it. It uses only the
// scoped reads the crash contract allows, and produces the worked Skyrim
// example's output.
import { crash } from "agora";

function number(text) {
  return Number(text);
}

export async function parseCrashLogger(request) {
  const reply = crash.readLines(request.artifact.id, 0, request.limits.max_lines);
  const lines = reply.lines;
  const facts = [];
  const bucket = [];

  if (!lines.some((line) => line.startsWith("Unhandled exception "))) {
    return {
      contract: 1,
      producer: { format: "crash_logger" },
      facts: [],
      notes: [{ kind: "wrong_format" }],
    };
  }
  const exceptionLine = lines.findIndex((line) => line.startsWith("Exception: "));

  const logger = lines[0].match(/\(Crash Logger ([0-9.]+)\)/);
  const pid = lines.map((line) => line.match(/^Process ID: (\d+)$/)).find(Boolean);
  const image = lines.map((line) => line.match(/^Image: (\S+) (\S+)$/)).find(Boolean);
  const exception = lines
    .map((line) =>
      line.match(
        /^Exception: (\S+) \((\w+), (\w+), (\w+)\) at (\S+)$/,
      ),
    )
    .find(Boolean);

  if (pid) {
    facts.push({
      kind: "core.process",
      attrs: {
        pid: number(pid[1]),
        image: image ? image[1] : "",
        image_version: image ? image[2] : "",
      },
    });
  }

  if (exception) {
    facts.push({
      kind: "core.exception",
      attrs: {
        class: exception[1],
        category: exception[2],
        access: exception[3],
        pointer_class: exception[4],
        fault_address: exception[5],
      },
      span: { section: "exception", line_start: exceptionLine + 1 },
    });
  }

  for (const line of lines) {
    const frame = line.match(/^Frame (\d+): 0x([0-9A-Fa-f]+) (\S+)/);
    if (!frame) continue;
    const index = number(frame[1]);
    const attrs = {
      index,
      kind: "probable",
      rva: "0x" + frame[2],
    };
    const refs = [{ kind: "module", value: frame[3], strength: "exec" }];
    const detailed = line.match(/^Frame \d+: 0x[0-9A-Fa-f]+ \S+ addrlib-ae (\d+) \+(\d+) (exec_top|exec)$/);
    if (detailed) {
      attrs.symbol_scheme = "addrlib-ae";
      attrs.symbol = detailed[1];
      attrs.symbol_offset = number(detailed[2]);
      refs[0].strength = detailed[3];
      bucket.push({
        scheme: "addrlib-ae",
        value: `${detailed[1]}+0x${number(detailed[2]).toString(16)}`,
      });
    }
    facts.push({ kind: "core.frame", attrs, refs });
  }

  const module = lines.map((line) => line.match(/^Module: (\S+) version (\S+) base (\S+)$/)).find(Boolean);
  if (module) {
    facts.push({
      kind: "core.module",
      attrs: { version: module[2], base_address: module[3] },
      refs: [{ kind: "module", value: module[1], strength: "mention" }],
    });
  }

  const object = lines
    .map((line) => line.match(/^Object: (\S+) (\S+) at (\S+) -> (\S+)$/))
    .find(Boolean);
  if (object) {
    facts.push({
      kind: "creation.object",
      attrs: { type: object[1], name: object[2], location: object[3] },
      refs: [{ kind: "file", value: object[4], strength: "object" }],
    });
  }

  return {
    contract: 1,
    producer: { format: "crash_logger", version: logger ? logger[1] : "1.2" },
    facts,
    bucket,
    coverage: [
      { kind: "core.log_line", state: "not_applicable" },
      {
        kind: "creation.model_request",
        state: "absent",
        why: "This logger version has no model section.",
      },
    ],
  };
}

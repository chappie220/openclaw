/** Pure OpenRC init-script and conf.d rendering/parsing for the managed Gateway service. */
import { GATEWAY_SERVICE_STOP_TIMEOUT_MS } from "../infra/gateway-shutdown-budget.js";
import type {
  GatewayServiceCommandSnapshot,
  GatewayServiceEnvironmentValueSource,
  GatewayServiceRenderArgs,
} from "./service-types.js";

export const OPENRC_MANAGED_MARKER = "# Managed by OpenClaw (openclaw gateway install).";

// openrc-run passes command, directory, and command_args through `eval`; plain
// tokens keep the first two unambiguous, and command_args carries quoted words.
const OPENRC_PLAIN_TOKEN = /^[A-Za-z0-9_@%+=:,./-]+$/;
const SHELL_IDENTIFIER = /^[A-Za-z_][A-Za-z0-9_]*$/;
const LINE_BREAKS = /[\r\n]/;

export function shellQuote(value: string): string {
  return `'${value.replace(/'/g, `'\\''`)}'`;
}

function assertPlainToken(value: string, label: string): void {
  if (!OPENRC_PLAIN_TOKEN.test(value)) {
    throw new Error(
      `OpenRC ${label} ${JSON.stringify(value)} must not contain spaces, quotes, or shell metacharacters; install OpenClaw and Node under a plain path.`,
    );
  }
}

function assertNoLineBreaks(value: string, label: string): void {
  if (LINE_BREAKS.test(value)) {
    throw new Error(`OpenRC ${label} cannot contain CR or LF characters.`);
  }
}

function renderExports(environment: Record<string, string>): string[] {
  return Object.entries(environment).map(([key, value]) => {
    if (!SHELL_IDENTIFIER.test(key)) {
      throw new Error(`OpenRC environment key ${JSON.stringify(key)} is not a shell identifier.`);
    }
    assertNoLineBreaks(value, `environment value for ${key}`);
    return `export ${key}=${shellQuote(value)}`;
  });
}

/** Split planned environment by source: file-sourced values may be secrets and stay in conf.d. */
export function splitOpenRcEnvironment(
  environment: Record<string, string | undefined> | undefined,
  sources: Record<string, GatewayServiceEnvironmentValueSource | undefined> | undefined,
): { inline: Record<string, string>; file: Record<string, string> } {
  const inline: Record<string, string> = {};
  const file: Record<string, string> = {};
  for (const [key, value] of Object.entries(environment ?? {})) {
    if (value === undefined) {
      continue;
    }
    (sources?.[key] && sources[key] !== "inline" ? file : inline)[key] = value;
  }
  return { inline, file };
}

export function renderOpenRcInitScript(params: {
  serviceName: string;
  render: GatewayServiceRenderArgs;
  inlineEnvironment: Record<string, string>;
}): string {
  const [command, ...args] = params.render.programArguments;
  if (!command) {
    throw new Error("OpenRC service requires a program to run.");
  }
  assertPlainToken(command, "command");
  if (params.render.workingDirectory) {
    assertPlainToken(params.render.workingDirectory, "working directory");
  }
  for (const arg of args) {
    assertNoLineBreaks(arg, "command arguments");
  }
  const description = params.render.description ?? "OpenClaw Gateway";
  assertNoLineBreaks(description, "description");
  const stopSeconds = Math.ceil(GATEWAY_SERVICE_STOP_TIMEOUT_MS / 1_000);
  return [
    "#!/sbin/openrc-run",
    OPENRC_MANAGED_MARKER,
    `# Rewritten by \`openclaw gateway install --force\`; owner-only values live in /etc/conf.d/${params.serviceName}.`,
    "",
    `description=${shellQuote(description)}`,
    "supervisor=supervise-daemon",
    `command=${command}`,
    `command_args=${shellQuote(args.map(shellQuote).join(" "))}`,
    ...(params.render.workingDirectory ? [`directory=${params.render.workingDirectory}`] : []),
    `output_log=/var/log/${params.serviceName}.log`,
    `error_log=/var/log/${params.serviceName}.log`,
    // Ten restarts in five minutes cover lifecycle ownership waits without crash loops.
    "respawn_delay=5",
    "respawn_max=10",
    "respawn_period=300",
    // Include the Gateway drain and teardown reserve before escalating.
    `retry="TERM/${stopSeconds}/KILL/5"`,
    ...renderExports(params.inlineEnvironment),
    "",
    "depend() {",
    "\tneed net",
    "\tuse dns logger",
    "\tafter firewall",
    "}",
    "",
  ].join("\n");
}

export function renderOpenRcConfFile(fileEnvironment: Record<string, string>): string {
  return [
    OPENRC_MANAGED_MARKER,
    "# Owner-only service environment; values may be secrets.",
    ...renderExports(fileEnvironment),
    "",
  ].join("\n");
}

/** POSIX shell word splitting for the quoting forms this module renders. */
export function parseShellWords(value: string): string[] {
  const words: string[] = [];
  let current = "";
  let inWord = false;
  for (let i = 0; i < value.length; i++) {
    const char = value.charAt(i);
    if (char === "'") {
      const end = value.indexOf("'", i + 1);
      if (end < 0) {
        throw new Error("Unterminated single quote in OpenRC service definition.");
      }
      current += value.slice(i + 1, end);
      inWord = true;
      i = end;
      continue;
    }
    if (char === "\\" && i + 1 < value.length) {
      current += value.charAt(i + 1);
      inWord = true;
      i++;
      continue;
    }
    if (/\s/.test(char)) {
      if (inWord) {
        words.push(current);
        current = "";
        inWord = false;
      }
      continue;
    }
    current += char;
    inWord = true;
  }
  if (inWord) {
    words.push(current);
  }
  return words;
}

function readAssignments(content: string): {
  values: Map<string, string>;
  exports: Record<string, string>;
} {
  const values = new Map<string, string>();
  const exports: Record<string, string> = {};
  for (const raw of content.split("\n")) {
    const line = raw.trim();
    const exported = line.startsWith("export ");
    const assignment = exported ? line.slice("export ".length) : line;
    const separator = assignment.indexOf("=");
    if (separator <= 0) {
      continue;
    }
    const key = assignment.slice(0, separator);
    if (!SHELL_IDENTIFIER.test(key)) {
      continue;
    }
    const value = parseShellWords(assignment.slice(separator + 1)).join(" ");
    if (exported) {
      exports[key] = value;
    } else {
      values.set(key, value);
    }
  }
  return { values, exports };
}

/** Parse a managed init script; foreign scripts are not OpenClaw definitions. */
export function parseOpenRcServiceDefinition(params: {
  initScript: string;
  confFile: string | null;
}): GatewayServiceCommandSnapshot | null {
  if (!params.initScript.includes(OPENRC_MANAGED_MARKER)) {
    return null;
  }
  const script = readAssignments(params.initScript);
  const command = script.values.get("command");
  if (!command) {
    return null;
  }
  const conf = params.confFile ? readAssignments(params.confFile).exports : {};
  const environment = { ...script.exports, ...conf };
  const environmentValueSources: Record<string, GatewayServiceEnvironmentValueSource> = {};
  for (const key of Object.keys(script.exports)) {
    environmentValueSources[key] = "inline";
  }
  for (const key of Object.keys(conf)) {
    environmentValueSources[key] = "file";
  }
  const workingDirectory = script.values.get("directory");
  return {
    programArguments: [command, ...parseShellWords(script.values.get("command_args") ?? "")],
    ...(workingDirectory ? { workingDirectory } : {}),
    ...(Object.keys(environment).length ? { environment, environmentValueSources } : {}),
  };
}

/** Map `rc-service <name> status` output to the Gateway runtime vocabulary. */
export function parseOpenRcStatus(output: string): {
  status: "running" | "stopped" | "unknown";
  state?: string;
} {
  const match = /status:\s*([a-z]+)/i.exec(output);
  const state = match?.[1]?.toLowerCase();
  if (state === "started") {
    return { status: "running", state };
  }
  if (state === "stopped" || state === "crashed" || state === "inactive") {
    return { status: "stopped", state };
  }
  return { status: "unknown", ...(state ? { state } : {}) };
}

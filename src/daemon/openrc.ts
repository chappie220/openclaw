/** OpenRC (Alpine Linux) system-service adapter for the managed Gateway. */
import { existsSync } from "node:fs";
import fs from "node:fs/promises";
import path from "node:path";
import { parseStrictPositiveInteger } from "@openclaw/normalization-core/number-coercion";
import { hasErrnoCode } from "../infra/errno.js";
import { execFileUtf8, type ExecResult } from "./exec-file.js";
import {
  parseOpenRcServiceDefinition,
  parseOpenRcStatus,
  renderOpenRcConfFile,
  renderOpenRcInitScript,
  splitOpenRcEnvironment,
} from "./openrc-script.js";
import { formatLine, writeFormattedLines } from "./output.js";
import { createGatewayLifecycleMutationReporter } from "./service-mutation.js";
import type { GatewayServiceRuntime } from "./service-runtime.js";
import { publishServiceFile } from "./service-stage.js";
import type {
  GatewayServiceCommandConfig,
  GatewayServiceControlArgs,
  GatewayServiceEnv,
  GatewayServiceEnvArgs,
  GatewayServiceInstallArgs,
  GatewayServiceManageArgs,
  GatewayServiceReadOptions,
  GatewayServiceRestartResult,
} from "./service-types.js";
import { resolveSystemdServiceName } from "./systemd-service-files.js";

const OPENRC_RUN_PATHS = ["/sbin/openrc-run", "/usr/sbin/openrc-run"];
const OPENRC_INIT_DIR = "/etc/init.d";
const OPENRC_CONF_DIR = "/etc/conf.d";
const OPENRC_RUNLEVELS_DIR = "/etc/runlevels";
const OPENRC_SERVICE_STATE_DIR = "/run/openrc";
const OPENRC_DEFAULT_RUNLEVEL = "default";
const OPENRC_PROBE_TIMEOUT_MS = 10_000;

let openRcHost: boolean | undefined;

/** Process-stable: a booted systemd always wins; otherwise an installed openrc-run selects OpenRC. */
export function isOpenRcServiceHost(): boolean {
  if (process.platform !== "linux") {
    return false;
  }
  openRcHost ??=
    !existsSync("/run/systemd/system") && OPENRC_RUN_PATHS.some((file) => existsSync(file));
  return openRcHost;
}

/** OpenRC shares the Linux service name (profile suffix, OPENCLAW_SYSTEMD_UNIT override). */
export function resolveOpenRcServiceName(env: GatewayServiceEnv): string {
  return resolveSystemdServiceName(env);
}

function resolveOpenRcPaths(env: GatewayServiceEnv) {
  const name = resolveOpenRcServiceName(env);
  return {
    name,
    initScript: path.posix.join(OPENRC_INIT_DIR, name),
    confFile: path.posix.join(OPENRC_CONF_DIR, name),
  };
}

function rootCommand(command: string): string {
  return process.geteuid?.() === 0 ? command : `sudo ${command}`;
}

function assertOpenRcRoot(action: string, env: GatewayServiceEnv): void {
  if (process.geteuid?.() === 0) {
    return;
  }
  const { name } = resolveOpenRcPaths(env);
  throw new Error(
    `OpenRC services are system-wide; ${action} needs root. Rerun from a root shell (\`su -\` or \`doas -s\`), or use \`sudo rc-service ${name} <start|stop|restart>\`.`,
  );
}

async function readOptionalFile(file: string): Promise<string | null> {
  try {
    return await fs.readFile(file, "utf8");
  } catch (error) {
    if (hasErrnoCode(error, "ENOENT")) {
      return null;
    }
    throw error;
  }
}

async function execOpenRc(
  command: "rc-service" | "rc-update",
  args: string[],
  timeoutMs?: number,
): Promise<ExecResult> {
  return await execFileUtf8(
    command,
    args,
    timeoutMs && timeoutMs > 0 ? { timeout: timeoutMs, killSignal: "SIGKILL" } : {},
  );
}

function detail(result: ExecResult): string {
  return (result.stderr || result.stdout).trim() || `exit ${result.code}`;
}

async function runOpenRcAction(command: "rc-service" | "rc-update", args: string[]) {
  const result = await execOpenRc(command, args);
  if (result.code !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed: ${detail(result)}`);
  }
}

async function listEnabledRunlevels(name: string): Promise<string[]> {
  let runlevels: string[];
  try {
    runlevels = await fs.readdir(OPENRC_RUNLEVELS_DIR);
  } catch (error) {
    if (hasErrnoCode(error, "ENOENT")) {
      return [];
    }
    throw error;
  }
  const enabled: string[] = [];
  for (const runlevel of runlevels) {
    const link = await fs
      .lstat(path.posix.join(OPENRC_RUNLEVELS_DIR, runlevel, name))
      .catch((error: unknown) => {
        if (hasErrnoCode(error, "ENOENT") || hasErrnoCode(error, "ENOTDIR")) {
          return null;
        }
        throw error;
      });
    if (link) {
      enabled.push(runlevel);
    }
  }
  return enabled;
}

export async function isOpenRcServiceEnabled(args: GatewayServiceEnvArgs): Promise<boolean> {
  const { name, initScript } = resolveOpenRcPaths(args.env ?? process.env);
  if ((await readOptionalFile(initScript)) === null) {
    return false;
  }
  return (await listEnabledRunlevels(name)).length > 0;
}

export async function hasOpenRcServiceDefinition(args: GatewayServiceEnvArgs): Promise<boolean> {
  return (await readOptionalFile(resolveOpenRcPaths(args.env ?? process.env).initScript)) !== null;
}

export async function readOpenRcServiceCommand(
  env: GatewayServiceEnv,
): Promise<GatewayServiceCommandConfig | null> {
  const { initScript, confFile } = resolveOpenRcPaths(env);
  const script = await readOptionalFile(initScript);
  if (script === null) {
    return null;
  }
  const conf = await readOptionalFile(confFile);
  const command = parseOpenRcServiceDefinition({ initScript: script, confFile: conf });
  if (!command) {
    throw new Error(
      `${initScript} is not managed by OpenClaw; remove or rename it before installing the Gateway service.`,
    );
  }
  return {
    ...command,
    sourcePath: initScript,
    definitionPaths: conf === null ? [initScript] : [initScript, confFile],
  };
}

async function readOpenRcChildPid(name: string): Promise<number | undefined> {
  // supervise-daemon records the supervised child under the service's option store.
  const value = await readOptionalFile(
    path.posix.join(OPENRC_SERVICE_STATE_DIR, "options", name, "child_pid"),
  ).catch(() => null);
  return parseStrictPositiveInteger(value?.trim());
}

export async function readOpenRcServiceRuntime(
  env: GatewayServiceEnv = process.env as GatewayServiceEnv,
  opts?: GatewayServiceReadOptions,
): Promise<GatewayServiceRuntime> {
  const { name, initScript } = resolveOpenRcPaths(env);
  if ((await readOptionalFile(initScript)) === null) {
    return { status: "stopped", missingUnit: true, openrc: { service: name, initScript } };
  }
  const result = await execOpenRc(
    "rc-service",
    [name, "status"],
    opts?.timeoutMs ?? OPENRC_PROBE_TIMEOUT_MS,
  );
  const parsed = parseOpenRcStatus(`${result.stdout}\n${result.stderr}`);
  if (result.termination !== "exit" || (parsed.status === "unknown" && !parsed.state)) {
    return {
      status: "unknown",
      detail: `rc-service ${name} status failed: ${detail(result)}`,
      openrc: { service: name, initScript },
    };
  }
  const pid = parsed.status === "running" ? await readOpenRcChildPid(name) : undefined;
  return {
    status: parsed.status,
    ...(parsed.state ? { state: parsed.state } : {}),
    ...(pid ? { pid } : {}),
    ...(parsed.state === "crashed"
      ? { detail: `OpenRC reports ${name} crashed; check /var/log/${name}.log.` }
      : {}),
    openrc: { service: name, initScript },
  };
}

async function writeOpenRcDefinition(args: GatewayServiceInstallArgs) {
  assertOpenRcRoot("installing the Gateway service", args.env);
  const { name, initScript, confFile } = resolveOpenRcPaths(args.env);
  const existing = await readOptionalFile(initScript);
  if (existing !== null) {
    // Refuses a foreign script with the same name before anything is replaced.
    await readOpenRcServiceCommand(args.env);
  }
  const { inline, file } = splitOpenRcEnvironment(args.environment, args.environmentValueSources);
  const script = renderOpenRcInitScript({
    serviceName: name,
    render: {
      description: args.description,
      programArguments: args.programArguments,
      workingDirectory: args.workingDirectory,
    },
    inlineEnvironment: inline,
  });
  args.assertCurrent?.();
  await publishServiceFile({
    filePath: confFile,
    contents: renderOpenRcConfFile(file),
    mode: 0o600,
    definitionTransaction: args.definitionTransaction,
    assertCurrent: args.assertCurrent,
  });
  await publishServiceFile({
    filePath: initScript,
    contents: script,
    mode: 0o755,
    definitionTransaction: args.definitionTransaction,
    assertCurrent: args.assertCurrent,
  });
  return { name, initScript, confFile };
}

function reportPublication(
  stdout: NodeJS.WritableStream,
  label: string,
  paths: { name: string; initScript: string; confFile: string },
) {
  writeFormattedLines(stdout, [
    { label, value: paths.initScript },
    { label: "Service environment", value: paths.confFile },
    { label: "Logs", value: `/var/log/${paths.name}.log` },
  ]);
}

export async function stageOpenRcService({
  stdout,
  ...args
}: GatewayServiceInstallArgs): Promise<void> {
  reportPublication(
    stdout,
    "Staged OpenRC service",
    await writeOpenRcDefinition({ stdout, ...args }),
  );
}

export async function installOpenRcService(args: GatewayServiceInstallArgs): Promise<void> {
  const paths = await writeOpenRcDefinition(args);
  if (!args.preserveAutoStart && !(await listEnabledRunlevels(paths.name)).length) {
    args.assertCurrent?.();
    await runOpenRcAction("rc-update", ["add", paths.name, OPENRC_DEFAULT_RUNLEVEL]);
  }
  args.assertCurrent?.();
  // restart starts a stopped service and reloads the rewritten definition for a running one.
  await runOpenRcAction("rc-service", [paths.name, "restart"]);
  reportPublication(args.stdout, "Installed OpenRC service", paths);
}

export async function uninstallOpenRcService({
  env,
  stdout,
}: GatewayServiceManageArgs): Promise<void> {
  const { name, initScript, confFile } = resolveOpenRcPaths(env);
  if ((await readOptionalFile(initScript)) === null) {
    stdout.write(`OpenRC service not found at ${initScript}\n`);
    return;
  }
  assertOpenRcRoot("removing the Gateway service", env);
  // Refuses to remove a foreign script with the same name.
  await readOpenRcServiceCommand(env);
  const stopped = await execOpenRc("rc-service", [name, "stop"]);
  if (stopped.code !== 0 && (await readOpenRcServiceRuntime(env)).status !== "stopped") {
    throw new Error(`rc-service ${name} stop failed: ${detail(stopped)}`);
  }
  for (const runlevel of await listEnabledRunlevels(name)) {
    await runOpenRcAction("rc-update", ["del", name, runlevel]);
  }
  for (const file of [initScript, confFile]) {
    await fs.unlink(file).catch((error: unknown) => {
      if (!hasErrnoCode(error, "ENOENT")) {
        throw error;
      }
    });
  }
  stdout.write(`${formatLine("Removed OpenRC service", initScript)}\n`);
}

async function runOpenRcControl(
  args: GatewayServiceControlArgs,
  action: "start" | "stop" | "restart",
): Promise<void> {
  const env = args.env ?? process.env;
  const { name } = resolveOpenRcPaths(env);
  assertOpenRcRoot(`${action === "stop" ? "stopping" : `${action}ing`} the Gateway service`, env);
  await args.beforeMutation?.();
  args.assertCurrent?.();
  await args.prepareEffect?.();
  args.beforeEffect?.();
  createGatewayLifecycleMutationReporter(args.onMutation)(`openrc-${action}`);
  if (action === "restart") {
    args.onRestartAttempted?.();
  }
  await runOpenRcAction("rc-service", [name, action]);
  if (action === "stop" && args.disable) {
    for (const runlevel of await listEnabledRunlevels(name)) {
      await runOpenRcAction("rc-update", ["del", name, runlevel]);
    }
  }
  const label = { start: "Started", stop: "Stopped", restart: "Restarted" }[action];
  args.stdout.write(`${formatLine(`${label} OpenRC service`, name)}\n`);
}

export async function startOpenRcService(args: GatewayServiceControlArgs): Promise<void> {
  await runOpenRcControl(args, "start");
}

export async function stopOpenRcService(args: GatewayServiceControlArgs): Promise<void> {
  await runOpenRcControl(args, "stop");
}

export async function restartOpenRcService(
  args: GatewayServiceControlArgs,
): Promise<GatewayServiceRestartResult> {
  await runOpenRcControl(args, "restart");
  return { outcome: "completed" };
}

/** Native manager commands for hints; root-only actions carry sudo for non-root shells. */
export function formatOpenRcServiceCommand(
  env: GatewayServiceEnv,
  action: "start" | "stop" | "restart" | "status",
): string {
  const command = `rc-service ${resolveOpenRcServiceName(env)} ${action}`;
  return action === "status" ? command : rootCommand(command);
}

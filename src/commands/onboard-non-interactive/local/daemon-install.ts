import type { OpenClawConfig } from "../../../config/types.openclaw.js";
import { isOpenRcServiceHost } from "../../../daemon/openrc.js";
import { resolveGatewayService } from "../../../daemon/service.js";
import { isSystemdUserServiceAvailable } from "../../../daemon/systemd.js";
import { formatErrorMessage } from "../../../infra/errors.js";
import type { RuntimeEnv } from "../../../runtime.js";
import { gatewayInstallErrorHint } from "../../daemon-install-helpers.js";
import { resolveGatewayInstallToken } from "../../gateway-install-token.js";
import { prepareGatewayServiceInstall } from "../../gateway-service-setup.js";
import { resolveGatewaySetupRuntime } from "../../gateway-setup-runtime.js";
import type { OnboardOptions } from "../../onboard-types.js";
import { ensureSystemdUserLingerNonInteractive } from "../../systemd-linger.js";

/** Installs the managed gateway daemon when non-interactive setup requested it. */
export async function installGatewayDaemonNonInteractive(params: {
  nextConfig: OpenClawConfig;
  opts: OnboardOptions;
  runtime: RuntimeEnv;
  port: number;
}): Promise<
  | {
      installed: true;
    }
  | {
      installed: false;
      skippedReason?: "systemd-user-unavailable" | "openrc-root-required";
    }
> {
  const { opts, runtime, port } = params;
  const openRc = isOpenRcServiceHost();
  if (openRc && process.geteuid?.() !== 0) {
    runtime.log(
      "OpenRC system services need root; skipping service install. Rerun from a root shell, or run `openclaw gateway install` as root later.",
    );
    return { installed: false, skippedReason: "openrc-root-required" };
  }
  const systemdAvailable =
    process.platform === "linux" && !openRc ? await isSystemdUserServiceAvailable() : true;
  if (process.platform === "linux" && !systemdAvailable) {
    // Container and CI sessions often lack a user systemd manager; onboarding
    // owns the failure outcome for an explicitly requested installation.
    runtime.log(
      "Systemd user services are unavailable; skipping service install. Use a direct shell run (`openclaw gateway run`) or rerun without --install-daemon on this session.",
    );
    return { installed: false, skippedReason: "systemd-user-unavailable" };
  }

  const service = resolveGatewayService();
  const tokenResolution = await resolveGatewayInstallToken({
    config: params.nextConfig,
    env: process.env,
  });
  for (const warning of tokenResolution.warnings) {
    runtime.log(warning);
  }
  if (tokenResolution.unavailableReason) {
    // Installing a daemon without durable gateway auth creates a service that
    // cannot be reached by paired clients after setup exits.
    runtime.error(
      [
        "Gateway install blocked:",
        tokenResolution.unavailableReason,
        "Fix gateway auth config/token input and rerun setup.",
      ].join(" "),
    );
    runtime.exit(1);
    return { installed: false };
  }
  const existingCommand = await service.readCommand(process.env);
  const selection = await resolveGatewaySetupRuntime({
    env: process.env,
    existingCommand,
    runtime: opts.daemonRuntime,
  });
  const installation = await prepareGatewayServiceInstall({
    service,
    selection,
    port,
    existingCommand,
    warn: (message) => runtime.log(message),
    config: params.nextConfig,
  });
  try {
    await installation.install();
  } catch (err) {
    runtime.error(`Gateway service install failed: ${formatErrorMessage(err)}`);
    runtime.log(gatewayInstallErrorHint());
    return { installed: false };
  }
  await ensureSystemdUserLingerNonInteractive({ runtime });
  return { installed: true };
}

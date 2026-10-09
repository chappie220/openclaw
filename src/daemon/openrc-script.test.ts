import { describe, expect, it } from "vitest";
import {
  parseOpenRcServiceDefinition,
  parseOpenRcStatus,
  renderOpenRcConfFile,
  renderOpenRcInitScript,
  splitOpenRcEnvironment,
} from "./openrc-script.js";

describe("OpenRC service definition", () => {
  it("round-trips arguments, working directory, and environment by source", () => {
    const programArguments = [
      "/usr/bin/node",
      "/usr/lib/node_modules/openclaw/dist/index.js",
      "gateway",
      "--port",
      "18789",
      'it\'s quoted "twice" $HOME',
    ];
    const { inline, file } = splitOpenRcEnvironment(
      { HOME: "/root", OPENCLAW_GATEWAY_TOKEN: "s3cr'et", SKIPPED: undefined },
      { OPENCLAW_GATEWAY_TOKEN: "file" },
    );
    const initScript = renderOpenRcInitScript({
      serviceName: "openclaw-gateway",
      render: { programArguments, workingDirectory: "/root" },
      inlineEnvironment: inline,
    });
    const confFile = renderOpenRcConfFile(file);

    expect(confFile).toContain("OPENCLAW_GATEWAY_TOKEN");
    expect(initScript).not.toContain("s3cr");
    expect(parseOpenRcServiceDefinition({ initScript, confFile })).toEqual({
      programArguments,
      workingDirectory: "/root",
      environment: { HOME: "/root", OPENCLAW_GATEWAY_TOKEN: "s3cr'et" },
      environmentValueSources: { HOME: "inline", OPENCLAW_GATEWAY_TOKEN: "file" },
    });
  });

  it("does not claim a foreign init script", () => {
    expect(
      parseOpenRcServiceDefinition({
        initScript: '#!/sbin/openrc-run\ncommand="/usr/bin/node"\n',
        confFile: null,
      }),
    ).toBeNull();
  });

  it("rejects launcher paths that openrc-run would re-split", () => {
    expect(() =>
      renderOpenRcInitScript({
        serviceName: "openclaw-gateway",
        render: { programArguments: ["/opt/my node/bin/node", "gateway"] },
        inlineEnvironment: {},
      }),
    ).toThrow(/plain path/);
  });

  it("maps rc-service status output", () => {
    expect(parseOpenRcStatus(" * status: started\n")).toEqual({
      status: "running",
      state: "started",
    });
    expect(parseOpenRcStatus(" * status: crashed\n")).toEqual({
      status: "stopped",
      state: "crashed",
    });
    expect(parseOpenRcStatus(" * status: starting\n")).toEqual({
      status: "unknown",
      state: "starting",
    });
  });
});

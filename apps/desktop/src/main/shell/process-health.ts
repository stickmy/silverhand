import { app } from "electron";

interface HealthLogger {
  warn(obj: unknown, msg?: string): void;
}

/**
 * Logs Chromium child-process deaths, with the GPU feature status on GPU
 * deaths. A GPU process that dies can come back with some compositor layers no
 * longer painting, and after repeated deaths Chromium drops to software
 * compositing; neither is visible anywhere else in the desktop log.
 */
export function watchProcessHealth(logger: HealthLogger): void {
  app.on("child-process-gone", (_event, details) => {
    logger.warn(
      {
        component: "process-health",
        type: details.type,
        reason: details.reason,
        exitCode: details.exitCode,
        serviceName: details.serviceName,
        name: details.name,
        gpuFeatureStatus: details.type === "GPU" ? app.getGPUFeatureStatus() : undefined,
      },
      "child process gone",
    );
  });

  app.on("render-process-gone", (_event, contents, details) => {
    logger.warn(
      {
        component: "process-health",
        reason: details.reason,
        exitCode: details.exitCode,
        url: contents.getURL().split("#")[0],
      },
      "render process gone",
    );
  });
}

declare const __APP_VERSION__: string;
declare const __APP_COMMIT_SHA__: string;
declare const __APP_BUILD_NUMBER__: string;

function displayVersion(version: string): string {
  return version.startsWith('v') ? version : `v${version}`;
}

export const buildInfo = {
  version: displayVersion(__APP_VERSION__),
  commitSha: __APP_COMMIT_SHA__,
  buildNumber: __APP_BUILD_NUMBER__
};

export const buildInfoLabel = [
  `Version ${buildInfo.version}`,
  buildInfo.buildNumber ? `Build ${buildInfo.buildNumber}` : null,
  buildInfo.commitSha ? `Commit ${buildInfo.commitSha}` : null
]
  .filter(Boolean)
  .join(', ');

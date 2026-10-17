// Interim telemetry ingestion Worker for https://api.amicu.dev/v1/telemetry/rollups,
// standing in for the planned Cloud Run ingestion service. See handler.ts and
// docs/ops/telemetry-ingest.md.

import { handle, type Env } from './handler.ts';

export default {
  fetch(request: Request, env: Env): Promise<Response> {
    return handle(request, env);
  },
};

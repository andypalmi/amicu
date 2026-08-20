import type { RecommendRequest } from '@amicu/api-client';

// A known-valid snapshot that seeds the console so it is non-empty (and returns
// recommendations for a Pro+ caller) on first load. Mirrors the `saturated_metrics`
// fixture in crates/amicu-api/tests/recommend.rs: a saturated stack (hot queue,
// hot GPU, KV eviction) that fires the under-provisioned + preemption rules.
export const EXAMPLE_REQUEST: RecommendRequest = {
  metrics: {
    deployment: 'chat_prod',
    window_seconds: 60,
    latency: {
      ttft_ms: { p50: 90, p90: 150, p99: 220 },
      tpot_ms: { p50: 12, p90: 18, p99: 25 },
      e2e_ms: { p50: 200, p90: 400, p99: 800 },
    },
    throughput: {
      output_tokens_per_s: 800,
      prompt_tokens_per_s: 4000,
      requests_per_s: 6,
    },
    concurrency: {
      in_flight_requests: 32,
      running_batch_size: 16,
      max_num_seqs: 16,
      seq_saturation: 1,
    },
    queue: {
      depth: 40,
      wait_ms: { p50: 200, p90: 700, p99: 1200 },
      admitted: 500,
      rejected: 12,
    },
    memory: {
      kv_cache_utilization: 0.97,
      preemptions: 12,
      recomputations: 4,
      oom_events: 0,
    },
    gpu: { sm_utilization: 0.95, vram_used: '40GiB', power_watts: 320 },
    cost: {
      per_hour: { amount: 1.5, currency: 'USD' },
      per_million_tokens: { amount: 0.5, currency: 'USD' },
      per_request: { amount: 0.001, currency: 'USD' },
      utilization_adjusted_per_hour: { amount: 1.6, currency: 'USD' },
    },
    reliability: { error_rate: 0.01, timeout_rate: 0, restarts: 0 },
  },
  estimate: {
    assumed_prompt_tokens: 512,
    assumed_generation_tokens: 256,
    assumed_concurrency: 8,
    expected_ttft_ms: {
      low: 80,
      high: 300,
      provenance: 'public_benchmark',
      confidence: 'estimated',
    },
    expected_tpot_ms: {
      low: 10,
      high: 30,
      provenance: 'public_benchmark',
      confidence: 'estimated',
    },
  },
};

/** The example snapshot pretty-printed, for seeding the editor textarea. */
export const EXAMPLE_SNAPSHOT = `${JSON.stringify(EXAMPLE_REQUEST, null, 2)}\n`;

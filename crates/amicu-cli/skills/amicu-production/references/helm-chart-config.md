# Helm / Kubernetes delivery

Production vLLM deployments are delivered to Kubernetes via a Helm chart that
Amicu renders from the deployment. The values below are the knobs that matter
for GPU serving and autoscaling; Amicu derives them from the HCL.

## Key values (derived from HCL)

| Concern | Source | Effect |
|---|---|---|
| Replica GPUs | `vllm.tensor_parallel_size` | GPUs requested per pod. |
| GPU count | `hardware_profile.accelerator.count` | `nvidia.com/gpu` resource request. |
| Provider/region | `hardware_profile.provider`/`region` | Node selectors + residency. |
| Container port | `vllm.port` | Service target port. |

## Autoscaling

- Horizontal scaling adds replicas behind a service; each replica holds a full
  copy of the model, so scale for concurrency, not for model size.
- Set sensible min/max replicas; scaling to zero is appropriate for bursty
  internal workloads, not for latency-critical paths.
- GPU nodes are the scaling bottleneck - ensure the cluster/provider has GPU
  capacity in the required region before raising max replicas.

## Residency

Node selection must honor `data_class` residency and `governance_rule`
`allowed_providers`/`allowed_regions`. A residency-violating stack is rejected
at plan time (`AMICU-POLICY-*`) and never reaches apply - see amicu-routing.

Note: rendering to a running cluster is a later milestone; today, validate and
plan describe the intended delivery.

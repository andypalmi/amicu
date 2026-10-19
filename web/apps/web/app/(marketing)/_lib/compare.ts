// Comparison page content. amicu claims follow content/docs (introduction,
// providers, plan-apply-state, state-and-backends, budget, security). Claims
// about other tools come from their official documentation:
//   Terraform  developer.hashicorp.com/terraform/intro
//   OpenTofu   opentofu.org/docs/intro, github.com/opentofu/opentofu (MPL-2.0)
//   SkyPilot   docs.skypilot.ai/en/latest/overview.html and /getting-started/installation.html
//   Ollama     docs.ollama.com, github.com/ollama/ollama (MIT)
//   Modal      modal.com/docs/guide and /guide/gpu
//   RunPod     docs.runpod.io/overview and /runpodctl/overview
// Keep both sides in step with those sources when editing.

export interface CompareRow {
  aspect: string;
  other: string;
  amicu: string;
}

export interface ComparePage {
  slug: string;
  other: string;
  title: string;
  description: string;
  headline: string;
  verdict: string;
  rows: CompareRow[];
  chooseOther: string[];
  chooseAmicu: string[];
  faq: { q: string; a: string }[];
  sources: { label: string; href: string }[];
  summary: string;
}

export const comparePages: ComparePage[] = [
  {
    slug: 'terraform',
    other: 'Terraform and OpenTofu',
    title: 'amicu vs Terraform and OpenTofu',
    description:
      'Terraform and OpenTofu are general infrastructure-as-code tools; amicu is an HCL-based CLI built only for deploying open-weight LLM servers. How they differ and when to use each.',
    headline: 'amicu vs Terraform and OpenTofu',
    summary: 'General infrastructure as code compared with a tool built only for LLM serving.',
    verdict:
      'Terraform and OpenTofu are general-purpose infrastructure-as-code tools: they manage any resource that a provider plugin exposes. amicu is a much narrower open-source CLI that deploys vLLM and llama.cpp servers from HCL and understands models, memory fit and endpoint authentication out of the box. They share the write, plan, apply workflow, so the two are not rivals: use Terraform or OpenTofu for your wider infrastructure, and amicu when the thing you are managing is an LLM endpoint.',
    rows: [
      {
        aspect: 'Scope',
        other: 'Any infrastructure with a provider: clouds, Kubernetes, SaaS and more.',
        amicu: 'Open-weight LLM inference only: models, hardware profiles and deployments.',
      },
      {
        aspect: 'Workflow',
        other: 'Write, plan, apply, with state as the record of real infrastructure.',
        amicu: 'validate, plan, apply, destroy, with state recording what was applied.',
      },
      {
        aspect: 'Configuration language',
        other: 'Declarative configuration files with reusable modules.',
        amicu: 'HCL (amicu.hcl) with a fixed set of resource kinds such as model, hardware_profile and deployment.',
      },
      {
        aspect: 'Knows what a model needs',
        other: 'No built-in concept of a model or of GPU memory fit.',
        amicu: 'validate checks that model weights plus KV cache fit the accelerator or host memory.',
      },
      {
        aspect: 'Serving engines',
        other: 'None built in; you describe the machines or containers yourself.',
        amicu: 'Starts vLLM or llama.cpp for you and exposes its OpenAI-compatible server.',
      },
      {
        aspect: 'Endpoint checks',
        other: 'Not part of the core workflow.',
        amicu: 'After apply, verifies that the endpoint rejects anonymous requests.',
      },
      {
        aspect: 'Where the workload runs',
        other: 'Wherever a provider and your configuration point.',
        amicu: 'This machine, or GPU cloud providers such as Modal and RunPod.',
      },
      {
        aspect: 'State storage',
        other: 'A state file as the source of truth for your environment; remote backends are available.',
        amicu: 'Local state file in the project only; there is no remote backend yet.',
      },
      {
        aspect: 'Ecosystem and maturity',
        other: 'Thousands of providers and a large community.',
        amicu: 'Small and early. Not stable; the HCL schema and CLI may change.',
      },
      {
        aspect: 'License',
        other: 'OpenTofu is MPL-2.0.',
        amicu: 'AGPL-3.0-only.',
      },
    ],
    chooseOther: [
      'You manage networks, databases, clusters and more, and want one tool for all of it.',
      'You need remote state, modules and the provider ecosystem your team already uses.',
      'Your LLM serving already runs on Kubernetes or VMs that you define yourself.',
    ],
    chooseAmicu: [
      'The resource you care about is an LLM server, and you want fit checks before anything starts.',
      'You want to deploy vLLM or llama.cpp to your own machine or a GPU provider without writing the plumbing.',
      'You want residency and provider allowlist rules checked by validate and plan.',
    ],
    faq: [
      {
        q: 'Can I use amicu and Terraform together?',
        a: 'Yes. They manage different things. Terraform or OpenTofu can provision the rest of your infrastructure while amicu deploys and tracks the LLM servers. amicu does not read Terraform state.',
      },
      {
        q: 'Is amicu a Terraform provider?',
        a: 'No. amicu is a standalone command-line tool with its own HCL schema and its own state file.',
      },
      {
        q: 'Does amicu support remote state like Terraform?',
        a: 'Not yet. State is a local file in the project directory, protected by a local lock. Remote state backends are not implemented.',
      },
      {
        q: 'Is amicu production-ready?',
        a: 'amicu is early and not stable yet. Expect breaking changes to the HCL schema and the CLI.',
      },
    ],
    sources: [
      { label: 'Terraform introduction', href: 'https://developer.hashicorp.com/terraform/intro' },
      { label: 'OpenTofu documentation', href: 'https://opentofu.org/docs/intro/' },
    ],
  },
  {
    slug: 'skypilot',
    other: 'SkyPilot',
    title: 'amicu vs SkyPilot',
    description:
      'SkyPilot runs AI workloads across clouds, Kubernetes and Slurm; amicu is a declarative CLI focused on deploying vLLM and llama.cpp servers. A fair comparison and when to pick each.',
    headline: 'amicu vs SkyPilot',
    summary: 'A broad AI compute layer compared with a focused LLM deployment tool.',
    verdict:
      'SkyPilot is a broad system for running AI workloads (clusters, jobs and services) across many clouds, Kubernetes and Slurm. amicu is narrower: a declarative CLI that deploys vLLM or llama.cpp servers from HCL with a plan you review, recorded state and pre-flight checks. If you need training, batch jobs or the widest choice of infrastructure, SkyPilot covers far more ground today. If you want a reviewable, Terraform-style workflow for open-weight model endpoints on your machine or on GPU providers such as Modal and RunPod, amicu is built for that.',
    rows: [
      {
        aspect: 'Scope',
        other: 'AI workloads in general: clusters, jobs and services.',
        amicu: 'LLM inference servers only.',
      },
      {
        aspect: 'Infrastructure',
        other: 'Many clouds, Kubernetes, Slurm and existing machines over SSH.',
        amicu: 'This machine, plus Modal and RunPod. Other providers are preview or planned.',
      },
      {
        aspect: 'Interface',
        other: 'CLI and Python API; tasks can be described in a YAML file.',
        amicu: 'CLI driven by declarative HCL with validate, plan, apply and destroy.',
      },
      {
        aspect: 'Plan before change',
        other: 'Launches and manages clusters and jobs with commands.',
        amicu: 'plan shows a diff against recorded state; apply asks for approval.',
      },
      {
        aspect: 'Serving',
        other: 'Services with replicas across regions, clouds, clusters, pricing models or GPU types.',
        amicu: 'Deployments of vLLM or llama.cpp exposing an OpenAI-compatible endpoint.',
      },
      {
        aspect: 'Model fit checks',
        other: 'You choose the accelerator and your own serving setup.',
        amicu: 'validate checks that weights plus KV cache fit the accelerator or host memory.',
      },
      {
        aspect: 'Cost control',
        other: 'Clusters can be set to stop automatically when idle.',
        amicu: 'RunPod serverless endpoints scale to zero by default; budgets are recorded but not enforced.',
      },
      {
        aspect: 'Training and batch jobs',
        other: 'Supported through jobs and clusters.',
        amicu: 'Not a goal.',
      },
      {
        aspect: 'Maturity',
        other: 'Broader provider coverage and a larger scope.',
        amicu: 'Early and not stable; breaking changes are expected.',
      },
      {
        aspect: 'License',
        other: 'See the SkyPilot repository.',
        amicu: 'AGPL-3.0-only.',
      },
    ],
    chooseOther: [
      'You need training, fine-tuning or batch jobs alongside serving.',
      'You want to spread work across many clouds, Kubernetes clusters or Slurm.',
      'You need capabilities amicu does not have, such as multi-cloud failover.',
    ],
    chooseAmicu: [
      'You only need open-weight LLM endpoints and want a plan-and-apply workflow for them.',
      'You want policy checks (residency, provider allowlists, endpoint auth) before anything starts.',
      'You want one small CLI whose state records what to stop, so nothing is left running by mistake.',
    ],
    faq: [
      {
        q: 'Does amicu replace SkyPilot?',
        a: 'No. SkyPilot covers a much wider range of AI workloads and infrastructure. amicu focuses on declaratively deploying LLM servers.',
      },
      {
        q: 'Which providers does amicu support compared with SkyPilot?',
        a: 'amicu runs on this machine and deploys to Modal and RunPod today. Hetzner, OVHcloud, Scaleway and SSH hosts are in preview, and AWS, Azure, GCP and Nebius are planned. SkyPilot lists many more clouds, plus Kubernetes and Slurm.',
      },
      {
        q: 'Can I use both?',
        a: 'Yes. They are independent tools. Use SkyPilot for jobs and clusters and amicu where you want a declarative LLM deployment.',
      },
    ],
    sources: [
      { label: 'SkyPilot overview', href: 'https://docs.skypilot.ai/en/latest/overview.html' },
      { label: 'SkyPilot installation', href: 'https://docs.skypilot.ai/en/latest/getting-started/installation.html' },
    ],
  },
  {
    slug: 'ollama',
    other: 'Ollama',
    title: 'amicu vs Ollama',
    description:
      'Ollama is a local runner for open models; amicu is a declarative deploy tool that can run llama.cpp or vLLM on your machine or on GPU providers. They are complementary: how to choose.',
    headline: 'amicu vs Ollama',
    summary: 'A local model runner compared with a declarative deployment tool. They complement each other.',
    verdict:
      'Ollama is an easy way to download and run open models on your own computer, with its own API and model library. amicu is a deploy tool: you describe models, hardware and deployments in HCL, review a plan, and amicu runs llama.cpp or vLLM on your machine or on GPU providers such as Modal and RunPod, tracking what it started. If you want to chat with a model on your laptop, Ollama is simpler. If you want a repeatable, reviewable deployment, possibly on a remote GPU, amicu fits. Many people will use both.',
    rows: [
      {
        aspect: 'What it is',
        other: 'A way to run open models locally or in the cloud, with a model library.',
        amicu: 'A CLI that deploys llama.cpp or vLLM servers from HCL configuration.',
      },
      {
        aspect: 'Getting started',
        other: 'Install it and pull a model.',
        amicu: 'Install the CLI, run init, then validate, plan and apply.',
      },
      {
        aspect: 'Engine',
        other: 'Its own runtime; llama.cpp is among its supported backends.',
        amicu: 'llama.cpp or vLLM, chosen per deployment.',
      },
      {
        aspect: 'Where models run',
        other: 'Your own computer, or its cloud models offering.',
        amicu: 'This machine, or GPU cloud providers such as Modal and RunPod.',
      },
      {
        aspect: 'Configuration',
        other: 'Commands and Modelfiles.',
        amicu: 'Declarative HCL kept in your repository, with a plan before every change.',
      },
      {
        aspect: 'API',
        other: 'Its own REST API, with OpenAI-compatible and Anthropic-compatible clients.',
        amicu: "The runtime's own OpenAI-compatible server; amicu is not a gateway.",
      },
      {
        aspect: 'Auth on remote endpoints',
        other: 'Not applicable to a local-only server.',
        amicu: 'Off-machine endpoints require an api_key secret, and apply verifies anonymous requests are rejected.',
      },
      {
        aspect: 'Tracking what is running',
        other: 'The Ollama server manages loaded models.',
        amicu: 'State records every started instance so destroy can stop it.',
      },
      {
        aspect: 'License',
        other: 'MIT.',
        amicu: 'AGPL-3.0-only.',
      },
    ],
    chooseOther: [
      'You want to try open models quickly on your own computer.',
      'You use desktop apps or tools that integrate with Ollama directly.',
      'You do not need vLLM or a reviewable deployment workflow.',
    ],
    chooseAmicu: [
      'You want deployments defined in HCL, reviewed in a plan, and tracked in state.',
      'You need vLLM, or a remote GPU on a provider such as Modal or RunPod.',
      'You want endpoint authentication and residency rules checked as part of apply.',
    ],
    faq: [
      {
        q: 'Are amicu and Ollama competitors?',
        a: 'Only partly. Ollama is a local runner. amicu is a deploy tool for llama.cpp and vLLM servers, locally or on GPU providers. Many setups use both.',
      },
      {
        q: 'Does amicu use Ollama?',
        a: 'No. amicu starts llama.cpp or vLLM itself. It does not depend on Ollama.',
      },
      {
        q: 'Can I run models on my own machine with amicu?',
        a: 'Yes. On macOS, Linux and Windows, amicu runs llama.cpp or vLLM as a user service on this machine, with a loopback endpoint.',
      },
    ],
    sources: [
      { label: 'Ollama documentation', href: 'https://docs.ollama.com/' },
      { label: 'Ollama on GitHub', href: 'https://github.com/ollama/ollama' },
    ],
  },
  {
    slug: 'runpod-modal',
    other: 'RunPod or Modal directly',
    title: 'amicu vs using RunPod or Modal directly',
    description:
      'Compare deploying LLM servers with amicu to using RunPod or Modal through their own CLIs, SDKs and consoles: what amicu adds, what it does not, and when to go direct.',
    headline: 'amicu vs using RunPod or Modal directly',
    summary: 'A declarative layer over GPU providers compared with using their own tools.',
    verdict:
      'RunPod and Modal are the platforms that actually run your GPUs, and their own consoles, CLIs and SDKs expose everything they offer. amicu is a layer on top for one job: deploying vLLM or llama.cpp servers there from HCL, with model fit checks, a plan before changes, recorded state and an endpoint authentication check. Going direct gives you full access to each platform and no extra tool. amicu gives you one workflow across providers, and it is a good fit when the job is specifically an LLM endpoint.',
    rows: [
      {
        aspect: 'What it is',
        other: 'GPU platforms with their own console, CLI and API or SDK.',
        amicu: 'A CLI that deploys LLM servers to providers such as Modal and RunPod using your own account.',
      },
      {
        aspect: 'Platform features',
        other: 'Everything the platform offers, such as RunPod Pods, Serverless and clusters, or Modal Python functions and sandboxes.',
        amicu: 'Only what is needed to deploy and stop LLM servers.',
      },
      {
        aspect: 'How you define a deployment',
        other: 'Console, provider CLI commands, or code (Modal defines GPUs and containers in Python).',
        amicu: 'The same HCL schema for every provider.',
      },
      {
        aspect: 'Review before change',
        other: 'Depends on how you script it.',
        amicu: 'plan diffs configuration against state; apply asks for approval.',
      },
      {
        aspect: 'Model fit',
        other: 'You pick the GPU and tune the serving engine yourself.',
        amicu: 'validate checks weights plus KV cache against the accelerator memory.',
      },
      {
        aspect: 'Cleanup',
        other: 'You track and delete resources yourself.',
        amicu: 'State records stop identities, and destroy confirms each stop before forgetting a deployment.',
      },
      {
        aspect: 'Endpoint auth',
        other: 'You configure it for each platform.',
        amicu: 'Requires an api_key secret and verifies that anonymous requests are rejected.',
      },
      {
        aspect: 'Switching providers',
        other: "Rewrite for the other platform's tools.",
        amicu: 'Change the provider on the hardware profile; models and deployments use the same schema.',
      },
      {
        aspect: 'Credentials',
        other: 'Managed per platform.',
        amicu: 'Read from environment variables; secrets are references and never stored in configuration or state.',
      },
      {
        aspect: 'Limits',
        other: 'Full control, including features amicu does not expose.',
        amicu: 'Models must come from Hugging Face on remote providers; early and not stable.',
      },
    ],
    chooseOther: [
      'You use platform features amicu does not cover, such as training, clusters or custom Python functions.',
      'You run a single provider and are happy scripting it yourself.',
      'You need to tune the platform beyond what amicu exposes.',
    ],
    chooseAmicu: [
      'You want a reviewable plan and recorded state for each LLM endpoint.',
      'You want model fit and endpoint auth checked before and after deploying.',
      'You want the same configuration style for your machine and for GPU providers.',
    ],
    faq: [
      {
        q: 'Does amicu replace the RunPod or Modal console?',
        a: 'No. amicu deploys and stops LLM servers. The platform consoles remain the place for everything else, including billing and account settings.',
      },
      {
        q: 'Do I need my own RunPod or Modal account?',
        a: 'Yes. amicu uses credentials you export in your shell, and talks only to the providers you deploy to.',
      },
      {
        q: 'Which providers does amicu support today?',
        a: 'This machine, plus Modal and RunPod as remote GPU providers. Other providers are in preview or planned; see the providers page.',
      },
      {
        q: 'Does amicu add cost?',
        a: 'The CLI is free and open source. You pay the provider for the GPU time you use.',
      },
    ],
    sources: [
      { label: 'RunPod overview', href: 'https://docs.runpod.io/overview' },
      { label: 'runpodctl', href: 'https://docs.runpod.io/runpodctl/overview' },
      { label: 'Modal guide', href: 'https://modal.com/docs/guide' },
      { label: 'Modal GPU guide', href: 'https://modal.com/docs/guide/gpu' },
    ],
  },
];

export function getComparePage(slug: string): ComparePage | undefined {
  return comparePages.find((p) => p.slug === slug);
}

export function compareHref(slug: string): string {
  return ["/compare", slug].join("/");
}

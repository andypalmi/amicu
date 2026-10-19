import { compareHref } from './compare';
import {
  plannedNames,
  previewNames,
  providers,
  RUNPOD_24GB_HOURLY,
  supportedCloudNames,
} from './content';

export interface FaqItem {
  question: string;
  answer: string;
  link?: { label: string; href: string };
}

const gpusOf = (slug: string) => providers.find((p) => p.slug === slug)?.exampleGpus;

export const faqs = {
  what: {
    question: 'What is amicu?',
    answer: `amicu is an open-source command-line tool, Terraform for your own LLMs, that deploys vLLM and llama.cpp servers to your own machine or to GPU cloud providers (${supportedCloudNames}) from an HCL file. You run amicu plan to see what would change, amicu apply to deploy it and amicu destroy to remove it.`,
  },
  hosted: {
    question: 'Is amicu a hosted service?',
    answer:
      'No. amicu is a CLI that runs on your machine and talks only to the providers you deploy to, plus api.amicu.dev if you opt in to telemetry (off by default). A managed hosted tier is planned but does not exist yet.',
  },
  providers: {
    question: 'Which providers and GPUs does amicu support?',
    answer: `amicu applies to your own hardware (macOS, Linux or Windows) and to the GPU cloud providers ${supportedCloudNames}. ${previewNames} are in preview, and ${plannedNames} are planned. Example GPUs: Modal offers ${gpusOf('modal')}; RunPod offers ${gpusOf('runpod')}. On your own machine amicu uses whatever GPU, Apple Silicon or CPU the machine has.`,
  },
  cost: {
    question: 'What does amicu cost?',
    answer: `The CLI is free and open source. You pay only your provider for compute. For example, a RunPod Serverless worker in the 24 GB GPU group is about ${RUNPOD_24GB_HOURLY} per hour, billed per second while it runs. amicu plan shows indicative estimates from bundled prices, not live quotes. Team, enterprise and hosted plans are planned.`,
  },
  license: {
    question: 'What license is amicu under?',
    answer: 'amicu is licensed under the GNU AGPL-3.0 (SPDX identifier AGPL-3.0-only). The source is on GitHub.',
  },
  terraform: {
    question: 'How does amicu compare to Terraform?',
    answer:
      'The workflow is the same: declarative HCL, a plan you read before anything changes, state that records what was applied, and destroy. amicu is a separate tool with its own HCL schema, built for LLM inference rather than general cloud infrastructure. It checks that a model fits the hardware, enforces data residency at plan time and runs vLLM or llama.cpp for you. It is not a Terraform provider.',
    link: { label: 'Compare amicu and Terraform in detail.', href: compareHref('terraform') },
  },
} satisfies Record<string, FaqItem>;

export type FaqId = keyof typeof faqs;

import type { ComponentProps, ReactNode } from 'react';
import defaultMdxComponents from 'fumadocs-ui/mdx';
import { CalloutDescription, CalloutTitle } from 'fumadocs-ui/components/callout';
import { Tab, Tabs } from 'fumadocs-ui/components/tabs';
import { Accordion, Accordions } from 'fumadocs-ui/components/accordion';
import type { MDXComponents } from 'mdx/types';
import { ProviderFacts } from '@/components/providers/provider-facts';
import { ProviderGrid } from '@/components/providers/provider-grid';
import { RUNPOD_SIGNUP_URL } from '@/app/(marketing)/_lib/content';
import { CircleCheck, CircleX, Info, Lightbulb, TriangleAlert } from 'lucide-react';

type CalloutType = 'info' | 'warning' | 'error' | 'success' | 'idea' | 'tip' | 'warn';

const icons = {
  info: Info,
  warning: TriangleAlert,
  error: CircleX,
  success: CircleCheck,
  idea: Lightbulb,
} as const;

// Docs callout: a flat tinted panel with a coloured icon. Replaces the
// Fumadocs default, which draws a left-edge accent bar and a drop shadow;
// state is carried by the fill tint and icon colour instead.
function Callout({
  type = 'info',
  title,
  icon,
  className,
  children,
  ...props
}: Omit<ComponentProps<'div'>, 'title'> & {
  type?: CalloutType;
  title?: ReactNode;
  icon?: ReactNode;
}) {
  const resolved = type === 'warn' ? 'warning' : type === 'tip' ? 'info' : type;
  const Icon = icons[resolved];
  return (
    <div
      className={`my-4 flex gap-3 rounded-md border bg-(--callout-color)/8 p-3 text-sm text-fd-card-foreground ${className ?? ''}`}
      style={{ '--callout-color': `var(--color-fd-${resolved}, var(--color-fd-muted))` } as React.CSSProperties}
      {...props}
    >
      {icon ?? <Icon className="mt-0.5 size-4 shrink-0 text-(--callout-color)" aria-hidden />}
      <div className="flex min-w-0 flex-1 flex-col gap-2">
        {title ? <CalloutTitle>{title}</CalloutTitle> : null}
        <CalloutDescription>{children}</CalloutDescription>
      </div>
    </div>
  );
}

function RunPodSignupLink({ children = 'Get started with RunPod' }: { children?: ReactNode }) {
  return (
    <a href={RUNPOD_SIGNUP_URL} target="_blank" rel="noopener noreferrer">
      {children}
    </a>
  );
}

export function getMDXComponents(components?: MDXComponents) {
  return {
    ...defaultMdxComponents,
    Callout,
    RunPodSignupLink,
    Tab,
    Tabs,
    Accordion,
    Accordions,
    ProviderFacts,
    ProviderGrid,
    ...components,
  } satisfies MDXComponents;
}

export const useMDXComponents = getMDXComponents;

declare global {
  type MDXProvidedComponents = ReturnType<typeof getMDXComponents>;
}

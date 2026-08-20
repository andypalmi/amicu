import * as React from 'react';
import { cn } from '../lib/cn';

// Plain <label> to avoid an extra Radix dependency; forwards htmlFor/children.
function Label({ className, ...props }: React.LabelHTMLAttributes<HTMLLabelElement>) {
  return (
    <label
      className={cn(
        'text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70',
        className,
      )}
      {...props}
    />
  );
}

export { Label };

'use client';

import { useId } from 'react';
import { Input, Label, type InputProps } from '@amicu/ui';

export interface FormFieldProps extends InputProps {
  label: string;
  error?: string;
}

export function FormField({ label, error, ...inputProps }: FormFieldProps) {
  const id = useId();
  const errorId = `${id}-error`;
  return (
    <div className="space-y-1.5">
      <Label htmlFor={id}>{label}</Label>
      <Input
        id={id}
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? errorId : undefined}
        {...inputProps}
      />
      {error ? (
        <p id={errorId} className="text-xs text-destructive">
          {error}
        </p>
      ) : null}
    </div>
  );
}

'use client';

import { Input, Label, Select } from '@amicu/ui';
import { CURRENCIES } from './wizard';
import type { AnswerValue, MoneyAnswerValue } from '@/lib/recommend/answers';
import type { QuestionDto } from '@/lib/recommend/candidates';

// One engine-provided question's form control, used by the guided wizard's
// questions step (andypalmi/amicu-planning#85). Values are kept as
// strings/booleans/money-pairs while editing; `lib/recommend/answers.ts`'s
// `buildAnswers` casts them to the shape `/recommend/candidates` expects at
// submit time.
//
// Unlike the #31 exercise route's candidate-picker.tsx (which hardcoded
// `currency: 'USD'`), a `money` question here renders an amount input AND a
// currency <Select> (reusing the wizard's existing `CURRENCIES` list) so the
// user actually picks the currency their answer is denominated in.
export function QuestionField({
  question,
  value,
  onChange,
}: {
  question: QuestionDto;
  value: AnswerValue | undefined;
  onChange: (value: AnswerValue) => void;
}) {
  const { kind } = question;

  return (
    <div className="space-y-1.5">
      <Label htmlFor={question.id}>{question.prompt}</Label>
      {kind.type === 'boolean' ? (
        <div className="flex items-center gap-2">
          <input
            id={question.id}
            type="checkbox"
            checked={value === true}
            onChange={(e) => onChange(e.target.checked)}
            className="h-4 w-4"
          />
        </div>
      ) : kind.type === 'choice' ? (
        <Select
          id={question.id}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">Balanced (default)</option>
          {kind.options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </Select>
      ) : kind.type === 'money' ? (
        <div className="flex gap-2">
          <Input
            id={question.id}
            type="number"
            inputMode="decimal"
            min={0}
            step="any"
            required={question.required}
            value={typeof value === 'object' ? value.amount : ''}
            onChange={(e) => {
              const currency = typeof value === 'object' ? value.currency : CURRENCIES[0];
              const next: MoneyAnswerValue = { amount: e.target.value, currency };
              onChange(next);
            }}
            className="flex-1"
          />
          <Select
            aria-label={`${question.prompt} currency`}
            value={typeof value === 'object' ? value.currency : CURRENCIES[0]}
            onChange={(e) => {
              const amount = typeof value === 'object' ? value.amount : '';
              const next: MoneyAnswerValue = { amount, currency: e.target.value };
              onChange(next);
            }}
            className="w-24"
          >
            {CURRENCIES.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </Select>
        </div>
      ) : (
        <Input
          id={question.id}
          type={kind.type === 'number' ? 'number' : 'text'}
          inputMode={kind.type === 'number' ? 'decimal' : undefined}
          min={kind.type === 'number' ? (kind.min ?? undefined) : undefined}
          max={kind.type === 'number' ? (kind.max ?? undefined) : undefined}
          required={question.required}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
      {question.help ? <p className="text-xs text-muted-foreground">{question.help}</p> : null}
      {kind.type === 'number' && kind.unit ? (
        <p className="text-xs text-muted-foreground">Unit: {kind.unit}</p>
      ) : null}
    </div>
  );
}

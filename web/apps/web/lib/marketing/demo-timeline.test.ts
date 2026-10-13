import { test } from 'node:test';
import assert from 'node:assert/strict';
import { DEMO_STEPS, demoDuration, demoState, type Step } from './demo-timeline.ts';

const steps: Step[] = [
  { kind: 'type', at: 100, screen: 'term', text: 'abcd', msPerChar: 10 },
  { kind: 'spinner', at: 200, until: 300, screen: 'term', text: 'job' },
  { kind: 'status', at: 300, status: 'online' },
  { kind: 'screen', at: 400, screen: 'chat' },
  { kind: 'stream', at: 400, screen: 'chat', text: 'one two three', msPerToken: 50 },
];

test('nothing is revealed before the first step', () => {
  const s = demoState(steps, 0);
  assert.equal(s.rows[0].shown, '');
  assert.equal(s.rows[0].rest, 'abcd');
  assert.equal(s.rows[1].visible, false);
  assert.equal(s.status, null);
  assert.equal(s.screen, 'term');
  assert.equal(s.done, false);
});

test('typing reveals one char per interval and keeps the rest reserved', () => {
  const s = demoState(steps, 125);
  assert.equal(s.rows[0].shown, 'ab');
  assert.equal(s.rows[0].rest, 'cd');
});

test('spinner animates then resolves to a check', () => {
  const mid = demoState(steps, 250).rows[1];
  assert.equal(mid.visible, true);
  assert.notEqual(mid.mark, '✓');
  assert.equal(demoState(steps, 300).rows[1].mark, '✓');
});

test('status and screen flip at their step time', () => {
  assert.equal(demoState(steps, 299).status, null);
  assert.equal(demoState(steps, 300).status, 'online');
  assert.equal(demoState(steps, 400).screen, 'chat');
});

test('stream reveals whole tokens', () => {
  assert.equal(demoState(steps, 399).rows[2].shown, '');
  assert.equal(demoState(steps, 400).rows[2].shown, 'one ');
  assert.equal(demoState(steps, 450).rows[2].shown, 'one two ');
});

test('final state is complete and stable', () => {
  const end = demoDuration(steps);
  assert.equal(end, 400 + 3 * 50);
  const s = demoState(steps, end);
  assert.equal(s.done, true);
  assert.equal(s.rows[2].rest, '');
  assert.deepEqual(demoState(steps, Infinity), s);
});

test('real timeline ends online, in chat, fully revealed, under 12s', () => {
  const end = demoDuration(DEMO_STEPS);
  assert.ok(end < 12000);
  const s = demoState(DEMO_STEPS, end);
  assert.equal(s.status, 'online');
  assert.equal(s.screen, 'chat');
  assert.ok(s.rows.every((r) => r.visible && r.rest === ''));
});

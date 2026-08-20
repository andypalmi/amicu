// Pure-logic tests for the org console formatters and guards. Uses Node's
// built-in test runner (`node --test`, with type-stripping) so no test framework
// is added. Run: `node --test --experimental-strip-types 'lib/**/*.test.ts'`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  roleLabel,
  actionLabel,
  permissionLabel,
  isTierDenied,
  looksLikeMembersResponse,
  looksLikeRolesResponse,
  looksLikeAuditResponse,
  assignableRoles,
  canManageMembers,
  canViewAudit,
  canActOnTarget,
} from './format.ts';

test('roleLabel maps known roles and falls back to the raw value', () => {
  assert.equal(roleLabel('owner'), 'Owner');
  assert.equal(roleLabel('admin'), 'Admin');
  assert.equal(roleLabel('member'), 'Member');
  assert.equal(roleLabel('viewer'), 'Viewer');
  assert.equal(roleLabel('wizard'), 'wizard');
});

test('actionLabel maps known audit actions and falls back', () => {
  assert.equal(actionLabel('member.added'), 'Member added');
  assert.equal(actionLabel('member.role_changed'), 'Role changed');
  assert.equal(actionLabel('member.removed'), 'Member removed');
  assert.equal(actionLabel('org.bootstrapped'), 'Organization created');
  assert.equal(actionLabel('mystery.event'), 'mystery.event');
});

test('permissionLabel humanizes snake_case names', () => {
  assert.equal(permissionLabel('view_audit'), 'View audit');
  assert.equal(permissionLabel('assign_privileged_role'), 'Assign privileged role');
});

test('isTierDenied is true only for 403', () => {
  assert.equal(isTierDenied(403), true);
  assert.equal(isTierDenied(401), false);
  assert.equal(isTierDenied(200), false);
});

test('looksLikeMembersResponse accepts a well-formed body', () => {
  assert.equal(
    looksLikeMembersResponse({
      caller_role: 'owner',
      members: [{ user_id: 'usr_1', role: 'owner' }],
    }),
    true,
  );
  // Empty member list is still valid.
  assert.equal(looksLikeMembersResponse({ caller_role: 'admin', members: [] }), true);
});

test('looksLikeMembersResponse rejects malformed bodies', () => {
  assert.equal(looksLikeMembersResponse(null), false);
  assert.equal(looksLikeMembersResponse({ members: [] }), false);
  assert.equal(looksLikeMembersResponse({ caller_role: 'owner', members: 'nope' }), false);
  assert.equal(
    looksLikeMembersResponse({ caller_role: 'owner', members: [{ user_id: 1, role: 'x' }] }),
    false,
  );
});

test('looksLikeRolesResponse validates the catalog shape', () => {
  assert.equal(
    looksLikeRolesResponse({
      roles: [{ role: 'owner', description: 'd', permissions: ['view_members'] }],
    }),
    true,
  );
  assert.equal(looksLikeRolesResponse({ roles: [{ role: 'owner' }] }), false);
  assert.equal(
    looksLikeRolesResponse({ roles: [{ role: 'x', description: 'd', permissions: [1] }] }),
    false,
  );
});

test('looksLikeAuditResponse validates entry shape', () => {
  assert.equal(
    looksLikeAuditResponse({
      entries: [{ id: 1, timestamp: 100, actor_id: 'usr_1', action: 'member.added' }],
    }),
    true,
  );
  assert.equal(looksLikeAuditResponse({ entries: [{ id: '1', timestamp: 100 }] }), false);
  assert.equal(looksLikeAuditResponse({}), false);
});

test('assignableRoles reflects the server RBAC rule', () => {
  assert.deepEqual(assignableRoles('owner'), ['owner', 'admin', 'member', 'viewer']);
  assert.deepEqual(assignableRoles('admin'), ['member', 'viewer']);
  assert.deepEqual(assignableRoles('member'), []);
  assert.deepEqual(assignableRoles('viewer'), []);
});

test('canManageMembers / canViewAudit gate on Owner and Admin', () => {
  assert.equal(canManageMembers('owner'), true);
  assert.equal(canManageMembers('admin'), true);
  assert.equal(canManageMembers('member'), false);
  assert.equal(canManageMembers('viewer'), false);
  assert.equal(canViewAudit('owner'), true);
  assert.equal(canViewAudit('admin'), true);
  assert.equal(canViewAudit('member'), false);
});

test('canActOnTarget reflects the privileged-target rule', () => {
  // Owner may act on anyone.
  assert.equal(canActOnTarget('owner', 'owner'), true);
  assert.equal(canActOnTarget('owner', 'admin'), true);
  assert.equal(canActOnTarget('owner', 'member'), true);
  // Admin may act only on non-privileged members.
  assert.equal(canActOnTarget('admin', 'member'), true);
  assert.equal(canActOnTarget('admin', 'viewer'), true);
  assert.equal(canActOnTarget('admin', 'admin'), false);
  assert.equal(canActOnTarget('admin', 'owner'), false);
  // Others may act on no one.
  assert.equal(canActOnTarget('member', 'viewer'), false);
});

//! Role-based access control: the single shared permission model, enforced
//! identically by every backend (in-memory + Postgres) and every handler
//!
//! [`Permission`] is the enforcement primitive. It is DERIVED from the [`Role`]
//! carried on the access token's `wsrole` claim - never a second, separately
//! carried authority, so there is no additional staleness surface beyond the
//! role itself. [`Role::grants`] is the single role -> permission matrix (both
//! enforcement call sites AND the `GET /org/roles` client catalog read from
//! it, so the catalog can never drift from what is actually enforced).
//! [`authorize_membership_mutation`], [`authorize_invite_create`], and
//! [`authorize_invite_revoke`] are the single implementations of the
//! actor-authorization rules for membership/invite mutations; the in-memory
//! store, the Postgres membership store, and the Postgres invite store all
//! call these SAME functions, so the two backends can never drift from each
//! other.
//!
//! This module is deliberately free of any wire/schema dependency (no
//! `utoipa`): it is the pure domain model. A crate that needs a wire DTO (the
//! org console's `GET /org/roles` catalog) defines its own `ToSchema` type and
//! maps it from [`Permission`]/[`Role::grants`] here.

use crate::tenant::Role;

/// A discrete capability a [`Role`] may hold. The enforcement primitive:
/// handlers and store-side mutation gates check `role.grants(permission)`
/// rather than matching on `Role` directly, so the role -> permission mapping
/// lives in exactly one place ([`Role::grants`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// See the member list and each member's role.
    ViewMembers,
    /// See the role/permission catalog.
    ViewRoles,
    /// See the audit log.
    ViewAudit,
    /// Invite/add a member.
    InviteMember,
    /// Change a member's role.
    ChangeRole,
    /// Remove a member.
    RemoveMember,
    /// Grant or modify a privileged (Owner/Admin) member - Owner only.
    AssignPrivilegedRole,
}

impl Permission {
    /// The wire name for this permission (used by the `GET /org/roles`
    /// catalog and audit/error messages).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::ViewMembers => "view_members",
            Permission::ViewRoles => "view_roles",
            Permission::ViewAudit => "view_audit",
            Permission::InviteMember => "invite_member",
            Permission::ChangeRole => "change_role",
            Permission::RemoveMember => "remove_member",
            Permission::AssignPrivilegedRole => "assign_privileged_role",
        }
    }
}

/// Every permission, in catalog order. Used by the `GET /org/roles` catalog
/// and by exhaustive tests of the role -> permission matrix.
pub const ALL_PERMISSIONS: [Permission; 7] = [
    Permission::ViewMembers,
    Permission::ViewRoles,
    Permission::ViewAudit,
    Permission::InviteMember,
    Permission::ChangeRole,
    Permission::RemoveMember,
    Permission::AssignPrivilegedRole,
];

impl Role {
    /// The full permission set this role holds - the single role -> permission
    /// matrix. Both enforcement call sites and the `GET /org/roles` client
    /// catalog read from this, so the catalog can never drift from what is
    /// actually enforced.
    ///
    /// - **Owner**: every permission.
    /// - **Admin**: manages membership and views the audit log, but may not
    ///   touch a privileged (Owner/Admin) member or grant a privileged role
    ///   (`AssignPrivilegedRole` is Owner-only).
    /// - **Member**: may view the member roster and the role catalog - a
    ///   regular, read-visible member.
    /// - **Viewer**: may view the role catalog only, NOT the member roster.
    ///   This is the deliberate fine-graining that makes Viewer a real, more
    ///   restricted role rather than a Member alias: a Viewer is meant for
    ///   read-only/external-facing access (e.g. an auditor or a billing
    ///   contact) where the org's membership roster - who specifically works
    ///   here, and at what privilege - is itself sensitive information that
    ///   should not be handed out beyond actual members.
    #[must_use]
    pub fn permissions(self) -> &'static [Permission] {
        match self {
            Role::Owner => &[
                Permission::ViewMembers,
                Permission::ViewRoles,
                Permission::ViewAudit,
                Permission::InviteMember,
                Permission::ChangeRole,
                Permission::RemoveMember,
                Permission::AssignPrivilegedRole,
            ],
            Role::Admin => &[
                Permission::ViewMembers,
                Permission::ViewRoles,
                Permission::ViewAudit,
                Permission::InviteMember,
                Permission::ChangeRole,
                Permission::RemoveMember,
            ],
            Role::Member => &[Permission::ViewMembers, Permission::ViewRoles],
            Role::Viewer => &[Permission::ViewRoles],
        }
    }

    /// Whether this role holds `permission`. The single check every handler
    /// and store-side mutation gate consults.
    #[must_use]
    pub fn grants(self, permission: Permission) -> bool {
        self.permissions().contains(&permission)
    }
}

/// Authorize a membership mutation by the actor's role against the target's
/// current role and (for add/change) the requested new role.
///
/// - `actor` must currently be a member (`None` - not, or no longer, a
///   member - is always refused).
/// - The actor must hold [`Permission::InviteMember`] (add),
///   [`Permission::ChangeRole`] (change), or [`Permission::RemoveMember`]
///   (remove) as appropriate - i.e. only Owner/Admin may manage membership at
///   all.
/// - Touching a privileged (Owner/Admin) target, or granting a privileged
///   role, additionally requires [`Permission::AssignPrivilegedRole`], which
///   only Owner holds - so an Admin manages Member/Viewer only.
///
/// Called from inside BOTH the in-memory store's mutation methods (under the
/// store's own lock) and the Postgres membership store's mutation methods
/// (under the mutation's own transaction and row locks), so `actor` and
/// `target_current` are always freshly read at the call site - this function
/// is a pure decision over already-fresh inputs.
///
/// `required` names the base capability the operation needs
/// (`InviteMember`/`ChangeRole`/`RemoveMember`); pass `None` only when there is
/// no base capability to check beyond privileged-role gating (there is not, in
/// practice - every call site names one), letting each of the three mutation
/// kinds phrase its "your role may not manage membership" refusal precisely.
///
/// # Errors
/// A `&'static str` explaining the refusal, suitable for wrapping in each
/// caller's own `Forbidden` variant.
pub fn authorize_membership_mutation(
    actor: Option<Role>,
    required: Permission,
    target_current: Option<Role>,
    new_role: Option<Role>,
) -> Result<(), &'static str> {
    let Some(actor) = actor else {
        return Err("you are not a member of this organization");
    };
    if !actor.grants(required) {
        return Err("your role may not manage organization membership");
    }
    if new_role.is_some_and(Role::is_privileged) && !actor.grants(Permission::AssignPrivilegedRole)
    {
        return Err("only an Owner may grant the Owner or Admin role");
    }
    if target_current.is_some_and(Role::is_privileged)
        && !actor.grants(Permission::AssignPrivilegedRole)
    {
        return Err("only an Owner may modify or remove an Owner or Admin");
    }
    Ok(())
}

/// Authorize creating an invite that grants `granted`, by an actor whose
/// CURRENT role is `actor` (freshly read at the call site, under whatever
/// lock/transaction the caller's mutation runs in).
///
/// Mirrors [`authorize_membership_mutation`]: only an actor holding
/// [`Permission::InviteMember`] may invite at all, and granting a privileged
/// (Owner/Admin) role additionally requires
/// [`Permission::AssignPrivilegedRole`] (Owner only).
///
/// # Errors
/// A `&'static str` explaining the refusal.
pub fn authorize_invite_create(actor: Option<Role>, granted: Role) -> Result<(), &'static str> {
    let Some(actor) = actor else {
        return Err("you are not a member of this workspace");
    };
    if !actor.grants(Permission::InviteMember) {
        return Err("your role may not invite members");
    }
    if granted.is_privileged() && !actor.grants(Permission::AssignPrivilegedRole) {
        return Err("only an Owner may invite an Owner or Admin");
    }
    Ok(())
}

/// Authorize revoking a pending invite, by an actor whose CURRENT role is
/// `actor` (freshly read at the call site). Gated on
/// [`Permission::InviteMember`] - invite management is one capability, so
/// whoever may create an invite may also revoke one (a concurrently demoted
/// or removed actor cannot).
///
/// # Errors
/// A `&'static str` explaining the refusal.
pub fn authorize_invite_revoke(actor: Option<Role>) -> Result<(), &'static str> {
    let Some(actor) = actor else {
        return Err("you are not a member of this workspace");
    };
    if !actor.grants(Permission::InviteMember) {
        return Err("your role may not revoke invites");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_grants_every_permission() {
        for p in ALL_PERMISSIONS {
            assert!(Role::Owner.grants(p), "owner should grant {p:?}");
        }
    }

    #[test]
    fn admin_grants_all_but_assign_privileged() {
        for p in ALL_PERMISSIONS {
            let expected = p != Permission::AssignPrivilegedRole;
            assert_eq!(Role::Admin.grants(p), expected, "admin x {p:?}");
        }
    }

    #[test]
    fn member_grants_only_view_members_and_roles() {
        for p in ALL_PERMISSIONS {
            let expected = matches!(p, Permission::ViewMembers | Permission::ViewRoles);
            assert_eq!(Role::Member.grants(p), expected, "member x {p:?}");
        }
    }

    #[test]
    fn viewer_grants_only_view_roles() {
        for p in ALL_PERMISSIONS {
            let expected = p == Permission::ViewRoles;
            assert_eq!(Role::Viewer.grants(p), expected, "viewer x {p:?}");
        }
    }

    #[test]
    fn viewer_is_strictly_more_restricted_than_member() {
        // The enforced distinction this slice introduces: Viewer loses
        // ViewMembers, which Member retains.
        assert!(Role::Member.grants(Permission::ViewMembers));
        assert!(!Role::Viewer.grants(Permission::ViewMembers));
    }

    // -- authorize_membership_mutation --------------------------------------

    #[test]
    fn membership_mutation_denies_non_member() {
        assert!(
            authorize_membership_mutation(None, Permission::ChangeRole, None, Some(Role::Member))
                .is_err()
        );
    }

    #[test]
    fn membership_mutation_denies_member_and_viewer() {
        for actor in [Role::Member, Role::Viewer] {
            assert!(
                authorize_membership_mutation(
                    Some(actor),
                    Permission::ChangeRole,
                    Some(Role::Member),
                    Some(Role::Viewer),
                )
                .is_err(),
                "{actor:?} must not manage membership"
            );
        }
    }

    #[test]
    fn admin_cannot_grant_privileged_role() {
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::ChangeRole,
                Some(Role::Member),
                Some(Role::Admin),
            )
            .is_err()
        );
    }

    #[test]
    fn admin_cannot_touch_privileged_target() {
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::RemoveMember,
                Some(Role::Owner),
                None,
            )
            .is_err()
        );
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::ChangeRole,
                Some(Role::Admin),
                Some(Role::Member),
            )
            .is_err()
        );
    }

    #[test]
    fn admin_can_manage_non_privileged_members() {
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::InviteMember,
                None,
                Some(Role::Member),
            )
            .is_ok()
        );
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::ChangeRole,
                Some(Role::Member),
                Some(Role::Viewer),
            )
            .is_ok()
        );
        assert!(
            authorize_membership_mutation(
                Some(Role::Admin),
                Permission::RemoveMember,
                Some(Role::Viewer),
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn owner_can_grant_and_touch_privileged() {
        assert!(
            authorize_membership_mutation(
                Some(Role::Owner),
                Permission::InviteMember,
                None,
                Some(Role::Admin),
            )
            .is_ok()
        );
        assert!(
            authorize_membership_mutation(
                Some(Role::Owner),
                Permission::ChangeRole,
                Some(Role::Admin),
                Some(Role::Member),
            )
            .is_ok()
        );
        assert!(
            authorize_membership_mutation(
                Some(Role::Owner),
                Permission::RemoveMember,
                Some(Role::Admin),
                None,
            )
            .is_ok()
        );
    }

    // -- authorize_invite_create / authorize_invite_revoke -------------------

    #[test]
    fn invite_create_denies_non_member_and_non_managers() {
        assert!(authorize_invite_create(None, Role::Member).is_err());
        for actor in [Role::Member, Role::Viewer] {
            assert!(authorize_invite_create(Some(actor), Role::Member).is_err());
        }
    }

    #[test]
    fn invite_create_admin_cannot_grant_privileged() {
        assert!(authorize_invite_create(Some(Role::Admin), Role::Admin).is_err());
        assert!(authorize_invite_create(Some(Role::Admin), Role::Owner).is_err());
        assert!(authorize_invite_create(Some(Role::Admin), Role::Member).is_ok());
        assert!(authorize_invite_create(Some(Role::Admin), Role::Viewer).is_ok());
    }

    #[test]
    fn invite_create_owner_can_grant_privileged() {
        assert!(authorize_invite_create(Some(Role::Owner), Role::Admin).is_ok());
        assert!(authorize_invite_create(Some(Role::Owner), Role::Owner).is_ok());
    }

    #[test]
    fn invite_revoke_denies_non_managers() {
        assert!(authorize_invite_revoke(None).is_err());
        assert!(authorize_invite_revoke(Some(Role::Member)).is_err());
        assert!(authorize_invite_revoke(Some(Role::Viewer)).is_err());
        assert!(authorize_invite_revoke(Some(Role::Admin)).is_ok());
        assert!(authorize_invite_revoke(Some(Role::Owner)).is_ok());
    }
}

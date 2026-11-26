//! Mirror of nexgraph's role policy, used only to hide controls the server would reject (the server enforces everything).
use crate::api::User;

pub fn rank(role: &str) -> u8 {
    match role {
        "reader" => 1,
        "writer" => 2,
        "tenant_admin" => 3,
        "platform_admin" => 4,
        _ => 0,
    }
}

/// Tenant accounts have a graph; platform administrators manage tenants but cannot read tenant data.
pub fn has_data(u: &User) -> bool {
    u.tenant.is_some()
}
pub fn can_write(u: &User) -> bool {
    has_data(u) && rank(&u.role) >= 2
}
/// `POST /v1/checkpoint` requires exactly tenant_admin.
pub fn can_checkpoint(u: &User) -> bool {
    u.role == "tenant_admin" && has_data(u)
}
pub fn can_audit(u: &User) -> bool {
    rank(&u.role) >= 3
}
pub fn can_manage_users(u: &User) -> bool {
    rank(&u.role) >= 3
}
pub fn is_platform(u: &User) -> bool {
    u.role == "platform_admin"
}

#[cfg(test)]
mod tests {
    use super::*;
    fn u(role: &str, tenant: Option<&str>) -> User {
        User { user: "x".into(), tenant: tenant.map(String::from), role: role.into() }
    }
    #[test]
    fn policy() {
        assert!(has_data(&u("reader", Some("t"))) && !can_write(&u("reader", Some("t"))));
        assert!(can_write(&u("writer", Some("t"))));
        assert!(!has_data(&u("platform_admin", None)) && !can_write(&u("platform_admin", None)));
        assert!(can_checkpoint(&u("tenant_admin", Some("t"))) && !can_checkpoint(&u("writer", Some("t"))));
        assert!(can_audit(&u("platform_admin", None)) && !can_audit(&u("writer", Some("t"))));
        assert!(is_platform(&u("platform_admin", None)) && !is_platform(&u("tenant_admin", Some("t"))));
    }
}

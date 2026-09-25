//! Role → permission policy.
//!
//! Zonemail is an authn/authz **verifier**, not an authz server: the set of
//! roles and the permissions each is granted is fixed in configuration
//! ([`crate::config::AuthConfig::roles`]), and the upstream auth server
//! (e.g. Janux) mints the JWTs that name one of those roles. This module turns
//! that config into an in-memory [`AuthPolicy`] and answers a single question:
//! *for the caller's role set, is `(resource, verb)` allowed?*
//!
//! A "permission" is `resource:verb`, matching the shape the API routes take:
//!
//! | resource    | route(s)                                   |
//! |------------|--------------------------------------------|
//! | `domains`  | `/domains`, `/domains/{id}`                 |
//! | `mailboxes`| `/mailboxes`, `/mailboxes/{id}`(+messages) |
//! | `records`  | `/records`, `/records/{id}`                 |
//! | `mail`     | `/mail`                                     |
//! | `services` | `/services`, `/services/**`                  |
//! | `messages` | `/messages/{id}`                           |
//! | `health`,`doc`   | always public (see the default `public` list) |

use std::collections::{HashMap, HashSet};

use salvo::http::Method;

use crate::config::RoleConfig;

/// A class of request, derived from its HTTP method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verb {
    Read,
    Write,
    Delete,
}

impl Verb {
     /// The canonical grant-token name for this verb.
    pub fn name(self) -> &'static str {
        match self {
            Verb::Read => "read",
            Verb::Write => "write",
            Verb::Delete => "delete",
         }
     }

     /// Classify an HTTP method into a verb. `GET`/`HEAD`/`OPTIONS` are reads,
     /// `DELETE` is a delete; everything else (`POST`/`PUT`/`PATCH`, and any
     /// non-standard method) is a *write* so an unlisted method is never silently
     /// allowed as a read.
    pub fn from_method(method: &Method) -> Verb {
        match method.as_str() {
             "GET" | "HEAD" | "OPTIONS" => Verb::Read,
             "DELETE" => Verb::Delete,
              _ => Verb::Write,
          }
     }

    fn parse(token: &str) -> Option<Verb> {
        match token.trim().to_ascii_lowercase().as_str() {
             "read" | "get" | "r" => Some(Verb::Read),
             "write" | "post" | "put" | "patch" | "w" => Some(Verb::Write),
             "delete" | "del" | "d" => Some(Verb::Delete),
              _ => None,
          }
     }
}

/// A single permission granted to a role, normalized from config.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Grant {
     /// `"*"` — every resource and verb.
    All,
     /// `"res:*"` — any verb on a specific resource.
    AnyVerb(String),
     /// `"*:verb"` — any resource for a specific verb.
    AnyResource(Verb),
     /// `"res:verb"` — an exact resource and verb.
    Exact(String, Verb),
}

impl Grant {
     /// Parse one grant token (e.g. `"domains:read"`). Returns `None` for an
     /// unrecognized verb component; unknown tokens are ignored (fail-closed),
     /// never silently broadened.
    fn parse(token: &str) -> Option<Grant> {
        let raw = token.trim();
        if raw == "*" || raw == "*:*" || raw.to_ascii_lowercase() == "all" {
            return Some(Grant::All);
          }

        let (lhs, rhs) = match raw.split_once(':') {
            Some((l, r)) => (l.trim(), r.trim()),
              // No colon: treat as a bare resource granting every verb on it.
              None => return Some(Grant::AnyVerb(raw.to_string())),
           };

         // Wildcard on the *verb* side: `res:*` (or `res:` / `res`).
        if rhs == "*" || rhs.is_empty() {
            return Some(if lhs == "*" {
                Grant::All
            } else {
                Grant::AnyVerb(lhs.to_ascii_lowercase())
               });
         }
         // Wildcard on the *resource* side: `*:verb`.
        if lhs == "*" {
            return Verb::parse(rhs).map(Grant::AnyResource);
         }
         // Exact `res:verb`.
        Verb::parse(rhs).map(|v| Grant::Exact(lhs.to_ascii_lowercase(), v))
      }

     /// Does this grant cover `(resource, verb)`?
    fn matches(&self, resource: &str, verb: Verb) -> bool {
        match self {
            Grant::All => true,
            Grant::AnyVerb(r) => r == resource,
            Grant::AnyResource(v) => *v == verb,
            Grant::Exact(r, v) => r == resource && *v == verb,
         }
      }
}

/// The request-side coordinates a route resolves to for an authz check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    pub resource: String,
    pub verb: Verb,
}

/// Map a request path to the governed resource it belongs to, or `None` if the
/// path is outside the API resource surface (an unregistered path, a `/` root,
/// etc.). Only the first path segment matters.
pub fn resource_for_path(path: &str) -> Option<String> {
    let seg = path
          .trim_start_matches('/')
          .split('/')
          .next()
          .unwrap_or_default();
    if seg.is_empty() {
        return None;
      }
    Some(seg.to_ascii_lowercase())
}

/// Map a request (path + method) to its `(resource, verb)` target. `None` means
/// the path is not a governed API resource.
pub fn route_target(path: &str, method: &Method) -> Option<RouteTarget> {
    let resource = resource_for_path(path)?;
    Some(RouteTarget { resource, verb: Verb::from_method(method) })
}

/// The compiled role → permission table, plus the set of resources that stay
/// open (public) regardless of role.
#[derive(Debug, Clone, Default)]
pub struct AuthPolicy {
     /// Effective grant sets per role, with `extends` transitively resolved.
    role_grants: HashMap<String, HashSet<Grant>>,
     /// Resources that bypass authorization.
    public: HashSet<String>,
}

impl AuthPolicy {
     /// Compile roles + `extends` inheritance + the public-resource list. Unknown
     /// `extends` targets are dropped; cycles are broken by a visited-set.
    pub fn build(roles: &HashMap<String, RoleConfig>, public: &[String]) -> AuthPolicy {
         // 1. Collect each role's *direct* grants and its `extends` list.
         let mut direct: HashMap<String, HashSet<Grant>> = HashMap::new();
         let mut extends: HashMap<String, Vec<String>> = HashMap::new();
         for (name, cfg) in roles {
             let key = name.to_ascii_lowercase();
             let set = direct.entry(key.clone()).or_default();
             for g in &cfg.grants {
                 if let Some(grant) = Grant::parse(g) {
                     set.insert(grant);
                       }
                  }
             let list = extends.entry(key).or_default();
             for e in &cfg.extends {
                 list.push(e.to_ascii_lowercase());
                   }
              }

               // 2. Fixpoint: a role's effective grant set is its direct grants unioned
             // with the effective sets of every role it `extends`. Iterate to a fixed
             // point; `guard` caps the loop so a mutual cycle converges rather than
             // hanging.
         let mut effective: HashMap<String, HashSet<Grant>> = direct.clone();
         let mut changed = true;
         let mut guard = 0;
         while changed && guard < 10_000 {
             changed = false;
             guard += 1;
             let snapshot = effective.clone();
             for (role, deps) in &extends {
                 let mut merged = snapshot.get(role).cloned().unwrap_or_default();
                 let before = merged.len();
                 for dep in deps {
                     if let Some(dg) = snapshot.get(dep) {
                         merged.extend(dg.iter().cloned());
                            }
                     }
                 if merged.len() != before {
                     let entry = effective.entry(role.clone()).or_default();
                     entry.extend(merged.iter().cloned());
                     changed = true;
                        }
                  }
              }

         AuthPolicy {
             role_grants: effective,
             public: public.iter().map(|s| s.to_ascii_lowercase()).collect(),
           }
        }

     /// Is this resource public (bypasses the role check)?
    pub fn is_public(&self, resource: &str) -> bool {
        self.public.contains(&resource.to_ascii_lowercase())
      }

     /// Is `(resource, verb)` permitted for any of the caller's roles?
    pub fn allows(&self, roles: &[String], target: &RouteTarget) -> bool {
       // Public resources need no role.
        let resource = target.resource.to_ascii_lowercase();
        if self.public.contains(&resource) {
            return true;
         }
        for role in roles {
            let key = role.to_ascii_lowercase();
            if let Some(grants) = self.role_grants.get(&key) {
                if grants.iter().any(|g| g.matches(&resource, target.verb)) {
                    return true;
                 }
             }
         }
        false
      }
}

// ===========================================================================
// Tests
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn target(resource: &str, verb: Verb) -> RouteTarget {
        RouteTarget { resource: resource.to_string(), verb }
      }

     fn role(grants: &[&str], extends: &[&str]) -> RoleConfig {
        RoleConfig {
            grants: grants.iter().map(|s| s.to_string()).collect(),
            extends: extends.iter().map(|s| s.to_string()).collect(),
         }
      }

     # [test]
     fn empty_policy_is_deny_default() {
        let p = AuthPolicy::build(&HashMap::new(), &["health", "doc"].iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(!p.allows(&[], &target("domains", Verb::Read)));
        assert!(!p.allows(&[], &target("domains", Verb::Write)));
        assert!(!p.allows(&["nobody".into()], &target("domains", Verb::Read)));
      }

     # [test]
     fn exact_grant_matches_only_its_pair() {
        let mut roles = HashMap::new();
        roles.insert("viewer".to_string(), role(&["domains:read", "mailboxes:read"], &[]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(p.allows(&["viewer".into()], &target("domains", Verb::Read)));
        assert!(p.allows(&["viewer".into()], &target("mailboxes", Verb::Read)));
        assert!(!p.allows(&["viewer".into()], &target("domains", Verb::Write)));
        assert!(!p.allows(&["viewer".into()], &target("records", Verb::Read)));
      }

     # [test]
     fn star_and_resource_wildcards() {
        let mut roles = HashMap::new();
        roles.insert("admin".to_string(), role(&["*"], &[]));
        roles.insert("editor".to_string(), role(&["domains:*", "*:read"], &[]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(p.allows(&["admin".into()], &target("anything", Verb::Delete)));
        assert!(p.allows(&["editor".into()], &target("domains", Verb::Write)));
        assert!(p.allows(&["editor".into()], &target("services", Verb::Read)));
        assert!(!p.allows(&["editor".into()], &target("services", Verb::Write)));
      }

     # [test]
     fn extends_inherits_grants() {
        let mut roles = HashMap::new();
        roles.insert("base".to_string(), role(&["domains:read"], &[]));
        roles.insert("child".to_string(), role(&["domains:write"], &["base"]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(p.allows(&["child".into()], &target("domains", Verb::Read)));
        assert!(p.allows(&["child".into()], &target("domains", Verb::Write)));
        assert!(!p.allows(&["base".into()], &target("domains", Verb::Write)));
      }

     # [test]
     fn unknown_grant_verb_is_ignored_not_broadened() {
        let mut roles = HashMap::new();
        roles.insert("x".to_string(), role(&["domains:banana"], &[]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(!p.allows(&["x".into()], &target("domains", Verb::Read)));
      }

     # [test]
     fn public_resources_bypass_roles() {
        let p = AuthPolicy::build(&HashMap::new(), &["health", "doc"].iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(p.allows(&[], &target("health", Verb::Read)));
        assert!(p.allows(&[], &target("doc", Verb::Read)));
        assert!(!p.allows(&[], &target("domains", Verb::Read)));
      }

     # [test]
     fn extends_cycle_does_not_hang() {
        let mut roles = HashMap::new();
        roles.insert("a".to_string(), role(&["records:read"], &["b"]));
        roles.insert("b".to_string(), role(&["domains:read"], &["a"]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(p.allows(&["a".into()], &target("records", Verb::Read)));
        assert!(p.allows(&["a".into()], &target("domains", Verb::Read)));
        assert!(p.allows(&["b".into()], &target("records", Verb::Read)));
      }

     # [test]
     fn method_to_verb_classification() {
        assert_eq!(Verb::from_method(&Method::GET), Verb::Read);
        assert_eq!(Verb::from_method(&Method::HEAD), Verb::Read);
        assert_eq!(Verb::from_method(&Method::OPTIONS), Verb::Read);
        assert_eq!(Verb::from_method(&Method::POST), Verb::Write);
        assert_eq!(Verb::from_method(&Method::PATCH), Verb::Write);
        assert_eq!(Verb::from_method(&Method::PUT), Verb::Write);
        assert_eq!(Verb::from_method(&Method::DELETE), Verb::Delete);
      }

     # [test]
     fn resource_for_path_and_target() {
        assert_eq!(resource_for_path("/domains/abc"), Some("domains".to_string()));
        assert_eq!(resource_for_path("/services/smtp/start"), Some("services".to_string()));
        assert_eq!(resource_for_path("/messages/9"), Some("messages".to_string()));
        assert_eq!(resource_for_path("/"), None);
        assert_eq!(resource_for_path(""), None);
        assert_eq!(route_target("/domains/abc", &Method::GET), Some(target("domains", Verb::Read)));
        assert_eq!(route_target("/domains", &Method::POST), Some(target("domains", Verb::Write)));
      }

     # [test]
     fn case_insensitive_role_and_resource() {
        let mut roles = HashMap::new();
        roles.insert("Admin".to_string(), role(&["Domains:read"], &[]));
        let p = AuthPolicy::build(&roles, &[]);
        assert!(p.allows(&["admin".into()], &target("domains", Verb::Read)));
        // Role (case-insensitive) and resource (case-insensitive) both resolve.
        assert!(p.allows(&["ADMIN".into()], &target("DOMAINS", Verb::Read)));
      }
}

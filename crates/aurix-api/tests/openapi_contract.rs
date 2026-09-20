//! Keeps `api/openapi.json` honest: every route the router registers must be documented with
//! the same methods, nothing may be documented that does not exist, and every permission the
//! handlers check must be a known one that the document lists.

use std::collections::{BTreeMap, BTreeSet};

const ROUTES_RS: &str = include_str!("../src/routes.rs");
const HANDLER_SOURCES: &[&str] = &[
    include_str!("../src/handlers.rs"),
    include_str!("../src/streams.rs"),
    include_str!("../src/webhooks.rs"),
];

fn spec() -> serde_json::Value {
    serde_json::from_str(aurix_api::handlers::OPENAPI_JSON).expect("api/openapi.json parses")
}

/// `(path, methods)` for every `.route("…", …)` in `routes.rs`, with `:param` → `{param}`.
fn router_routes() -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rest = ROUTES_RS;
    while let Some(pos) = rest.find(".route(") {
        rest = &rest[pos + ".route(".len()..];
        let rest_trim = rest.trim_start();
        let Some(path_body) = rest_trim.strip_prefix('"') else {
            continue;
        };
        let end = path_body.find('"').expect("closing quote");
        let path = &path_body[..end];
        // The handler chain runs until the parenthesis that closes `.route(` — routes never
        // nest parentheses deeper than `method(handler)`, so count depth.
        let mut depth = 1usize;
        let mut chain_end = 0usize;
        for (i, c) in rest.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        chain_end = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let chain = &rest[end + 1..chain_end];
        let methods = out.entry(openapi_path(path)).or_default();
        for m in ["get", "post", "put", "patch", "delete"] {
            if chain.contains(&format!("{m}(")) {
                methods.insert(m.to_string());
            }
        }
    }
    out
}

fn openapi_path(axum_path: &str) -> String {
    axum_path
        .split('/')
        .map(|seg| match seg.strip_prefix(':') {
            Some(name) => format!("{{{name}}}"),
            None => seg.to_string(),
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn spec_routes(spec: &serde_json::Value) -> BTreeMap<String, BTreeSet<String>> {
    spec["paths"]
        .as_object()
        .expect("paths object")
        .iter()
        .map(|(path, item)| {
            let methods = item
                .as_object()
                .expect("path item")
                .keys()
                .filter(|k| ["get", "post", "put", "patch", "delete"].contains(&k.as_str()))
                .cloned()
                .collect();
            (path.clone(), methods)
        })
        .collect()
}

#[test]
fn every_router_route_is_documented_and_vice_versa() {
    let router = router_routes();
    let documented = spec_routes(&spec());
    assert!(router.len() > 60, "route parser found too few routes");
    for (path, methods) in &router {
        let doc = documented
            .get(path)
            .unwrap_or_else(|| panic!("{path} is routed but missing from api/openapi.json"));
        assert_eq!(doc, methods, "methods differ for {path}");
    }
    for path in documented.keys() {
        assert!(
            router.contains_key(path),
            "{path} is documented but not routed"
        );
    }
}

#[test]
fn required_permissions_are_known_and_documented() {
    let spec = spec();
    let known: BTreeSet<&str> = aurix_api::handlers::KNOWN_PERMISSIONS
        .iter()
        .copied()
        .collect();

    let mut checked = BTreeSet::new();
    for src in HANDLER_SOURCES {
        for needle in ["ctx.require(\"", "ctx.has(\""] {
            let mut rest = *src;
            while let Some(pos) = rest.find(needle) {
                rest = &rest[pos + needle.len()..];
                let end = rest.find('"').expect("closing quote");
                checked.insert(rest[..end].to_string());
            }
        }
    }
    assert!(checked.len() > 15, "permission scan found too few checks");
    for perm in &checked {
        assert!(
            known.contains(perm.as_str()),
            "handler requires '{perm}' which is not in KNOWN_PERMISSIONS — scoped keys could never be granted it"
        );
    }

    let mut documented = BTreeSet::new();
    for item in spec["paths"].as_object().unwrap().values() {
        for op in item.as_object().unwrap().values() {
            if let Some(perms) = op.get("x-aurix-permissions").and_then(|p| p.as_array()) {
                for p in perms {
                    // Entries may carry a qualifier after a space ("moderation:write (kick…)").
                    let name = p.as_str().unwrap().split(' ').next().unwrap().to_string();
                    assert!(
                        known.contains(name.as_str()),
                        "api/openapi.json documents unknown permission '{name}'"
                    );
                    documented.insert(name);
                }
            }
        }
    }
    for perm in &checked {
        if perm == "*" {
            continue;
        }
        assert!(
            documented.contains(perm),
            "'{perm}' is required by a handler but no operation documents it"
        );
    }

    let listed = spec["info"]["description"].as_str().unwrap();
    for perm in &known {
        assert!(
            listed.contains(&format!("`{perm}`")),
            "info.description does not list permission '{perm}'"
        );
    }
}

#[test]
fn spec_metadata_matches_crate() {
    let spec = spec();
    assert_eq!(spec["openapi"], "3.1.0");
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
    for scheme in [
        "ApiKeyHeader",
        "ApiKeyBearer",
        "PlayerToken",
        "AdminToken",
        "BootstrapToken",
    ] {
        assert!(
            spec["components"]["securitySchemes"][scheme].is_object(),
            "missing security scheme {scheme}"
        );
    }
}

/// Admin operations: every `admin.require(AdminPermission::…)` in the handlers must be one of
/// the typed permissions and documented on some operation, every admin-token operation must
/// document its permission (or be in the explicit "any active administrator" list), and the
/// documented role table must match the code's matrix.
#[test]
fn admin_permissions_are_documented() {
    use aurix_common::types::{AdminPermission, AdminRole};

    let spec = spec();
    let variants: BTreeMap<String, &str> = AdminPermission::ALL
        .iter()
        .map(|p| (format!("{p:?}"), p.as_str()))
        .collect();
    let known: BTreeSet<&str> = variants.values().copied().collect();

    let mut checked = BTreeSet::new();
    for src in HANDLER_SOURCES {
        let needle = "admin.require(AdminPermission::";
        let mut rest = *src;
        while let Some(pos) = rest.find(needle) {
            rest = &rest[pos + needle.len()..];
            let end = rest.find(')').expect("closing paren");
            let variant = &rest[..end];
            let name = variants
                .get(variant)
                .unwrap_or_else(|| panic!("unknown AdminPermission::{variant}"));
            checked.insert(name.to_string());
        }
    }
    assert!(
        checked.len() >= 8,
        "admin permission scan found too few checks"
    );

    let any_admin: BTreeSet<&str> = ["/admin/me", "/admin/me/password", "/admin/logout-all"]
        .into_iter()
        .collect();
    let mut documented = BTreeSet::new();
    for (path, item) in spec["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if !["get", "post", "put", "patch", "delete"].contains(&method.as_str()) {
                continue;
            }
            let admin_token = op["security"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s.get("AdminToken").is_some()));
            let perm = op.get("x-aurix-admin-permission").and_then(|p| p.as_str());
            match (admin_token, perm) {
                (true, Some(p)) => {
                    assert!(
                        known.contains(p),
                        "{method} {path} documents unknown admin permission '{p}'"
                    );
                    documented.insert(p.to_string());
                }
                (true, None) => assert!(
                    any_admin.contains(path.as_str()),
                    "{method} {path} takes an admin token but documents no x-aurix-admin-permission"
                ),
                (false, Some(_)) => {
                    panic!("{method} {path} documents an admin permission without AdminToken")
                }
                (false, None) => {}
            }
        }
    }
    for perm in &checked {
        assert!(
            documented.contains(perm),
            "'{perm}' is required by a handler but no operation documents it"
        );
    }

    let listed = spec["info"]["description"].as_str().unwrap();
    for role in AdminRole::ALL {
        assert!(
            listed.contains(&format!("| `{role}` |")),
            "info.description lacks the row for role {role}"
        );
    }
    for perm in AdminPermission::ALL {
        assert!(
            listed.contains(&format!("`{}`", perm.as_str())),
            "info.description does not list admin permission '{}'",
            perm.as_str()
        );
    }
    let enum_roles: Vec<&str> = spec["components"]["schemas"]["AdminRole"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        enum_roles,
        AdminRole::ALL
            .iter()
            .map(|r| r.as_str())
            .collect::<Vec<_>>()
    );
    let enum_perms: BTreeSet<&str> = spec["components"]["schemas"]["AdminPermission"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(enum_perms, known);
}

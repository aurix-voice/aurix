//! Administrator lifecycle against a real PostgreSQL (`AURIX_E2E_DATABASE_URL`): bootstrap,
//! password and SSO logins, typed roles enforced from the *current* row, token revocation on
//! role change / deactivation / password change / logout-all, the self- and last-superadmin
//! guards, SSO binding and provisioning, and the password-login-disabled mode.
//!
//! The suite needs an empty admin table (the "last superadmin" guard is global), so it creates
//! a throw-away database next to the configured one (the role must be allowed to `CREATE
//! DATABASE`, which the CI/dev `aurix` superuser is), runs the migrations there — which also
//! exercises `20240101000013_admin_sso.sql` from scratch — and drops it at the end.

use aurix_auth::admin::{AdminAuthService, AdminUpdate, SsoLogin};
use aurix_common::error::AurixError;
use aurix_common::types::{AdminAuthSource, AdminPermission, AdminRole};
use aurix_db::DbPool;
use sqlx::postgres::PgPoolOptions;
use sqlx::Connection;
use uuid::Uuid;

const SECRET: &str = "pg-live-admin-secret-0123456789abcdef";
const ISSUER: &str = "https://idp.example.com";

struct TempDb {
    admin_url: String,
    name: String,
    pool: DbPool,
}

impl TempDb {
    async fn create() -> Option<TempDb> {
        let admin_url = std::env::var("AURIX_E2E_DATABASE_URL").ok()?;
        let name = format!("aurix_admin_test_{}", Uuid::new_v4().simple());
        let mut conn = sqlx::PgConnection::connect(&admin_url)
            .await
            .expect("postgres");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&mut conn)
            .await
            .expect("create database");
        conn.close().await.ok();
        let url = replace_database(&admin_url, &name);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect temp database");
        aurix_db::migrations::MIGRATOR
            .run(&pool)
            .await
            .expect("migrations");
        Some(TempDb {
            admin_url,
            name,
            pool,
        })
    }

    async fn drop(self) {
        self.pool.close().await;
        let mut conn = sqlx::PgConnection::connect(&self.admin_url)
            .await
            .expect("postgres");
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.name))
            .execute(&mut conn)
            .await
            .expect("drop database");
    }
}

fn replace_database(url: &str, name: &str) -> String {
    let mut parsed = url::Url::parse(url).expect("database url");
    parsed.set_path(&format!("/{name}"));
    parsed.to_string()
}

fn sso(subject: &str, email: &str, role: AdminRole) -> SsoLogin {
    SsoLogin {
        issuer: ISSUER.into(),
        subject: subject.into(),
        email: email.into(),
        display_name: format!("SSO {subject}"),
        role,
        auto_provision: true,
        sync_role: true,
    }
}

#[tokio::test]
async fn admin_lifecycle_roles_revocation_and_sso() {
    let Some(db) = TempDb::create().await else {
        eprintln!("AURIX_E2E_DATABASE_URL not set; skipping");
        return;
    };
    // Drop the temporary database even when an assertion fails.
    let outcome = tokio::spawn(lifecycle(db.pool.clone())).await;
    db.drop().await;
    outcome.expect("lifecycle scenario");
}

async fn lifecycle(pool: DbPool) {
    let svc = AdminAuthService::new(pool.clone(), SECRET.into())
        .with_bootstrap_token(Some("bootstrap-0123456789".into()));

    // --- bootstrap -----------------------------------------------------------------------
    assert_eq!(svc.admin_count().await.unwrap(), 0);
    let root = svc
        .bootstrap_admin(None, "Root@Example.com", "root-password-0123", "Root")
        .await
        .unwrap();
    assert_eq!(root.email, "root@example.com");
    assert_eq!(root.role, "superadmin");
    assert_eq!(root.auth_source, "password");
    let denied = svc
        .bootstrap_admin(None, "second@example.com", "second-password-0123", "Second")
        .await
        .unwrap_err();
    assert!(
        matches!(denied, AurixError::AuthorizationDenied(_)),
        "{denied:?}"
    );
    let wrong = svc
        .bootstrap_admin(
            Some("nope"),
            "second@example.com",
            "second-password-0123",
            "Second",
        )
        .await
        .unwrap_err();
    assert!(matches!(wrong, AurixError::AuthorizationDenied(_)));
    let ops = svc
        .bootstrap_admin(
            Some("bootstrap-0123456789"),
            "ops@example.com",
            "ops-password-0123456",
            "Ops",
        )
        .await
        .unwrap();
    assert_eq!(ops.role, "superadmin");

    // --- password login and typed permissions --------------------------------------------
    let (_, root_token) = svc
        .authenticate("root@example.com", "root-password-0123")
        .await
        .unwrap();
    let root_ctx = svc.authenticate_token(&root_token).await.unwrap();
    assert_eq!(root_ctx.role, AdminRole::Superadmin);
    assert_eq!(root_ctx.auth_source, AdminAuthSource::Password);
    root_ctx.require(AdminPermission::AdminsManage).unwrap();
    assert!(matches!(
        svc.authenticate("root@example.com", "wrong-password-0123")
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    assert!(matches!(
        svc.authenticate("ghost@example.com", "ghost-password-0123")
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));

    let viewer = svc
        .create_admin(
            "viewer@example.com",
            "viewer-password-0123",
            "Viewer",
            AdminRole::Viewer,
        )
        .await
        .unwrap();
    let dup = svc
        .create_admin(
            "VIEWER@example.com",
            "viewer-password-0123",
            "Viewer",
            AdminRole::Viewer,
        )
        .await
        .unwrap_err();
    assert!(matches!(dup, AurixError::Conflict(_)), "{dup:?}");
    let (_, viewer_token) = svc
        .authenticate("viewer@example.com", "viewer-password-0123")
        .await
        .unwrap();
    let ctx = svc.authenticate_token(&viewer_token).await.unwrap();
    assert_eq!(ctx.role, AdminRole::Viewer);
    ctx.require(AdminPermission::AppsRead).unwrap();
    ctx.require(AdminPermission::NodesRead).unwrap();
    for denied in [
        AdminPermission::AuditRead,
        AdminPermission::AppsWrite,
        AdminPermission::KeysRotate,
        AdminPermission::AppsDelete,
        AdminPermission::AdminsManage,
    ] {
        assert!(matches!(
            ctx.require(denied).unwrap_err(),
            AurixError::AuthorizationDenied(_)
        ));
    }

    // --- role change: existing tokens die, the next one carries the current role ---------
    let updated = svc
        .update_admin(
            &root_ctx,
            viewer.id,
            AdminUpdate {
                role: Some(AdminRole::Moderator),
                display_name: Some(" Mod ".into()),
                active: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.role, "moderator");
    assert_eq!(updated.display_name, "Mod");
    assert!(updated.tokens_revoked_at.is_some());
    assert!(matches!(
        svc.authenticate_token(&viewer_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    let (_, mod_token) = svc
        .authenticate("viewer@example.com", "viewer-password-0123")
        .await
        .unwrap();
    let ctx = svc.authenticate_token(&mod_token).await.unwrap();
    assert_eq!(ctx.role, AdminRole::Moderator);
    ctx.require(AdminPermission::AuditRead).unwrap();
    assert!(ctx.require(AdminPermission::AppsWrite).is_err());

    // A display-name-only edit keeps tokens alive.
    svc.update_admin(
        &root_ctx,
        viewer.id,
        AdminUpdate {
            role: None,
            display_name: Some("Moderator".into()),
            active: None,
        },
    )
    .await
    .unwrap();
    svc.authenticate_token(&mod_token).await.unwrap();

    // --- guards ----------------------------------------------------------------------------
    let self_role = svc
        .update_admin(
            &root_ctx,
            root.id,
            AdminUpdate {
                role: Some(AdminRole::Admin),
                display_name: None,
                active: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(self_role, AurixError::Validation(_)),
        "{self_role:?}"
    );
    let self_off = svc
        .update_admin(
            &root_ctx,
            root.id,
            AdminUpdate {
                role: None,
                display_name: None,
                active: Some(false),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(self_off, AurixError::Validation(_)));
    // Two superadmins exist: root may demote ops ...
    svc.update_admin(
        &root_ctx,
        ops.id,
        AdminUpdate {
            role: Some(AdminRole::Admin),
            display_name: None,
            active: None,
        },
    )
    .await
    .unwrap();
    // ... after which root is the last one and nobody can demote or disable it.
    let (_, ops_token) = svc
        .authenticate("ops@example.com", "ops-password-0123456")
        .await
        .unwrap();
    let ops_ctx = svc.authenticate_token(&ops_token).await.unwrap();
    assert_eq!(ops_ctx.role, AdminRole::Admin);
    let promoted_back = svc
        .update_admin(
            &root_ctx,
            ops.id,
            AdminUpdate {
                role: Some(AdminRole::Superadmin),
                display_name: None,
                active: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(promoted_back.role, "superadmin");
    let (_, ops_token) = svc
        .authenticate("ops@example.com", "ops-password-0123456")
        .await
        .unwrap();
    let ops_ctx = svc.authenticate_token(&ops_token).await.unwrap();
    svc.update_admin(
        &ops_ctx,
        root.id,
        AdminUpdate {
            role: Some(AdminRole::Admin),
            display_name: None,
            active: None,
        },
    )
    .await
    .unwrap();
    let last = svc
        .update_admin(
            &root_ctx,
            ops.id,
            AdminUpdate {
                role: Some(AdminRole::Viewer),
                display_name: None,
                active: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(last, AurixError::Conflict(_)), "{last:?}");
    let last_off = svc
        .update_admin(
            &root_ctx,
            ops.id,
            AdminUpdate {
                role: None,
                display_name: None,
                active: Some(false),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(last_off, AurixError::Conflict(_)));
    // root's token now reflects the demotion (current role, not the claim).
    assert!(matches!(
        svc.authenticate_token(&root_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    let (_, root_token) = svc
        .authenticate("root@example.com", "root-password-0123")
        .await
        .unwrap();
    assert_eq!(
        svc.authenticate_token(&root_token).await.unwrap().role,
        AdminRole::Admin
    );
    svc.update_admin(
        &ops_ctx,
        root.id,
        AdminUpdate {
            role: Some(AdminRole::Superadmin),
            display_name: None,
            active: None,
        },
    )
    .await
    .unwrap();
    let (_, root_token) = svc
        .authenticate("root@example.com", "root-password-0123")
        .await
        .unwrap();
    let root_ctx = svc.authenticate_token(&root_token).await.unwrap();
    assert_eq!(root_ctx.role, AdminRole::Superadmin);

    // --- deactivate / reactivate -------------------------------------------------------------
    let off = svc
        .update_admin(
            &root_ctx,
            viewer.id,
            AdminUpdate {
                role: None,
                display_name: None,
                active: Some(false),
            },
        )
        .await
        .unwrap();
    assert!(!off.active);
    assert!(matches!(
        svc.authenticate_token(&mod_token).await.unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    assert!(matches!(
        svc.authenticate("viewer@example.com", "viewer-password-0123")
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    assert!(!svc.get_admin(viewer.id).await.unwrap().active);
    assert!(svc
        .list_admins()
        .await
        .unwrap()
        .iter()
        .any(|a| a.id == viewer.id));
    // The email is free again while the account is inactive ...
    let replacement = svc
        .create_admin(
            "viewer@example.com",
            "replacement-pass-0123",
            "Viewer 2",
            AdminRole::Viewer,
        )
        .await
        .unwrap();
    // ... so reactivating the old account would collide and is refused.
    let clash = svc
        .update_admin(
            &root_ctx,
            viewer.id,
            AdminUpdate {
                role: None,
                display_name: None,
                active: Some(true),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(clash, AurixError::Conflict(_)), "{clash:?}");
    svc.update_admin(
        &root_ctx,
        replacement.id,
        AdminUpdate {
            role: None,
            display_name: None,
            active: Some(false),
        },
    )
    .await
    .unwrap();
    let back = svc
        .update_admin(
            &root_ctx,
            viewer.id,
            AdminUpdate {
                role: None,
                display_name: None,
                active: Some(true),
            },
        )
        .await
        .unwrap();
    assert!(back.active);
    let (_, mod_token) = svc
        .authenticate("viewer@example.com", "viewer-password-0123")
        .await
        .unwrap();
    assert_eq!(
        svc.authenticate_token(&mod_token).await.unwrap().role,
        AdminRole::Moderator
    );
    assert!(matches!(
        svc.get_admin(Uuid::new_v4()).await.unwrap_err(),
        AurixError::NotFound(_)
    ));

    // --- passwords and logout-all -------------------------------------------------------------
    let wrong = svc
        .change_own_password(viewer.id, "not-the-password-0123", "new-viewer-pass-0123")
        .await
        .unwrap_err();
    assert!(matches!(wrong, AurixError::AuthenticationFailed(_)));
    let weak = svc
        .change_own_password(viewer.id, "viewer-password-0123", "short")
        .await
        .unwrap_err();
    assert!(matches!(weak, AurixError::Validation(_)));
    svc.change_own_password(viewer.id, "viewer-password-0123", "new-viewer-pass-0123")
        .await
        .unwrap();
    assert!(matches!(
        svc.authenticate_token(&mod_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    assert!(svc
        .authenticate("viewer@example.com", "viewer-password-0123")
        .await
        .is_err());
    let (_, mod_token) = svc
        .authenticate("viewer@example.com", "new-viewer-pass-0123")
        .await
        .unwrap();
    svc.authenticate_token(&mod_token).await.unwrap();

    svc.reset_password(viewer.id, "reset-by-root-pass-0123")
        .await
        .unwrap();
    assert!(matches!(
        svc.authenticate_token(&mod_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    let (_, mod_token) = svc
        .authenticate("viewer@example.com", "reset-by-root-pass-0123")
        .await
        .unwrap();
    svc.authenticate_token(&mod_token).await.unwrap();

    svc.revoke_tokens(viewer.id).await.unwrap();
    assert!(matches!(
        svc.authenticate_token(&mod_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    let (_, mod_token) = svc
        .authenticate("viewer@example.com", "reset-by-root-pass-0123")
        .await
        .unwrap();
    svc.authenticate_token(&mod_token).await.unwrap();
    assert!(matches!(
        svc.revoke_tokens(Uuid::new_v4()).await.unwrap_err(),
        AurixError::NotFound(_)
    ));

    // --- SSO: provisioning, role sync, binding ----------------------------------------------
    let (sso_admin, sso_token) = svc
        .login_sso(sso("sub-100", "Alice@Example.com", AdminRole::Admin))
        .await
        .unwrap();
    assert_eq!(sso_admin.email, "alice@example.com");
    assert_eq!(sso_admin.role, "admin");
    assert_eq!(sso_admin.auth_source, "oidc");
    assert_eq!(sso_admin.sso_issuer.as_deref(), Some(ISSUER));
    assert_eq!(sso_admin.sso_subject.as_deref(), Some("sub-100"));
    assert_eq!(sso_admin.password_hash, "!");
    let ctx = svc.authenticate_token(&sso_token).await.unwrap();
    assert_eq!(ctx.role, AdminRole::Admin);
    assert_eq!(ctx.auth_source, AdminAuthSource::Oidc);
    // SSO-only accounts have no password to log in or to change.
    assert!(matches!(
        svc.authenticate("alice@example.com", "!")
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    assert!(matches!(
        svc.change_own_password(sso_admin.id, "!", "some-new-password-0123")
            .await
            .unwrap_err(),
        AurixError::Validation(_)
    ));

    // The provider now says "moderator": with role sync the stored role follows and, like an
    // admin-made role change, tokens issued under the old role are revoked; the token minted
    // by this very login is newer than the bump and stays valid.
    let (synced, synced_token) = svc
        .login_sso(sso("sub-100", "alice@example.com", AdminRole::Moderator))
        .await
        .unwrap();
    assert_eq!(synced.role, "moderator");
    assert!(matches!(
        svc.authenticate_token(&sso_token).await.unwrap_err(),
        AurixError::TokenInvalid(_)
    ));
    assert_eq!(
        svc.authenticate_token(&synced_token).await.unwrap().role,
        AdminRole::Moderator
    );
    // Same role again: no revocation.
    let (_, again) = svc
        .login_sso(sso("sub-100", "alice@example.com", AdminRole::Moderator))
        .await
        .unwrap();
    svc.authenticate_token(&synced_token).await.unwrap();
    let sso_token = again;
    // Without sync the local role wins.
    let mut no_sync = sso("sub-100", "alice@example.com", AdminRole::Viewer);
    no_sync.sync_role = false;
    let (kept, _) = svc.login_sso(no_sync).await.unwrap();
    assert_eq!(kept.role, "moderator");
    // A changed provider email does not matter once the identity is bound.
    let (same, _) = svc
        .login_sso(sso(
            "sub-100",
            "alice.renamed@example.com",
            AdminRole::Moderator,
        ))
        .await
        .unwrap();
    assert_eq!(same.id, sso_admin.id);
    assert_eq!(same.email, "alice@example.com");

    // Binding a password account by email: keeps the password, adopts the SSO identity.
    let bound = svc
        .login_sso(sso("sub-viewer", "viewer@example.com", AdminRole::Viewer))
        .await
        .unwrap()
        .0;
    assert_eq!(bound.id, viewer.id);
    assert_eq!(
        bound.role, "viewer",
        "role synced from the provider on bind"
    );
    assert_eq!(bound.sso_subject.as_deref(), Some("sub-viewer"));
    assert_eq!(
        bound.auth_source, "oidc",
        "SSO becomes the primary identity"
    );
    assert_ne!(bound.password_hash, "!", "the password stays usable");
    let (_, pw_token) = svc
        .authenticate("viewer@example.com", "reset-by-root-pass-0123")
        .await
        .unwrap();
    let ctx = svc.authenticate_token(&pw_token).await.unwrap();
    assert_eq!(ctx.role, AdminRole::Viewer);
    assert_eq!(ctx.auth_source, AdminAuthSource::Password);
    // A different subject claiming the same email is refused.
    let hijack = svc
        .login_sso(sso("sub-evil", "viewer@example.com", AdminRole::Superadmin))
        .await
        .unwrap_err();
    assert!(
        matches!(hijack, AurixError::AuthenticationFailed(_)),
        "{hijack:?}"
    );

    // No auto-provisioning: unknown identities are refused, known ones still sign in.
    let mut strict = sso("sub-200", "bob@example.com", AdminRole::Viewer);
    strict.auto_provision = false;
    assert!(matches!(
        svc.login_sso(strict).await.unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    let mut known = sso("sub-100", "alice@example.com", AdminRole::Moderator);
    known.auto_provision = false;
    svc.login_sso(known).await.unwrap();

    // Deactivated SSO accounts cannot sign in; a password set by a superadmin makes the
    // account usable with a password too (once reactivated).
    svc.update_admin(
        &root_ctx,
        sso_admin.id,
        AdminUpdate {
            role: None,
            display_name: None,
            active: Some(false),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        svc.login_sso(sso("sub-100", "alice@example.com", AdminRole::Moderator))
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    assert!(matches!(
        svc.authenticate_token(&sso_token).await.unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    svc.update_admin(
        &root_ctx,
        sso_admin.id,
        AdminUpdate {
            role: None,
            display_name: None,
            active: Some(true),
        },
    )
    .await
    .unwrap();
    svc.reset_password(sso_admin.id, "alice-now-has-a-pass-0123")
        .await
        .unwrap();
    let (_, alice_pw) = svc
        .authenticate("alice@example.com", "alice-now-has-a-pass-0123")
        .await
        .unwrap();
    assert_eq!(
        svc.authenticate_token(&alice_pw).await.unwrap().auth_source,
        AdminAuthSource::Password
    );

    // --- password login disabled -------------------------------------------------------------
    let sso_only = AdminAuthService::new(pool.clone(), SECRET.into()).with_password_login(false);
    assert!(matches!(
        sso_only
            .authenticate("root@example.com", "root-password-0123")
            .await
            .unwrap_err(),
        AurixError::AuthenticationFailed(_)
    ));
    let (_, token) = sso_only
        .login_sso(sso("sub-100", "alice@example.com", AdminRole::Moderator))
        .await
        .unwrap();
    assert_eq!(
        sso_only.authenticate_token(&token).await.unwrap().role,
        AdminRole::Moderator
    );
    // Tokens are interchangeable between services sharing the secret and database.
    svc.authenticate_token(&token).await.unwrap();

    // --- SSO role sync never strips the last active superadmin -------------------------------
    let (root_sso, _) = svc
        .login_sso(sso("sub-root", "root@example.com", AdminRole::Superadmin))
        .await
        .unwrap();
    assert_eq!(root_sso.id, root.id, "bound by email");
    assert_eq!(root_sso.role, "superadmin");
    for other in svc.list_admins().await.unwrap() {
        if other.id != root.id && other.active && other.role == "superadmin" {
            svc.update_admin(
                &root_ctx,
                other.id,
                AdminUpdate {
                    role: Some(AdminRole::Admin),
                    display_name: None,
                    active: None,
                },
            )
            .await
            .unwrap();
        }
    }
    let (still_root, _) = svc
        .login_sso(sso("sub-root", "root@example.com", AdminRole::Viewer))
        .await
        .unwrap();
    assert_eq!(
        still_root.role, "superadmin",
        "a provider group change must not lock everyone out of admin management"
    );
    svc.update_admin(
        &root_ctx,
        ops.id,
        AdminUpdate {
            role: Some(AdminRole::Superadmin),
            display_name: None,
            active: None,
        },
    )
    .await
    .unwrap();
    let (demoted_root, _) = svc
        .login_sso(sso("sub-root", "root@example.com", AdminRole::Viewer))
        .await
        .unwrap();
    assert_eq!(
        demoted_root.role, "viewer",
        "with another superadmin present the sync applies"
    );
}

//! The authorization macros, compiled and run the way an application uses them.
//!
//! Everything here goes through the `moso` facade, because that is the only
//! path a user has: `permissions!`, `roles!`, `#[requires]` and `#[public]` all
//! emit `::moso::__private::*` (decision D6), so a test that named
//! `moso_authz` directly would prove nothing about the facade's re-exports —
//! which is exactly the half that breaks silently.

#![allow(missing_docs)]

use moso_authz::{
    AuthzDeclaration, PermSet, Permission, PermissionRegistry, RequireMode, Requirement, RoleSet,
};
use moso_core::extract::Extract;
use moso_openapi::OperationBuilder;
use moso_schema::json_schema::SchemaGenerator;

// ---------------------------------------------------------------------------
// The registry, exactly as `docs/03-batteries/31-authorization.md` writes it
// ---------------------------------------------------------------------------

moso::permissions! {
    /// Posts
    posts.read      = "View posts",
    posts.create    = "Create posts",
    posts.update    = "Edit posts",
    posts.delete    = "Delete posts",
    posts.publish   = "Publish posts",

    /// Users
    users.read      = "View users",
    users.invite    = "Invite users",
    users.suspend   = "Suspend users",

    /// Administration
    admin.access    = "Access the admin panel",
    admin.settings  = "Change organisation settings",
}

moso::roles! {
    /// Read-only access.
    Viewer  = [posts.read, users.read],
    /// Writes and edits posts.
    Editor  = Viewer + [posts.create, posts.update],
    /// Runs the organisation.
    Admin   = Editor + [posts.publish, posts.delete, users.invite, users.suspend, admin.access],
    /// Owns the organisation.
    Owner   = Admin + [admin.settings],
}

// ---------------------------------------------------------------------------
// `permissions!`
// ---------------------------------------------------------------------------

#[test]
fn a_dotted_declaration_becomes_an_upper_camel_variant() {
    assert_eq!(Perm::PostsRead.as_str(), "posts.read");
    assert_eq!(Perm::AdminSettings.as_str(), "admin.settings");
    assert_eq!(Perm::PostsPublish.description(), "Publish posts");
    assert_eq!(Perm::AdminAccess.group(), "admin");
}

/// The inherent constructors are `const`, which is what a `match` arm and a
/// `#[requires]` expansion both need.
#[test]
fn the_accessors_are_const() {
    const NAME: &str = Perm::PostsPublish.as_str();
    const DESCRIPTION: &str = Perm::PostsPublish.description();
    const GROUP: &str = Perm::PostsPublish.group();

    assert_eq!(
        (NAME, DESCRIPTION, GROUP),
        ("posts.publish", "Publish posts", "posts")
    );
}

/// Declaration order is bit order, which is what makes a stored `PermSet` mean
/// the same thing after a deploy that only *appends*.
#[test]
fn declaration_order_is_bit_order() {
    for (position, permission) in Perm::ALL.iter().copied().enumerate() {
        assert_eq!(permission as usize, position);
        assert_eq!(Permission::index(permission) as usize, position);
        assert_eq!(Perm::from_index(position as u16), Some(permission));
    }
    assert_eq!(Perm::ALL.len(), 10);
    assert_eq!(<Perm as Permission>::count(), 10);
}

#[test]
fn wire_names_round_trip_and_an_unknown_one_is_none() {
    for permission in Perm::ALL.iter().copied() {
        assert_eq!(Perm::parse(permission.as_str()), Some(permission));
    }
    assert_eq!(Perm::parse("posts.pubish"), None);
    assert_eq!(Perm::PostsRead.to_string(), "posts.read");
}

#[test]
fn the_generated_fingerprint_is_the_registry_it_describes() {
    assert_eq!(
        <Perm as Permission>::FINGERPRINT,
        moso_authz::perm::fingerprint_of(Perm::NAMES),
    );
    assert_ne!(
        <Perm as Permission>::FINGERPRINT,
        moso_authz::perm::fingerprint_of(&["posts.read"]),
    );
}

/// Design goal 2: the whole set is knowable at boot, so the admin can render it
/// and an audit can list it.
#[test]
fn the_generated_registry_is_enumerable_at_boot() {
    let registry = PermissionRegistry::of::<Perm>();

    assert_eq!(registry.all().len(), 10);
    assert_eq!(registry.groups(), ["posts", "users", "admin"]);
    assert_eq!(registry.in_group("posts").len(), 5);
    assert_eq!(
        registry
            .lookup("admin.access")
            .map(|entry| entry.description().to_owned()),
        Some("Access the admin panel".to_owned()),
    );
    assert_eq!(registry.suggest("posts.pubish"), Some("posts.publish"));
}

// ---------------------------------------------------------------------------
// `roles!`
// ---------------------------------------------------------------------------

/// Inheritance is flattened by the macro, so this is a constant comparison and
/// not a graph walk.
#[test]
fn inheritance_is_flattened_at_expansion_time() {
    const VIEWER: PermSet<Perm> = Role::Viewer.permissions();

    assert_eq!(VIEWER, PermSet::of([Perm::PostsRead, Perm::UsersRead]));

    assert_eq!(
        Role::Editor.permissions(),
        PermSet::of([
            Perm::PostsRead,
            Perm::UsersRead,
            Perm::PostsCreate,
            Perm::PostsUpdate,
        ]),
    );

    // Admin inherits everything Editor has, and adds five.
    assert!(
        Role::Admin
            .permissions()
            .has_all(Role::Editor.permissions())
    );
    assert_eq!(Role::Admin.permissions().len(), 9);

    // Owner is Admin plus one.
    assert!(Role::Owner.permissions().has_all(Role::Admin.permissions()));
    assert_eq!(Role::Owner.permissions(), PermSet::all());
}

#[test]
fn a_role_carries_its_wire_name_and_its_doc_comment() {
    assert_eq!(Role::Editor.as_str(), "editor");
    assert_eq!(Role::Editor.description(), "Writes and edits posts.");
    assert_eq!(Role::parse("owner"), Some(Role::Owner));
    assert_eq!(Role::parse("sorcerer"), None);
    assert_eq!(Role::Admin.to_string(), "admin");
}

#[test]
fn a_role_set_unions_what_its_roles_grant() {
    let held = RoleSet::of([Role::Viewer, Role::Editor]);

    assert_eq!(held.permissions(), Role::Editor.permissions());
    assert_eq!(held.names(), ["viewer", "editor"]);
    assert_eq!(<Role as moso_authz::Role>::ALL.len(), 4);
    assert_eq!(<Role as moso_authz::Role>::from_index(3), Some(Role::Owner));
}

// ---------------------------------------------------------------------------
// The actor built from the generated registries
// ---------------------------------------------------------------------------

fn actor(id: &str, role: Role) -> moso_authz::Actor<Role> {
    moso_authz::Actor::new(
        moso_authz::ActorId::new(id),
        moso_authz::ActorKind::User,
        moso_authz::Scope::Global,
        RoleSet::of([role]),
    )
}

#[test]
fn an_actor_answers_with_the_generated_permissions() {
    let editor = actor("usr_1", Role::Editor);

    assert!(editor.has(Perm::PostsUpdate));
    assert!(!editor.has(Perm::AdminAccess));
    assert!(editor.is(Role::Editor));
    assert!(editor.has_all(PermSet::of([Perm::PostsRead, Perm::PostsCreate])));
}

// ---------------------------------------------------------------------------
// `#[requires]` and `#[public]`
// ---------------------------------------------------------------------------

moso_authz::actions! {
    for Role;
    /// Making a draft public.
    Publish = "publish",
}

/// A post, as the API returns one.
#[derive(moso::Schema)]
pub struct PostOut {
    /// Stable identifier.
    pub id: i64,
}

/// Create a post.
#[moso::requires(Perm::PostsCreate)]
#[moso::endpoint]
pub async fn create() -> moso::Result<moso::response::Created<PostOut>> {
    Ok(moso::response::Created::at("/posts/1", PostOut { id: 1 }))
}

/// Suspend a user, and record that somebody did.
#[moso::requires(Perm::UsersSuspend, audit)]
#[moso::endpoint]
pub async fn suspend() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// Read a post, for anybody who can read *or* administer.
#[moso::requires(any(Perm::PostsRead, Perm::AdminAccess))]
#[moso::endpoint]
pub async fn show() -> moso::Result<moso::extract::Json<PostOut>> {
    Ok(moso::extract::Json(PostOut { id: 1 }))
}

/// Publish a post, named by its wire name rather than its variant.
#[moso::requires("posts.publish")]
#[moso::endpoint]
pub async fn publish() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// Liveness. Deliberately open.
#[moso::public]
#[moso::endpoint]
pub async fn healthz() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// An endpoint that declares nothing, which is the finding
/// `moso check --authz` exists to report.
#[moso::endpoint]
pub async fn undeclared() -> moso::Result<moso::response::NoContent> {
    Ok(moso::response::NoContent)
}

/// The operation an endpoint describes at boot.
fn spec_of<E: moso::Endpoint>() -> moso_openapi::OperationSpec {
    let mut op = OperationBuilder::new(SchemaGenerator::default());
    E::spec(&mut op);
    op.into_spec()
}

#[test]
fn requires_declares_the_permission_and_documents_the_403() {
    let spec = spec_of::<__moso_op_create>();

    assert_eq!(
        moso_authz::read_declarations(&spec),
        vec![AuthzDeclaration::Permissions {
            names: vec!["posts.create".to_owned()],
            all: true,
        }],
    );

    let forbidden = spec.responses["403"]
        .description
        .clone()
        .expect("a described 403");
    assert!(forbidden.contains("posts.create"), "{forbidden}");
    assert!(forbidden.contains("Create posts"), "{forbidden}");
    assert!(spec.has_response("401"));
    assert!(moso_authz::boot_problems(&spec).is_empty());
}

#[test]
fn requires_declares_the_permission_source_it_needs() {
    let required = <__moso_op_create as moso::Endpoint>::required_providers();

    assert!(
        required
            .iter()
            .any(|req| req.name().contains("PermissionSource")),
        "{:?}",
        required.iter().map(|req| req.name()).collect::<Vec<_>>(),
    );
}

#[test]
fn the_any_form_says_so_in_the_document() {
    let spec = spec_of::<__moso_op_show>();
    let forbidden = spec.responses["403"]
        .description
        .clone()
        .expect("described");

    assert!(forbidden.contains("at least one of"), "{forbidden}");
    assert_eq!(
        moso_authz::read_declarations(&spec),
        vec![AuthzDeclaration::Permissions {
            names: vec!["posts.read".to_owned(), "admin.access".to_owned()],
            all: false,
        }],
    );
}

#[test]
fn the_string_form_is_checked_against_the_registry_at_boot() {
    let spec = spec_of::<__moso_op_publish>();

    assert_eq!(
        moso_authz::read_declarations(&spec),
        vec![AuthzDeclaration::Permissions {
            names: vec!["posts.publish".to_owned()],
            all: true,
        }],
    );
    assert!(moso_authz::boot_problems(&spec).is_empty());
}

#[test]
fn public_declares_the_endpoint_open_and_overrides_the_document_security() {
    let spec = spec_of::<__moso_op_healthz>();

    assert_eq!(
        moso_authz::read_declarations(&spec),
        vec![AuthzDeclaration::Public],
    );
    assert_eq!(spec.security.as_deref(), Some(&[][..]));
}

/// Acceptance criterion 2, end to end: the endpoint that declared nothing is
/// the one `moso check --authz` reports.
#[test]
fn an_endpoint_that_declares_nothing_is_distinguishable_from_one_declared_open() {
    assert!(moso_authz::read_declarations(&spec_of::<__moso_op_undeclared>()).is_empty());
    assert!(!moso_authz::read_declarations(&spec_of::<__moso_op_healthz>()).is_empty());
}

/// The `audit` flag reaches the generated declaration, which is what makes
/// `#[requires(.., audit)]` record the *allows* as well as the denials.
#[test]
fn the_audit_flag_reaches_the_generated_requirement() {
    let spec = spec_of::<__moso_op_suspend>();

    assert_eq!(
        moso_authz::read_declarations(&spec),
        vec![AuthzDeclaration::Permissions {
            names: vec!["users.suspend".to_owned()],
            all: true,
        }],
    );
    const { assert!(<__moso_authz_suspend as Requirement>::AUDIT) };
    const { assert!(!<__moso_authz_create as Requirement>::AUDIT) };
    assert_eq!(<__moso_authz_show as Requirement>::MODE, RequireMode::Any,);
    assert_eq!(
        <__moso_authz_create as Requirement>::resolve().0,
        PermSet::of([Perm::PostsCreate]),
    );
}

/// The routes register, which is the other half of "the macro output composes":
/// a `#[requires]` that broke the signature would fail here rather than in a
/// user's application.
#[test]
fn the_endpoints_register_in_a_router() {
    let table = moso::routes! {
        POST   "/posts"           => create,
        GET    "/posts/{id}"      => show,
        POST   "/posts/{id}/publish" => publish,
        POST   "/users/{id}/suspend" => suspend,
        GET    "/healthz"         => healthz,
        GET    "/undeclared"      => undeclared,
    };

    assert_eq!(table.len(), 6);
}

/// A generated `Required<..>` is an `Extract`, which is what puts it in a
/// handler signature at all.
#[test]
fn the_generated_requirement_is_an_extractor() {
    fn assert<E: Extract>() {}
    assert::<moso_authz::Required<__moso_authz_create>>();
    assert::<moso_authz::Public>();
}

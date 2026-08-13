//! The frozen surface, exercised the way an application will exercise it.
//!
//! This is an *integration* test on purpose: it compiles from outside the crate,
//! so the orphan rule applies exactly as it will for a user. That matters more
//! here than anywhere else in the workspace, because the central ergonomic claim
//! of `moso-authz` — "write `impl Policy<Edit, Post> for Actor<Role>` in your own
//! crate" — is a coherence question, and a unit test inside the crate would
//! prove nothing about it.
//!
//! What it proves is that the signatures compose the way an application will
//! compose them: the policy impls are legal, `authorized_for::<Read>` takes one
//! turbofish, `Requires` is a `Guard`, and every documented spelling of
//! `Authorized` resolves.

#![allow(dead_code, missing_docs)]

use moso_authz::perm::fingerprint_of;
use moso_authz::{
    Actor, ActorId, ActorKind, AuthorizedQuery, Decision, HasRole, Masked, PermSet, Permission,
    Policy, PolicyCtx, RequireMode, Required, Requirement, RoleSet, Scope, ScopedPolicy, TraceStep,
};
use moso_orm::descriptor::EntityDescriptor;
use moso_orm::{ColumnDef, DecodeError, Entity, Row, Select};
use moso_sql::{TableRef, ValueKind};

// ---------------------------------------------------------------------------
// What `moso::permissions!` generates
// ---------------------------------------------------------------------------

/// The shape of the enum `permissions!` emits, written by hand so the macro has
/// a target to match.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum Perm {
    PostsRead,
    PostsPublish,
    AdminAccess,
}

impl Perm {
    pub const ALL: &'static [Perm] = &[Perm::PostsRead, Perm::PostsPublish, Perm::AdminAccess];

    const NAMES: &'static [&'static str] = &["posts.read", "posts.publish", "admin.access"];

    /// Inherent and `const`, which is what a `match` arm in user code wants.
    /// The trait method delegates to it.
    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::PostsRead => "View posts",
            Self::PostsPublish => "Publish posts",
            Self::AdminAccess => "Access the admin panel",
        }
    }

    pub const fn group(self) -> &'static str {
        match self {
            Self::PostsRead | Self::PostsPublish => "posts",
            Self::AdminAccess => "admin",
        }
    }
}

// The cap check `permissions!` emits alongside the enum.
const _: () = assert!(Perm::ALL.len() <= moso_authz::MAX_PERMISSIONS);

impl Permission for Perm {
    const ALL: &'static [Self] = Perm::ALL;
    const FINGERPRINT: u64 = fingerprint_of(Perm::NAMES);

    fn index(self) -> u16 {
        self as u16
    }

    fn from_index(index: u16) -> Option<Self> {
        Perm::ALL.get(index as usize).copied()
    }

    fn as_str(self) -> &'static str {
        Perm::as_str(self)
    }

    fn description(self) -> &'static str {
        Perm::description(self)
    }

    fn group(self) -> &'static str {
        Perm::group(self)
    }

    fn parse(name: &str) -> Option<Self> {
        Perm::ALL.iter().copied().find(|p| Perm::as_str(*p) == name)
    }
}

// ---------------------------------------------------------------------------
// What `moso::roles!` generates
// ---------------------------------------------------------------------------

/// The shape of the enum `roles!` emits. Inheritance is flattened by the macro,
/// which is why `permissions` can be a plain constructor and not a graph walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    Viewer,
    Editor,
    Admin,
}

impl Role {
    pub const ALL: &'static [Role] = &[Role::Viewer, Role::Editor, Role::Admin];
}

const _: () = assert!(Role::ALL.len() <= moso_authz::MAX_ROLES);

impl moso_authz::Role for Role {
    type Perm = Perm;

    const ALL: &'static [Self] = Role::ALL;

    fn index(self) -> u8 {
        self as u8
    }

    fn from_index(index: u8) -> Option<Self> {
        Role::ALL.get(index as usize).copied()
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Editor => "editor",
            Self::Admin => "admin",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Viewer => "Read-only",
            Self::Editor => "Writes and publishes",
            Self::Admin => "Everything",
        }
    }

    fn permissions(self) -> PermSet<Perm> {
        match self {
            Self::Viewer => PermSet::of([Perm::PostsRead]),
            Self::Editor => PermSet::of([Perm::PostsRead, Perm::PostsPublish]),
            Self::Admin => PermSet::all(),
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Role::ALL
            .iter()
            .copied()
            .find(|r| moso_authz::Role::as_str(*r) == name)
    }
}

// ---------------------------------------------------------------------------
// The application's actions and resources
// ---------------------------------------------------------------------------

moso_authz::actions! {
    for Role;
    /// Listing posts.
    Read = "read",
    /// Making a draft public.
    Publish = "publish",
}

moso_authz::path_name!(
    /// The `{post_id}` segment, for `Authorized<_, _, FromPath<PostId>>`.
    PostId = "post_id"
);

/// A post row, as the ORM sees it.
pub struct Post {
    pub id: i64,
    pub author_id: String,
    pub published: bool,
}

impl Entity for Post {
    type Pk = i64;

    const TABLE: TableRef = TableRef::from_static("posts");
    const COLUMNS: &'static [ColumnDef] = &[
        ColumnDef::new("id", ValueKind::I64).primary_key(),
        ColumnDef::new("author_id", ValueKind::Text),
        ColumnDef::new("published", ValueKind::Bool),
    ];
    const NAME: &'static str = "Post";

    fn pk(&self) -> i64 {
        self.id
    }

    fn from_row(row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            id: row.get_i64(0)?,
            author_id: row.get_string(1)?,
            published: row.get_bool(2)?,
        })
    }

    fn descriptor() -> &'static EntityDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<EntityDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
    }
}

// ---------------------------------------------------------------------------
// The policies — the coherence claim this file exists to check
// ---------------------------------------------------------------------------

// `Policy` and `Actor` are both foreign; `Publish` and `Post` are local. The
// orphan rule allows this *because* the role parameter is concrete: writing
// `impl<R: moso_authz::Role> Policy<Publish, Post> for Actor<R>` would be
// rejected, since `R` is an uncovered parameter in a foreign type ahead of the
// first local one. That is why `roles!` generates a concrete `Role` and why the
// documentation shows `Actor<Role>` rather than a generic.
impl Policy<Publish, Post> for Actor<Role> {
    async fn allows(&self, _: Publish, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if self.has(Perm::PostsPublish) && post.author_id == self.id().as_str() {
            return Decision::allow("author").with_step(TraceStep::new("author", true));
        }
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override");
        }
        Decision::deny("not the author and not an admin")
    }
}

impl ScopedPolicy<Read, Post> for Actor<Role> {
    fn scope_query(&self, query: Select<Post>) -> Select<Post> {
        // An admin sees everything; everybody else sees their own. Returning the
        // query unchanged is enough to prove the signature composes — the filter
        // itself is the implementation agent's.
        if self.has(Perm::AdminAccess) {
            return query;
        }
        query
    }
}

// ---------------------------------------------------------------------------
// Compile-only exercises of the frozen signatures
// ---------------------------------------------------------------------------

/// `authorized_for::<Read>` takes exactly one turbofish argument and infers the
/// rest. This is the reason the actor arrives as `&dyn ScopedPolicy` — Rust has
/// no partial turbofish, so a second type parameter here would make the call
/// unwritable.
fn query_level_filtering_composes(actor: &Actor<Role>) -> Select<Post> {
    Select::<Post>::new().authorized_for::<Read>(actor)
}

/// The imperative form, and the shape of the `where` clause a generic caller
/// needs.
async fn imperative_check(actor: &Actor<Role>, post: &Post) -> bool {
    actor.can(Publish, post).await.allowed()
}

/// `Requires<P>` is the guard `#[requires(..)]` expands to, and it has to be a
/// `moso_core::Guard` — that is what makes the permission appear in the OpenAPI
/// document instead of being an undocumented 403.
fn requires_is_a_guard() -> impl moso_core::Guard {
    moso_authz::Requires::new(PermSet::of([Perm::PostsPublish])).audited()
}

/// Every extractor spelling in the documentation resolves.
fn the_extractor_spellings_resolve(
    _default_source: moso_authz::Authorized<Publish, Post>,
    _named_source: moso_authz::Authorized<Publish, Post, moso_authz::FromPath<PostId>>,
    _masked: moso_authz::Authorized<Publish, Post, Masked<moso_authz::FromPathId>>,
) {
}

/// Every one of them is an `Extract`, which is what makes it usable as a
/// handler parameter — and what makes the 403 appear in the document.
fn the_extractor_spellings_are_extractors() {
    fn assert<E: moso_core::extract::Extract>() {}

    assert::<moso_authz::Authorized<Publish, Post>>();
    assert::<moso_authz::Authorized<Publish, Post, moso_authz::FromPath<PostId>>>();
    assert::<moso_authz::Authorized<Publish, Post, Masked<moso_authz::FromPathId>>>();
    assert::<moso_authz::Public>();
    assert::<Required<MayPublish>>();
}

/// What `#[requires(Perm::PostsPublish)]` generates beside the handler, written
/// by hand so the macro has a target to match.
pub struct MayPublish;

impl Requirement for MayPublish {
    type Perm = Perm;
    const NAMES: &'static [&'static str] = &[Perm::PostsPublish.as_str()];
    const MODE: RequireMode = RequireMode::All;
    const AUDIT: bool = false;
}

/// The role registry an action names is the one `Authorized` resolves through.
/// Without it the extractor would need a fourth type parameter and the
/// documented `Authorized<Publish, Post>` would be unwritable.
#[test]
fn an_action_names_the_role_registry_that_answers_for_it() {
    fn role_of<A: HasRole>() -> &'static str {
        <A::Role as moso_authz::Role>::ALL
            .first()
            .map(|role| moso_authz::Role::as_str(*role))
            .unwrap_or_default()
    }

    assert_eq!(role_of::<Publish>(), "viewer");
    assert_eq!(role_of::<Read>(), "viewer");
}

/// The `#[requires]` declaration resolves against the registry it names.
#[test]
fn a_requirement_resolves_against_its_own_registry() {
    let (set, unknown) = MayPublish::resolve();

    assert_eq!(set, PermSet::of([Perm::PostsPublish]));
    assert!(unknown.is_empty());
}

/// `Actor<Role>` is what a handler holds across an `.await`, so it has to be
/// `Send + Sync`, and it has to be `Clone` to be a `Dependency`.
fn actor_is_send_sync_and_clone() {
    fn assert<T: Send + Sync + Clone + 'static>() {}
    assert::<Actor<Role>>();
}

/// No user-visible type may print longer than 80 characters (non-negotiable N1's
/// sibling rule from the diagnostics style guide). The longest spelling in this
/// crate is the three-parameter `Authorized`.
#[test]
fn no_user_visible_type_is_longer_than_eighty_characters() {
    let longest = "Authorized<Publish, Post, FromPath<PostId>>";
    assert!(
        longest.len() <= 80,
        "`{longest}` is {} characters",
        longest.len()
    );
}

/// The permission registry's identity is its names *in order*, and the
/// fingerprint has to separate every way two registries can differ — that is
/// what makes `PermBits` safe to send across a process boundary.
#[test]
fn the_generated_fingerprint_is_stable_and_discriminating() {
    assert_eq!(
        <Perm as Permission>::FINGERPRINT,
        fingerprint_of(&["posts.read", "posts.publish", "admin.access"]),
    );
    assert_ne!(
        <Perm as Permission>::FINGERPRINT,
        fingerprint_of(&["posts.publish", "posts.read", "admin.access"]),
    );
    assert_ne!(
        <Perm as Permission>::FINGERPRINT,
        fingerprint_of(&["posts.read", "posts.publish"]),
    );
}

/// The generated enum's indices are its bit positions, and `from_index` is the
/// exact inverse. A registry where those disagree would make every stored
/// `PermBits` mean something else.
#[test]
fn indices_round_trip_through_from_index() {
    for (position, permission) in <Perm as Permission>::ALL.iter().copied().enumerate() {
        assert_eq!(permission.index() as usize, position);
        assert_eq!(Perm::from_index(permission.index()), Some(permission));
    }
    assert_eq!(Perm::from_index(9_999), None);
    assert_eq!(<Perm as Permission>::count(), 3);
}

/// Wire names round-trip, which is what a database column and an API key's
/// scope list depend on.
#[test]
fn wire_names_round_trip() {
    for permission in <Perm as Permission>::ALL.iter().copied() {
        let name = Permission::as_str(permission);
        assert_eq!(Perm::parse(name), Some(permission));
        assert!(
            name.starts_with(Permission::group(permission)),
            "`{name}` should begin with its group",
        );
    }
    assert_eq!(Perm::parse("posts.pubish"), None);
}

/// The role registry round-trips the same way.
#[test]
fn role_names_round_trip() {
    for role in <Role as moso_authz::Role>::ALL.iter().copied() {
        let name = moso_authz::Role::as_str(role);
        assert_eq!(<Role as moso_authz::Role>::parse(name), Some(role));
        assert_eq!(
            <Role as moso_authz::Role>::from_index(moso_authz::Role::index(role)),
            Some(role),
        );
    }
}

/// `Scope` is what multi-tenancy hangs off, so its identity has to be exact:
/// two organisations with the same identifier string are the same scope, and an
/// organisation and a project with that identifier are not.
#[test]
fn scopes_compare_by_kind_as_well_as_identifier() {
    use moso_authz::ScopeId;

    assert_eq!(
        Scope::Org(ScopeId::new("acme")),
        Scope::Org(ScopeId::new("acme")),
    );
    assert_ne!(
        Scope::Org(ScopeId::new("acme")),
        Scope::Project(ScopeId::new("acme")),
    );
    assert_ne!(Scope::Global, Scope::Org(ScopeId::new("acme")));
    assert!(Scope::Global.is_global());
    assert!(Scope::Global.id().is_none());
    assert_eq!(
        Scope::Org(ScopeId::new("acme")).id().map(ScopeId::as_str),
        Some("acme"),
    );
}

/// An anonymous actor holds nothing, and `ActorKind` says so — the deny-by-default
/// starting point.
#[test]
fn the_anonymous_actor_is_the_deny_by_default_starting_point() {
    assert!(!ActorKind::Anonymous.is_authenticated());
    assert!(ActorId::anonymous().is_anonymous());
    assert!(!ActorId::new("usr_1").is_anonymous());
}

/// The empty sets are `const`-constructible, which is what lets a generated
/// `Role::permissions` be a constant and role resolution be free.
#[test]
fn the_empty_sets_are_const_constructible() {
    const NO_PERMISSIONS: PermSet<Perm> = PermSet::empty();
    const NO_ROLES: RoleSet<Role> = RoleSet::empty();

    assert_eq!(NO_PERMISSIONS, PermSet::<Perm>::default());
    assert_eq!(NO_ROLES, RoleSet::<Role>::default());
    assert!(NO_ROLES.is_empty());
}

//! The registry every unit test in this crate authorises against.
//!
//! It is the example from `docs/03-batteries/31-authorization.md`, written out
//! by hand: ten permissions in three groups, four roles with the documented
//! inheritance, and a `Post` entity to be a resource. Writing it by hand rather
//! than invoking `moso::permissions!` is deliberate — this crate cannot depend
//! on the facade the macro emits paths against, and a hand-written registry is
//! also the thing the macro's output has to match, so keeping one here means
//! the runtime is testable without the macro and the macro has a target.
//!
//! `crates/moso-authz-tests` compiles the *macro's* version of the same
//! registry and asserts the two agree.

#![allow(missing_docs)]

use crate::perm::fingerprint_of;
use crate::{PermSet, Permission};

// ---------------------------------------------------------------------------
// Permissions — what `moso::permissions!` generates
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum Perm {
    PostsRead,
    PostsCreate,
    PostsUpdate,
    PostsDelete,
    PostsPublish,
    UsersRead,
    UsersInvite,
    UsersSuspend,
    AdminAccess,
    AdminSettings,
}

impl Perm {
    pub const ALL: &'static [Perm] = &[
        Perm::PostsRead,
        Perm::PostsCreate,
        Perm::PostsUpdate,
        Perm::PostsDelete,
        Perm::PostsPublish,
        Perm::UsersRead,
        Perm::UsersInvite,
        Perm::UsersSuspend,
        Perm::AdminAccess,
        Perm::AdminSettings,
    ];

    pub const NAMES: &'static [&'static str] = &[
        "posts.read",
        "posts.create",
        "posts.update",
        "posts.delete",
        "posts.publish",
        "users.read",
        "users.invite",
        "users.suspend",
        "admin.access",
        "admin.settings",
    ];

    const DESCRIPTIONS: &'static [&'static str] = &[
        "View posts",
        "Create posts",
        "Edit posts",
        "Delete posts",
        "Publish posts",
        "View users",
        "Invite users",
        "Suspend users",
        "Access the admin panel",
        "Change organisation settings",
    ];

    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub const fn description(self) -> &'static str {
        Self::DESCRIPTIONS[self as usize]
    }

    pub const fn group(self) -> &'static str {
        match self {
            Self::PostsRead
            | Self::PostsCreate
            | Self::PostsUpdate
            | Self::PostsDelete
            | Self::PostsPublish => "posts",
            Self::UsersRead | Self::UsersInvite | Self::UsersSuspend => "users",
            Self::AdminAccess | Self::AdminSettings => "admin",
        }
    }
}

const _: () = assert!(Perm::ALL.len() <= crate::MAX_PERMISSIONS);

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
// Roles — what `moso::roles!` generates, with inheritance already flattened
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Role {
    Viewer,
    Editor,
    Admin,
    Owner,
}

impl Role {
    pub const ALL: &'static [Role] = &[Role::Viewer, Role::Editor, Role::Admin, Role::Owner];

    /// `Viewer = [posts.read, users.read]`.
    const VIEWER: PermSet<Perm> = PermSet::from_words([0b0010_0001, 0, 0, 0]);
    /// `Editor = Viewer + [posts.create, posts.update]`.
    const EDITOR: PermSet<Perm> = PermSet::from_words([0b0010_0111, 0, 0, 0]);
    /// `Admin = Editor + [posts.publish, posts.delete, users.invite,
    /// users.suspend, admin.access]`.
    const ADMIN: PermSet<Perm> = PermSet::from_words([0b1_1111_1111, 0, 0, 0]);
    /// `Owner = Admin + [admin.settings]`.
    const OWNER: PermSet<Perm> = PermSet::from_words([0b11_1111_1111, 0, 0, 0]);

    pub const fn permissions(self) -> PermSet<Perm> {
        match self {
            Self::Viewer => Self::VIEWER,
            Self::Editor => Self::EDITOR,
            Self::Admin => Self::ADMIN,
            Self::Owner => Self::OWNER,
        }
    }
}

const _: () = assert!(Role::ALL.len() <= crate::MAX_ROLES);

impl crate::Role for Role {
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
            Self::Owner => "owner",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Viewer => "Read-only access",
            Self::Editor => "Writes and edits posts",
            Self::Admin => "Runs the organisation",
            Self::Owner => "Owns the organisation",
        }
    }

    fn permissions(self) -> PermSet<Perm> {
        Role::permissions(self)
    }

    fn parse(name: &str) -> Option<Self> {
        Role::ALL
            .iter()
            .copied()
            .find(|role| crate::Role::as_str(*role) == name)
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

crate::actions! {
    for Role;
    /// Listing or showing.
    Read = "read",
    /// Changing.
    Edit = "edit",
    /// Making a draft public.
    Publish = "publish",
}

crate::path_name!(
    /// The `{post_id}` segment.
    PostId = "post_id"
);

// ---------------------------------------------------------------------------
// A resource
// ---------------------------------------------------------------------------

/// A post, as the ORM sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    pub id: i64,
    pub author_id: String,
    pub published: bool,
    pub title: String,
}

impl Post {
    /// `Post::AUTHOR_ID`, as `#[derive(Entity)]` would generate it.
    pub fn author_id() -> moso_orm::Column<Post, String> {
        moso_orm::Column::new("author_id")
    }

    /// `Post::PUBLISHED`.
    pub fn published() -> moso_orm::Column<Post, bool> {
        moso_orm::Column::new("published")
    }
}

impl moso_orm::Entity for Post {
    type Pk = i64;

    const TABLE: moso_sql::TableRef = moso_sql::TableRef::from_static("posts");
    const COLUMNS: &'static [moso_orm::ColumnDef] = &[
        moso_orm::ColumnDef::new("id", moso_sql::ValueKind::I64).primary_key(),
        moso_orm::ColumnDef::new("author_id", moso_sql::ValueKind::Text),
        moso_orm::ColumnDef::new("published", moso_sql::ValueKind::Bool),
        moso_orm::ColumnDef::new("title", moso_sql::ValueKind::Text),
    ];
    const NAME: &'static str = "Post";

    fn pk(&self) -> i64 {
        self.id
    }

    fn from_row(row: &moso_orm::Row) -> Result<Self, moso_orm::DecodeError> {
        Ok(Self {
            id: row.get_i64(0)?,
            author_id: row.get_string(1)?,
            published: row.get_bool(2)?,
            title: row.get_string(3)?,
        })
    }

    fn descriptor() -> &'static moso_orm::descriptor::EntityDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<moso_orm::descriptor::EntityDescriptor> =
            std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            moso_orm::descriptor::EntityDescriptor::builder("Post", Self::TABLE).build()
        })
    }
}

// ---------------------------------------------------------------------------
// The policies
// ---------------------------------------------------------------------------

impl crate::Policy<Publish, Post> for crate::Actor<Role> {
    async fn allows(&self, _: Publish, post: &Post, _ctx: &crate::PolicyCtx) -> crate::Decision {
        let author = post.author_id == self.id().as_str();
        if self.has(Perm::PostsPublish) && author {
            return crate::Decision::allow("author")
                .with_step(crate::TraceStep::new("has(posts.publish)", true))
                .with_step(crate::TraceStep::new("post.author_id == actor.id", true));
        }
        if self.has(Perm::AdminAccess) {
            return crate::Decision::allow("admin override")
                .with_step(crate::TraceStep::new("has(admin.access)", true));
        }
        crate::Decision::deny("not the author and not an admin")
            .with_step(crate::TraceStep::new(
                "has(posts.publish)",
                self.has(Perm::PostsPublish),
            ))
            .with_step(
                crate::TraceStep::new("post.author_id == actor.id", author).with_detail(format!(
                    "author={}, actor={}",
                    post.author_id,
                    self.id()
                )),
            )
            .with_step(crate::TraceStep::new("has(admin.access)", false))
    }
}

/// The policy written out in `docs/03-batteries/31-authorization.md`, verbatim.
impl crate::Policy<Edit, Post> for crate::Actor<Role> {
    async fn allows(&self, _: Edit, post: &Post, _ctx: &crate::PolicyCtx) -> crate::Decision {
        if self.has(Perm::PostsUpdate) && post.author_id == self.id().as_str() {
            return crate::Decision::allow("author");
        }
        if self.has(Perm::AdminAccess) {
            return crate::Decision::allow("admin override");
        }
        crate::Decision::deny("not the author and not an admin")
    }
}

impl crate::Policy<Read, Post> for crate::Actor<Role> {
    async fn allows(&self, _: Read, post: &Post, _ctx: &crate::PolicyCtx) -> crate::Decision {
        if post.published {
            // A peer may read it, but not the author's identity.
            let decision = crate::Decision::allow("published");
            if self.has(Perm::AdminAccess) {
                return decision;
            }
            return decision.with_obligation(crate::Obligation::redact("/author_id"));
        }
        if post.author_id == self.id().as_str() || self.has(Perm::AdminAccess) {
            return crate::Decision::allow("author or admin");
        }
        crate::Decision::deny("draft, and not the author")
    }
}

impl crate::ScopedPolicy<Read, Post> for crate::Actor<Role> {
    fn scope_query(&self, query: moso_orm::Select<Post>) -> moso_orm::Select<Post> {
        if self.has(Perm::AdminAccess) {
            return query;
        }
        query.filter(Post::published().eq(true) | Post::author_id().eq(self.id().as_str()))
    }
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

/// An actor holding exactly `roles`, globally.
pub fn actor(id: &str, roles: impl IntoIterator<Item = Role>) -> crate::Actor<Role> {
    crate::Actor::new(
        crate::ActorId::new(id),
        crate::ActorKind::User,
        crate::Scope::Global,
        crate::RoleSet::of(roles),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `impl Policy<Edit, Post> for Actor` in
    /// `docs/03-batteries/31-authorization.md`, exercised on its three branches.
    #[tokio::test]
    async fn the_documented_edit_policy_behaves_as_documented() {
        use crate::Policy as _;

        let post = Post {
            id: 1,
            author_id: "usr_1".to_owned(),
            published: false,
            title: "Draft".to_owned(),
        };
        let ctx = crate::detached_ctx();

        let author = actor("usr_1", [Role::Editor]);
        assert_eq!(author.allows(Edit, &post, &ctx).await.reason(), "author");

        let admin = actor("usr_9", [Role::Admin]);
        assert_eq!(
            admin.allows(Edit, &post, &ctx).await.reason(),
            "admin override",
        );

        let stranger = actor("usr_2", [Role::Viewer]);
        assert!(!stranger.allows(Edit, &post, &ctx).await.allowed());
    }
}

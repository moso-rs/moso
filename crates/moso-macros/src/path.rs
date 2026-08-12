//! Route template validation, at the literal.
//!
//! `moso_core::router::validate_path` enforces the same rules as a `const fn`,
//! and `route_path!` binds it to a `const` so a hand-written
//! `Router::get(route_path!("/users/:id"), …)` still fails at compile time. What
//! it cannot do is *explain* itself: a const-evaluation panic prints one
//! `E0080` line, points into `moso-core/src/router.rs` and then into
//! `core/src/panic.rs`, and has nowhere to put a `note:` or a `help:`.
//!
//! `routes!` and `ep!` have the literal in hand, so they check it here instead
//! and produce the shape the style guide asks for — the user's span, one
//! sentence, the rule, and a fix that can be pasted:
//!
//! ```text
//! error: legacy path parameter syntax: write `{id}`, not `:id`
//!
//!        note: a route and the operation it documents spell a parameter the same way
//!        help: write "/users/{id}"
//!   --> src/routes/users.rs:9:13
//!    |
//!  9 |         GET "/users/:id" => show,
//!    |             ^^^^^^^^^^^^
//! ```
//!
//! # Two implementations of one rule
//!
//! This is a deliberate duplicate of `validate_path`: a proc-macro crate may not
//! depend on a runtime crate (`03-crate-layout.md`), so the rules cannot be
//! shared as code. They are kept in step by the table below, which is a copy of
//! the one on `validate_path`, and by [`tests`], which exercises every row.
//!
//! | Rejected | Why |
//! | --- | --- |
//! | `""`, `"users"` | `matchit` requires a leading `/` |
//! | `/users/:id` | pre-0.8 Axum / Actix syntax; write `{id}` |
//! | `/files/*rest` | pre-0.8 wildcard syntax; write `{*rest}` |
//! | `/users/{id`, `/users/id}` | unbalanced braces |
//! | `/users/{}` | a parameter must have a name |
//! | `/users/{a}{b}` | one parameter per segment |
//! | `/users/{id}x` | static text may not follow a parameter in a segment |
//! | `/{*rest}/more` | a catch-all must be the last segment |
//! | `/{id}/posts/{id}` | duplicate parameter name |

use syn::{Error, LitStr};

/// Check a route template, reporting the first problem against `literal`.
///
/// One error, not a cascade: `"/users/:id/:slug"` is one mistake made twice, and
/// the second report would say nothing the first did not.
pub(crate) fn validate(literal: &LitStr) -> Result<(), Error> {
    let path = literal.value();
    match check(&path) {
        Ok(()) => Ok(()),
        Err(problem) => Err(Error::new(literal.span(), problem.render(&path))),
    }
}

/// What is wrong with a template, in the vocabulary of the person who typed it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    /// `""`.
    Empty,
    /// `"users"` — no leading slash.
    NoLeadingSlash,
    /// `":id"` — the pre-brace parameter syntax, with the name that was used.
    LegacyParameter(String),
    /// `"*rest"` — the pre-brace wildcard syntax, with the name that was used.
    LegacyWildcard(String),
    /// A `{` with no `}` before the end of the segment.
    Unterminated,
    /// A `}` with no `{` before it.
    Unmatched,
    /// `"{}"`.
    Unnamed,
    /// A character in a parameter name that a struct field could not carry.
    IllegalNameCharacter(char),
    /// `"{a}{b}"`.
    TwoInOneSegment,
    /// `"{id}.json"`, `"v{n}"`.
    TextBesideParameter,
    /// `"{*rest}/more"`.
    CatchAllNotLast(String),
    /// The same name twice.
    DuplicateName(String),
}

impl Problem {
    /// The full message, `note:` and `help:` included.
    ///
    /// Every arm ends in a `help:` line that repeats the user's own path with
    /// the mistake corrected, because a fix the reader has to adapt is a fix
    /// they can get wrong.
    fn render(&self, path: &str) -> String {
        match self {
            Self::Empty => "a route path must not be empty\n\n\
                 help: write \"/\" for the root of the router"
                .to_owned(),
            Self::NoLeadingSlash => format!(
                "a route path must start with `/`\n\n\
                 help: write \"/{path}\""
            ),
            Self::LegacyParameter(name) => format!(
                "legacy path parameter syntax: write `{{{name}}}`, not `:{name}`\n\n\
                 note: a route and the operation it documents spell a parameter the same way\n\
                 help: write \"{fixed}\"",
                fixed = replace_legacy(path)
            ),
            Self::LegacyWildcard(name) => format!(
                "legacy wildcard syntax: write `{{*{name}}}`, not `*{name}`\n\n\
                 note: a route and the operation it documents spell a parameter the same way\n\
                 help: write \"{fixed}\"",
                fixed = replace_legacy(path)
            ),
            Self::Unterminated => format!(
                "unterminated `{{` in a route path\n\n\
                 help: close the parameter: \"{fixed}\"",
                fixed = close_braces(path)
            ),
            Self::Unmatched => "unmatched `}` in a route path\n\n\
                 help: open the parameter, as in \"/users/{id}\""
                .to_owned(),
            Self::Unnamed => "a path parameter must have a name\n\n\
                 note: the name is what `Path<T>` reads the captured value into\n\
                 help: write \"/users/{id}\", not \"/users/{}\""
                .to_owned(),
            Self::IllegalNameCharacter(character) => format!(
                "`{character}` is not allowed in a path parameter name\n\n\
                 note: a name is also a field on the `Path<T>` that reads it, so it is limited to \
                 letters, digits and `_`\n\
                 help: write \"/users/{{id}}\", and destructure it as `Path(id): Path<u32>`"
            ),
            Self::TwoInOneSegment => "only one parameter is allowed per path segment\n\n\
                 note: with no separator between them, neither has a boundary the router could \
                 find\n\
                 help: give each a segment of its own: \"/users/{id}/posts/{slug}\""
                .to_owned(),
            Self::TextBesideParameter => {
                "static text may not follow a parameter inside one path segment\n\n\
                 note: the parameter would swallow the text, so nothing could ever match\n\
                 help: give the parameter a segment of its own: \"/files/{name}/download\""
                    .to_owned()
            }
            Self::CatchAllNotLast(name) => format!(
                "a catch-all parameter must be the last segment of a route path\n\n\
                 note: `{{*{name}}}` matches everything that follows, so nothing can follow it\n\
                 help: move it to the end, or use a named parameter: \"{{{name}}}\""
            ),
            Self::DuplicateName(name) => format!(
                "the parameter `{name}` appears twice in this path\n\n\
                 note: `Path<T>` reads captures by name, so the second would shadow the first\n\
                 help: give them different names, as in \"/users/{{user_id}}/posts/{{post_id}}\""
            ),
        }
    }
}

/// `/users/:id` becomes `/users/{id}`; `/files/*rest` becomes `/files/{*rest}`.
///
/// Applied to the whole path so the `help:` line is the user's route with every
/// legacy segment rewritten, not just the one that was reported.
fn replace_legacy(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 4);
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        match segment.strip_prefix(':') {
            Some(name) if !name.is_empty() => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
            _ => match segment.strip_prefix('*') {
                Some(name) if !name.is_empty() => {
                    out.push_str("{*");
                    out.push_str(name);
                    out.push('}');
                }
                _ => out.push_str(segment),
            },
        }
    }
    out
}

/// The path with a `}` added where a segment opened a parameter and never
/// closed it.
fn close_braces(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    for (index, segment) in path.split('/').enumerate() {
        if index > 0 {
            out.push('/');
        }
        out.push_str(segment);
        if segment.contains('{') && !segment.contains('}') {
            out.push('}');
        }
    }
    out
}

/// The rules, in the order a reader would hit them.
fn check(path: &str) -> Result<(), Problem> {
    if path.is_empty() {
        return Err(Problem::Empty);
    }
    if !path.starts_with('/') {
        return Err(Problem::NoLeadingSlash);
    }

    let mut seen: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    let last = segments.len().saturating_sub(1);

    for (index, segment) in segments.iter().enumerate() {
        if let Some(name) = segment.strip_prefix(':') {
            return Err(Problem::LegacyParameter(identifier(name)));
        }
        if let Some(name) = segment.strip_prefix('*') {
            return Err(Problem::LegacyWildcard(identifier(name)));
        }
        for name in check_segment(segment, index == last)? {
            if seen.contains(&name) {
                return Err(Problem::DuplicateName(name.to_owned()));
            }
            seen.push(name);
        }
    }
    Ok(())
}

/// The parameter names one segment declares, after checking its shape.
fn check_segment(segment: &str, is_last: bool) -> Result<Vec<&str>, Problem> {
    let bytes = segment.as_bytes();
    let mut names = Vec::new();
    let mut index = 0;
    let mut parameters = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            b'}' => return Err(Problem::Unmatched),
            b'{' => {
                if parameters > 0 {
                    return Err(Problem::TwoInOneSegment);
                }
                if index > 0 {
                    return Err(Problem::TextBesideParameter);
                }
                let Some(offset) = bytes[index + 1..].iter().position(|byte| *byte == b'}') else {
                    return Err(Problem::Unterminated);
                };
                let close = index + 1 + offset;
                let mut start = index + 1;
                let catch_all = bytes.get(start) == Some(&b'*');
                if catch_all {
                    start += 1;
                }
                if start >= close {
                    return Err(Problem::Unnamed);
                }
                let name = &segment[start..close];
                if let Some(character) = name
                    .chars()
                    .find(|c| !c.is_ascii_alphanumeric() && *c != '_')
                {
                    return Err(Problem::IllegalNameCharacter(character));
                }
                if catch_all && !is_last {
                    return Err(Problem::CatchAllNotLast(name.to_owned()));
                }
                names.push(name);
                parameters += 1;
                index = close + 1;
            }
            _ => {
                if parameters > 0 {
                    return Err(Problem::TextBesideParameter);
                }
                index += 1;
            }
        }
    }
    Ok(names)
}

/// The identifier part of a legacy segment, for the message that quotes it.
///
/// `:id` yields `id`; a bare `:` or `*` yields `id`/`rest` so the `help:` line
/// is still a path the reader can copy.
fn identifier(rest: &str) -> String {
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        "id".to_owned()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn problem(path: &str) -> Problem {
        check(path).expect_err(path)
    }

    #[test]
    fn valid_templates_are_accepted() {
        for path in [
            "/",
            "/users",
            "/users/{id}",
            "/users/{id}/posts/{slug}",
            "/files/{*rest}",
            "/a/b/c",
            "/{id}",
        ] {
            assert!(check(path).is_ok(), "{path} should be valid");
        }
    }

    #[test]
    fn the_rule_table_is_enforced_row_by_row() {
        assert_eq!(problem(""), Problem::Empty);
        assert_eq!(problem("users"), Problem::NoLeadingSlash);
        assert_eq!(
            problem("/users/:id"),
            Problem::LegacyParameter("id".to_owned())
        );
        assert_eq!(
            problem("/files/*rest"),
            Problem::LegacyWildcard("rest".to_owned())
        );
        assert_eq!(problem("/users/{id"), Problem::Unterminated);
        assert_eq!(problem("/users/id}"), Problem::Unmatched);
        assert_eq!(problem("/users/{}"), Problem::Unnamed);
        assert_eq!(problem("/users/{a}{b}"), Problem::TwoInOneSegment);
        assert_eq!(problem("/users/{id}x"), Problem::TextBesideParameter);
        assert_eq!(problem("/v{n}"), Problem::TextBesideParameter);
        assert_eq!(
            problem("/{*rest}/more"),
            Problem::CatchAllNotLast("rest".to_owned())
        );
        assert_eq!(
            problem("/{id}/posts/{id}"),
            Problem::DuplicateName("id".to_owned())
        );
        assert_eq!(
            problem("/users/{user-id}"),
            Problem::IllegalNameCharacter('-')
        );
    }

    #[test]
    fn every_message_carries_a_help_line() {
        for path in [
            "",
            "users",
            "/users/:id",
            "/files/*rest",
            "/users/{id",
            "/users/id}",
            "/users/{}",
            "/users/{a}{b}",
            "/users/{id}x",
            "/{*rest}/more",
            "/{id}/posts/{id}",
            "/users/{user-id}",
        ] {
            let rendered = problem(path).render(path);
            assert!(
                rendered.contains("help: "),
                "no help line for {path}: {rendered}"
            );
            for line in rendered.lines() {
                assert!(
                    line.chars().count() <= 110,
                    "line too long for {path}: {line}"
                );
            }
        }
    }

    #[test]
    fn the_help_line_repeats_the_users_own_path_corrected() {
        let rendered = problem("/users/:id/posts/:slug").render("/users/:id/posts/:slug");
        assert!(
            rendered.contains("help: write \"/users/{id}/posts/{slug}\""),
            "{rendered}"
        );

        let rendered = problem("/files/*rest").render("/files/*rest");
        assert!(
            rendered.contains("help: write \"/files/{*rest}\""),
            "{rendered}"
        );

        let rendered = problem("/users/{id").render("/users/{id");
        assert!(
            rendered.contains("help: close the parameter: \"/users/{id}\""),
            "{rendered}"
        );
    }

    /// Only a *leading* colon is the old parameter syntax, so a static segment
    /// that happens to contain one is left alone. `moso_core::has_legacy_syntax`
    /// makes the same distinction, and its unit test asserts the same path.
    #[test]
    fn a_colon_inside_a_segment_is_not_legacy_syntax() {
        assert!(check("/a:b").is_ok());
    }
}

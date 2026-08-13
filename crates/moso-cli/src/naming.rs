//! Turning one word the user typed into the six spellings a template needs.
//!
//! `moso generate endpoint posts` has to produce a module called `posts`, a
//! type called `Post`, a route at `/posts`, a `CreatePost` and a `PostOut`. All
//! of that comes from the single argument, so the rules that derive it are
//! worth writing down and testing rather than scattering `to_uppercase` calls
//! through the templates.
//!
//! # On the pluraliser
//!
//! It is a heuristic and it is meant to be. English pluralisation is not a
//! function of spelling — "mouse" and "house" differ in the last letter that
//! matters — and the correct fix is not a bigger table but an escape hatch:
//! `--singular` overrides it, and the generated code is ordinary Rust the user
//! is about to edit anyway. What the heuristic must not do is produce something
//! that fails to *compile*, which is why every output is forced through
//! [`to_snake`] or [`to_pascal`] rather than being used as typed.

/// The spellings one generated resource needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Names {
    /// The module name: snake case, plural. `posts`.
    pub module: String,
    /// The singular, snake case. `post`.
    pub singular: String,
    /// The singular type name: Pascal case. `Post`.
    pub type_name: String,
    /// The plural type name: Pascal case. `Posts`.
    pub type_plural: String,
    /// The URL path the router mounts at. `/posts`.
    pub path: String,
    /// Exactly what the user typed, in snake case, with no pluralisation.
    ///
    /// A resource is a noun and is inflected; a middleware is a verb and is not.
    /// `moso generate middleware observe` must produce `fn observe`, not
    /// `fn observes`, so the templates that name a *behaviour* rather than a
    /// *thing* use this and [`raw_type`](Self::raw_type) instead.
    pub raw: String,
    /// [`raw`](Self::raw) in Pascal case. `Observe`.
    pub raw_type: String,
    /// [`raw`](Self::raw) in screaming snake case, for constants. `OBSERVE`.
    pub raw_screaming: String,
    /// [`raw`](Self::raw) with underscores as hyphens, for header names.
    pub raw_kebab: String,
}

impl Names {
    /// Derive every spelling from what the user typed.
    ///
    /// `explicit_singular` comes from `--singular` and wins over the heuristic.
    #[must_use]
    pub fn new(input: &str, explicit_singular: Option<&str>) -> Self {
        let raw = to_snake(input);
        let singular = explicit_singular
            .map(to_snake)
            .unwrap_or_else(|| singularise(&raw));
        let plural = pluralise(&singular);
        Self {
            path: format!("/{plural}"),
            type_name: to_pascal(&singular),
            type_plural: to_pascal(&plural),
            module: plural,
            singular,
            raw_type: to_pascal(&raw),
            raw_screaming: raw.to_uppercase(),
            raw_kebab: raw.replace('_', "-"),
            raw,
        }
    }
}

/// Convert to `snake_case`.
///
/// Accepts what a user actually types: `BlogPost`, `blog-post`, `blog post`,
/// `blog_post` all become `blog_post`. A digit does not start a new word, so
/// `oauth2Client` is `oauth2_client` and not `oauth2_client` with a stray
/// underscore before the `2`.
#[must_use]
pub fn to_snake(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 4);
    let mut previous_lower_or_digit = false;

    for character in input.chars() {
        if character == '-' || character == ' ' || character == '_' {
            if !out.ends_with('_') && !out.is_empty() {
                out.push('_');
            }
            previous_lower_or_digit = false;
            continue;
        }
        if character.is_uppercase() {
            if previous_lower_or_digit && !out.ends_with('_') {
                out.push('_');
            }
            for lowered in character.to_lowercase() {
                out.push(lowered);
            }
            previous_lower_or_digit = false;
        } else {
            out.push(character);
            previous_lower_or_digit = character.is_alphanumeric();
        }
    }

    out.trim_matches('_').to_owned()
}

/// Convert to `PascalCase`.
#[must_use]
pub fn to_pascal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for word in to_snake(input).split('_').filter(|word| !word.is_empty()) {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

/// Words whose plural is not formed by any suffix rule.
///
/// Deliberately short. It exists so the three or four nouns that appear in
/// nearly every tutorial application do not come out wrong; it is not an
/// attempt at completeness, which `--singular` covers.
const IRREGULAR: &[(&str, &str)] = &[
    ("person", "people"),
    ("child", "children"),
    ("man", "men"),
    ("woman", "women"),
    ("datum", "data"),
    ("index", "indices"),
    ("matrix", "matrices"),
    ("mouse", "mice"),
];

/// Nouns that are spelled the same in both numbers.
const UNCOUNTABLE: &[&str] = &[
    "series",
    "species",
    "equipment",
    "information",
    "media",
    "news",
    "data",
    "audio",
    "traffic",
];

/// The plural of a snake-case singular.
#[must_use]
pub fn pluralise(singular: &str) -> String {
    if UNCOUNTABLE.contains(&singular) {
        return singular.to_owned();
    }
    for (one, many) in IRREGULAR {
        if singular == *one {
            return (*many).to_owned();
        }
    }
    // Only the final word of a compound is inflected: `blog_post` → `blog_posts`.
    let (prefix, last) = match singular.rsplit_once('_') {
        Some((prefix, last)) => (format!("{prefix}_"), last),
        None => (String::new(), singular),
    };

    let inflected = if last.ends_with('y')
        && last.len() > 1
        && !last.ends_with("ay")
        && !last.ends_with("ey")
        && !last.ends_with("oy")
        && !last.ends_with("uy")
    {
        format!("{}ies", &last[..last.len() - 1])
    } else if last.ends_with('s')
        || last.ends_with('x')
        || last.ends_with('z')
        || last.ends_with("ch")
        || last.ends_with("sh")
    {
        format!("{last}es")
    } else {
        format!("{last}s")
    };

    format!("{prefix}{inflected}")
}

/// The singular of a snake-case plural.
///
/// A word that is already singular is returned unchanged, which is what makes
/// `moso generate endpoint post` and `... posts` produce the same module.
#[must_use]
pub fn singularise(plural: &str) -> String {
    if UNCOUNTABLE.contains(&plural) {
        return plural.to_owned();
    }
    for (one, many) in IRREGULAR {
        if plural == *many {
            return (*one).to_owned();
        }
    }
    let (prefix, last) = match plural.rsplit_once('_') {
        Some((prefix, last)) => (format!("{prefix}_"), last),
        None => (String::new(), plural),
    };

    let deflected = if let Some(stem) = last.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = last
        .strip_suffix("es")
        .filter(|stem| stem.ends_with('s') || stem.ends_with('x') || stem.ends_with('z'))
    {
        stem.to_owned()
    } else if let Some(stem) = last
        .strip_suffix("es")
        .filter(|stem| stem.ends_with("ch") || stem.ends_with("sh"))
    {
        stem.to_owned()
    } else if last.ends_with("ss") {
        // `address` is not the plural of `addres`.
        last.to_owned()
    } else if let Some(stem) = last.strip_suffix('s') {
        stem.to_owned()
    } else {
        last.to_owned()
    };

    format!("{prefix}{deflected}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_case_accepts_every_spelling_a_user_might_type() {
        for input in [
            "BlogPost",
            "blog-post",
            "blog post",
            "blog_post",
            "blogPost",
        ] {
            assert_eq!(to_snake(input), "blog_post", "from {input}");
        }
        assert_eq!(to_snake("Post"), "post");
        assert_eq!(to_snake("oauth2Client"), "oauth2_client");
        assert_eq!(to_snake("__leading"), "leading");
    }

    #[test]
    fn pascal_case_is_the_inverse_shape() {
        assert_eq!(to_pascal("blog_post"), "BlogPost");
        assert_eq!(to_pascal("post"), "Post");
        assert_eq!(to_pascal("blog-post"), "BlogPost");
        assert_eq!(to_pascal("oauth2_client"), "Oauth2Client");
    }

    #[test]
    fn regular_plurals_follow_the_suffix_rules() {
        assert_eq!(pluralise("post"), "posts");
        assert_eq!(pluralise("category"), "categories");
        assert_eq!(pluralise("box"), "boxes");
        assert_eq!(pluralise("class"), "classes");
        assert_eq!(pluralise("batch"), "batches");
        assert_eq!(pluralise("dish"), "dishes");
        // A vowel before the `y` keeps it: `day` is not `daies`.
        assert_eq!(pluralise("day"), "days");
        assert_eq!(pluralise("key"), "keys");
    }

    #[test]
    fn only_the_last_word_of_a_compound_is_inflected() {
        assert_eq!(pluralise("blog_post"), "blog_posts");
        assert_eq!(pluralise("user_category"), "user_categories");
        assert_eq!(singularise("blog_posts"), "blog_post");
        assert_eq!(singularise("user_categories"), "user_category");
    }

    #[test]
    fn irregular_and_uncountable_nouns_are_handled() {
        assert_eq!(pluralise("person"), "people");
        assert_eq!(singularise("people"), "person");
        assert_eq!(pluralise("series"), "series");
        assert_eq!(singularise("series"), "series");
    }

    #[test]
    fn a_double_s_word_is_not_mistaken_for_a_plural() {
        assert_eq!(singularise("address"), "address");
        assert_eq!(singularise("addresses"), "address");
    }

    #[test]
    fn singular_and_plural_input_produce_the_same_resource() {
        let from_plural = Names::new("posts", None);
        let from_singular = Names::new("post", None);

        // Every *resource* spelling agrees, which is what makes `generate
        // endpoint post` and `... posts` interchangeable.
        assert_eq!(from_plural.module, from_singular.module);
        assert_eq!(from_plural.singular, from_singular.singular);
        assert_eq!(from_plural.type_name, from_singular.type_name);
        assert_eq!(from_plural.path, from_singular.path);

        assert_eq!(from_plural.module, "posts");
        assert_eq!(from_plural.singular, "post");
        assert_eq!(from_plural.type_name, "Post");
        assert_eq!(from_plural.path, "/posts");

        // `raw` deliberately does *not* agree: it is what the user typed, and a
        // middleware called `posts` must not become a function called `post`.
        assert_eq!(from_plural.raw, "posts");
        assert_eq!(from_singular.raw, "post");
    }

    #[test]
    fn an_explicit_singular_overrides_the_heuristic() {
        // `mice` would be `mouse` by the table, but the point is that a word the
        // heuristic gets wrong can always be corrected.
        let names = Names::new("geese", Some("goose"));
        assert_eq!(names.singular, "goose");
        assert_eq!(names.type_name, "Goose");
        assert_eq!(
            names.module, "gooses",
            "the plural follows the given singular"
        );
    }

    #[test]
    fn a_pascal_case_argument_still_produces_a_snake_case_module() {
        let names = Names::new("BlogPost", None);
        assert_eq!(names.module, "blog_posts");
        assert_eq!(names.type_name, "BlogPost");
        assert_eq!(names.type_plural, "BlogPosts");
        assert_eq!(names.path, "/blog_posts");
    }
}

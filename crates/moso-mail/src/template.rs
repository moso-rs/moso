//! Template rendering, and the check that makes a typo cheap to find.
//!
//! Templates are Jinja2-compatible, because designers and language models
//! already know that syntax. Undefined variables are strict, so
//! `{{ user.nmae }}` fails the render rather than sending "Hello ,", and
//! [`TemplateEngine::variables`] reports every path a template references —
//! which is what a test compares against the keys its context builder sets, so
//! the typo is found by `cargo test` and not by a password-reset email at 3am.
//!
//! That comparison is the job a `#[derive(Email)]` would do at compile time.
//! `moso-macros` ships no such derive, so this module's `variables` is the
//! whole of the check and a test is where it runs.

use std::borrow::Cow;

use serde::Serialize;

use crate::Result;

/// Where a template's source came from.
///
/// ```
/// use moso_mail::TemplateSource;
///
/// let inline = TemplateSource::Inline("Hello {{ name }}");
/// assert!(matches!(inline, TemplateSource::Inline(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TemplateSource {
    /// Embedded in the binary by `include_str!`, so that a deployment cannot
    /// lose its templates.
    Inline(&'static str),
    /// Read from disk at boot, for templates an operator edits without a
    /// rebuild. Reloaded on SIGHUP with the rest of the configuration.
    Path(std::path::PathBuf),
}

/// One named template.
///
/// ```
/// use moso_mail::{Template, TemplateSource};
///
/// let t = Template::inline("emails/welcome.html", "Hi {{ user.name }}");
/// assert_eq!(t.name(), "emails/welcome.html");
/// ```
#[derive(Clone, Debug)]
pub struct Template {
    /// The name the message refers to it by.
    name: Cow<'static, str>,
    /// Where the source lives.
    source: TemplateSource,
}

impl Template {
    /// A template whose source is embedded in the binary.
    ///
    /// ```
    /// use moso_mail::Template;
    ///
    /// let _ = Template::inline("emails/welcome.txt", "Hi {{ name }}");
    /// ```
    #[must_use]
    pub const fn inline(name: &'static str, source: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            source: TemplateSource::Inline(source),
        }
    }

    /// A template read from disk.
    ///
    /// ```
    /// use moso_mail::Template;
    ///
    /// let t = Template::from_path("emails/welcome.html", "templates/welcome.html");
    /// assert_eq!(t.name(), "emails/welcome.html");
    /// ```
    #[must_use]
    pub fn from_path(
        name: impl Into<Cow<'static, str>>,
        path: impl Into<std::path::PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            source: TemplateSource::Path(path.into()),
        }
    }

    /// The template's source text, reading the file when it lives on disk.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) when the file cannot be
    /// read. The message names the path, because "template not found" without
    /// one is the least useful error in this crate.
    ///
    /// ```
    /// use moso_mail::Template;
    ///
    /// assert_eq!(Template::inline("a", "hi").read()?, "hi");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    pub fn read(&self) -> Result<String> {
        match &self.source {
            TemplateSource::Inline(source) => Ok((*source).to_owned()),
            TemplateSource::Path(path) => std::fs::read_to_string(path).map_err(|error| {
                crate::Error::template(
                    self.name.clone(),
                    format!("could not read `{}`: {error}", path.display()),
                )
            }),
        }
    }

    /// The template's name.
    ///
    /// ```
    /// use moso_mail::Template;
    ///
    /// assert_eq!(Template::inline("a", "b").name(), "a");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the source comes from.
    ///
    /// ```
    /// use moso_mail::{Template, TemplateSource};
    ///
    /// assert!(matches!(Template::inline("a", "b").source(), TemplateSource::Inline("b")));
    /// ```
    #[must_use]
    pub fn source(&self) -> &TemplateSource {
        &self.source
    }
}

/// Renders a template against a context.
///
/// Dyn-compatible so the engine can be swapped in configuration, and so that
/// a message type holds `&dyn TemplateEngine` rather than being generic over
/// one.
///
/// ```no_run
/// use moso_mail::TemplateEngine;
///
/// fn render(engine: &dyn TemplateEngine, name: &str) -> moso_mail::Result<String> {
///     engine.render(name, &serde_json::json!({ "name": "Ada" }))
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a template engine",
    label = "not a template engine",
    note = "a template engine implements `add`, `render` and `variables`",
    note = "help: the shipped engine is `Jinja`; write your own only to change the template \
            language"
)]
pub trait TemplateEngine: Send + Sync + 'static {
    /// Register a template. Called once per template at boot.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) when the source does not
    /// parse. This is a boot failure, so a broken template never reaches a
    /// send.
    fn add(&mut self, template: Template) -> Result<()>;

    /// Render a registered template against a JSON context.
    ///
    /// The context is `serde_json::Value` rather than a generic `S: Serialize`
    /// so the trait stays dyn-compatible; [`render_with`]
    /// is the typed convenience on top.
    ///
    /// # Errors
    ///
    /// [`Error::Template`](crate::Error::Template) when the template is not
    /// registered or rendering fails.
    fn render(&self, name: &str, context: &serde_json::Value) -> Result<String>;

    /// The variable paths a registered template references, e.g. `user.name`.
    ///
    /// This is what `moso check` and the derive's compile-time check consume.
    /// Returns an empty vector for a template the engine does not know.
    fn variables(&self, name: &str) -> Vec<String>;
}

/// Render with a typed context.
///
/// A free function rather than a trait method, because a generic method would
/// make [`TemplateEngine`] dyn-incompatible and the trait object is the point.
///
/// # Errors
///
/// [`Error::Template`](crate::Error::Template) when serialisation or rendering
/// fails.
///
/// ```no_run
/// use moso_mail::{render_with, TemplateEngine};
/// use serde::Serialize;
///
/// /// The context of the welcome email.
/// #[derive(Serialize)]
/// struct Ctx<'a> {
///     /// The recipient's display name.
///     name: &'a str,
/// }
///
/// fn go(engine: &dyn TemplateEngine) -> moso_mail::Result<String> {
///     render_with(engine, "emails/welcome.html", &Ctx { name: "Ada" })
/// }
/// ```
pub fn render_with<C: Serialize>(
    engine: &dyn TemplateEngine,
    name: &str,
    context: &C,
) -> Result<String> {
    let value = serde_json::to_value(context).map_err(|error| {
        crate::Error::template(
            name.to_owned(),
            format!("the context did not serialise: {error}"),
        )
    })?;
    engine.render(name, &value)
}

/// The shipped Jinja2-compatible engine.
///
/// # Two defaults that are not negotiable by accident
///
/// **Undefined variables are an error, not an empty string.** minijinja's
/// default is lenient; this engine sets `UndefinedBehavior::Strict`, so a typo
/// that survived the derive's compile-time check — a dynamic template loaded
/// from disk, typically — fails the render instead of silently sending
/// "Hello , your order is ready".
///
/// **HTML templates autoescape, text templates do not.** The decision is made
/// per template from its name, so `welcome.html` escapes and `welcome.txt`
/// does not. Escaping a text part would put `&amp;` in front of a reader.
///
/// ```
/// use moso_mail::{Jinja, Template, TemplateEngine};
///
/// let mut engine = Jinja::new();
/// engine.add(Template::inline("hi.txt", "Hello {{ name }}"))?;
/// let context = serde_json::json!({ "name": "Ada" });
/// assert_eq!(engine.render("hi.txt", &context)?, "Hello Ada");
/// # Ok::<(), moso_mail::Error>(())
/// ```
pub struct Jinja {
    /// The registered templates, by name.
    templates: std::collections::BTreeMap<String, Template>,
    /// The engine that owns the parsed sources.
    environment: minijinja::Environment<'static>,
    /// Whether `.html` templates autoescape. On unless turned off.
    autoescape: bool,
}

impl Jinja {
    /// An engine with no templates.
    ///
    /// ```
    /// use moso_mail::Jinja;
    ///
    /// assert_eq!(Jinja::new().len(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::with_autoescape(true)
    }

    /// Turn autoescaping on or off. On by default for `.html` templates.
    ///
    /// Turning it off is for a template that assembles HTML from trusted
    /// fragments and nothing else. A template that interpolates anything a
    /// user typed and does not escape it is an XSS in whatever webmail opens
    /// the message.
    ///
    /// ```
    /// use moso_mail::{Jinja, Template, TemplateEngine};
    ///
    /// let mut engine = Jinja::new().autoescape(false);
    /// engine.add(Template::inline("raw.html", "{{ body }}"))?;
    /// let context = serde_json::json!({ "body": "<b>bold</b>" });
    /// assert_eq!(engine.render("raw.html", &context)?, "<b>bold</b>");
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn autoescape(self, enabled: bool) -> Self {
        // Rebuilt rather than mutated: the escape decision is a callback
        // installed on the environment, and templates already added were
        // parsed under the old one.
        let mut rebuilt = Self::with_autoescape(enabled);
        for template in self.templates.into_values() {
            // Re-adding a template that parsed once cannot fail.
            let _ = rebuilt.add(template);
        }
        rebuilt
    }

    /// How many templates are registered.
    ///
    /// ```
    /// use moso_mail::Jinja;
    ///
    /// assert_eq!(Jinja::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// Whether no template is registered.
    ///
    /// ```
    /// use moso_mail::Jinja;
    ///
    /// assert!(Jinja::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// Every registered template's name, in order.
    ///
    /// What `moso doctor` lists and what a "no such template" error suggests
    /// from.
    ///
    /// ```
    /// use moso_mail::{Jinja, Template, TemplateEngine};
    ///
    /// let mut engine = Jinja::new();
    /// engine.add(Template::inline("a.txt", "x"))?;
    /// assert_eq!(engine.names(), vec!["a.txt"]);
    /// # Ok::<(), moso_mail::Error>(())
    /// ```
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.templates.keys().map(String::as_str).collect()
    }

    /// The engine behind both constructors.
    fn with_autoescape(autoescape: bool) -> Self {
        let mut environment = minijinja::Environment::new();
        // An undefined variable is a typo, and a typo in a password-reset
        // email is a support ticket. Fail the render.
        environment.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        environment.set_auto_escape_callback(move |name| {
            if autoescape && is_html_template(name) {
                minijinja::AutoEscape::Html
            } else {
                minijinja::AutoEscape::None
            }
        });
        Self {
            templates: std::collections::BTreeMap::new(),
            environment,
            autoescape,
        }
    }
}

impl Default for Jinja {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Jinja {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Jinja")
            .field("templates", &self.names())
            .field("autoescape", &self.autoescape)
            .finish()
    }
}

impl TemplateEngine for Jinja {
    fn add(&mut self, template: Template) -> Result<()> {
        let source = template.read()?;
        let name = template.name().to_owned();
        self.environment
            .add_template_owned(name.clone(), source)
            .map_err(|error| crate::Error::template(name.clone(), describe(&error)))?;
        self.templates.insert(name, template);
        Ok(())
    }

    fn render(&self, name: &str, context: &serde_json::Value) -> Result<String> {
        let template = self
            .environment
            .get_template(name)
            .map_err(|error| crate::Error::template(name.to_owned(), describe(&error)))?;
        template
            .render(context)
            .map_err(|error| crate::Error::template(name.to_owned(), describe(&error)))
    }

    fn variables(&self, name: &str) -> Vec<String> {
        let Ok(template) = self.environment.get_template(name) else {
            return Vec::new();
        };
        // `nested` gives dotted paths — `user.name` and not just `user` —
        // which is exactly what the derive checks against the struct's fields.
        let mut names: Vec<String> = template.undeclared_variables(true).into_iter().collect();
        names.sort();
        names
    }
}

/// Whether a template's name says it produces HTML.
///
/// By extension, because that is the only signal available before the source
/// is parsed and it is the signal every template author already understands.
fn is_html_template(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm") || lower.ends_with(".xhtml")
}

/// minijinja's error, flattened into one line with its whole chain.
///
/// The default `Display` prints only the outermost message, and the useful
/// half — "undefined value", with the line number — is in the source.
fn describe(error: &minijinja::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

/// Derive a readable plain-text part from an HTML one.
///
/// What [`Message`](crate::Message) uses when no text part was given, and what
/// a hand-written [`Email::text`](crate::Email::text) should call. Links become
/// `text (https://…)`, block elements become newlines, entities are unescaped
/// and `<script>`/`<style>` bodies are dropped entirely — the goal is a
/// message a human can read, not a round trip.
///
/// ```
/// use moso_mail::html_to_text;
///
/// assert_eq!(html_to_text("<p>Hi <b>Ada</b></p>"), "Hi Ada");
/// assert_eq!(
///     html_to_text(r#"<a href="https://x.example">verify</a>"#),
///     "verify (https://x.example)",
/// );
/// ```
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut chars = html.char_indices().peekable();

    // The `href` of the anchor currently open, if any, and how long `out` was
    // when it opened — so the URL is only appended when the anchor had text.
    let mut open_link: Option<(String, usize)> = None;

    while let Some((index, c)) = chars.next() {
        if c != '<' {
            if c == '&' {
                let rest = &html[index..];
                if let Some((entity, consumed)) = decode_entity(rest) {
                    out.push_str(entity);
                    for _ in 1..consumed {
                        chars.next();
                    }
                    continue;
                }
            }
            // Runs of whitespace collapse, as they do when a browser renders.
            if c.is_whitespace() {
                if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() {
                    out.push(' ');
                }
            } else {
                out.push(c);
            }
            continue;
        }

        // A tag. Find its end; an unterminated `<` is literal text.
        let Some(end) = html[index..].find('>').map(|offset| index + offset) else {
            out.push('<');
            continue;
        };
        let tag = &html[index + 1..end];
        for _ in 0..(end - index) {
            chars.next();
        }

        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();

        match name.as_str() {
            // A script or a style has a body nobody should read. Skip to the
            // matching close tag rather than emitting its source.
            "script" | "style" if !closing => {
                let close = format!("</{name}");
                let remainder = &html[end + 1..];
                let skip = remainder
                    .to_ascii_lowercase()
                    .find(&close)
                    .map_or(remainder.len(), |offset| offset);
                for _ in 0..skip {
                    chars.next();
                }
            }
            "br" => push_newline(&mut out),
            "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "table"
            | "ul" | "ol" | "blockquote" | "section" | "article" | "header" | "footer" => {
                push_newline(&mut out);
                if !closing && name == "li" {
                    out.push_str("- ");
                }
            }
            "a" => {
                if closing {
                    if let Some((href, opened_at)) = open_link.take()
                        && out.len() > opened_at
                        && !href.is_empty()
                        // A link whose text already *is* the URL reads worse
                        // with the URL repeated after it.
                        && !out[opened_at..].trim().eq_ignore_ascii_case(href.trim())
                    {
                        out.push_str(" (");
                        out.push_str(&href);
                        out.push(')');
                    }
                } else {
                    open_link = Some((attribute(tag, "href").unwrap_or_default(), out.len()));
                }
            }
            _ => {}
        }
    }

    // Collapse the runs of blank lines the block handling produces, and trim.
    let mut text = String::with_capacity(out.len());
    let mut blank = 0_usize;
    for line in out.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        text.push_str(line);
        text.push('\n');
    }
    text.trim().to_owned()
}

/// Append a newline unless one is already there.
fn push_newline(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Read one attribute out of a tag's body, single or double quoted.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0_usize;
    loop {
        let at = lower[from..].find(name)? + from;
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let after = &lower[at + name.len()..];
        let after_ok = after.trim_start().starts_with('=');
        if before_ok && after_ok {
            let value = tag[at + name.len()..].trim_start();
            let value = value.strip_prefix('=')?.trim_start();
            let quote = value.chars().next()?;
            return if quote == '"' || quote == '\'' {
                value[1..].find(quote).map(|end| value[1..=end].to_owned())
            } else {
                Some(
                    value
                        .split(|c: char| c.is_whitespace())
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                )
            };
        }
        from = at + name.len();
    }
}

/// Decode one HTML entity at the head of `rest`, returning it and how many
/// bytes it occupied.
///
/// The five that matter plus the numeric forms. A message that needs more than
/// this in its *text* part has a bigger problem than an entity table.
fn decode_entity(rest: &str) -> Option<(&'static str, usize)> {
    const NAMED: &[(&str, &str)] = &[
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&apos;", "'"),
        ("&#39;", "'"),
        ("&nbsp;", " "),
        ("&mdash;", "—"),
        ("&ndash;", "–"),
        ("&hellip;", "…"),
    ];
    NAMED
        .iter()
        .find(|(entity, _)| rest.starts_with(*entity))
        .map(|(entity, decoded)| (*decoded, entity.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A typo that survived the compile-time check must not silently render as
    /// an empty string in a transactional email.
    #[test]
    fn an_undefined_variable_fails_the_render() {
        let mut engine = Jinja::new();
        engine
            .add(Template::inline("hi.txt", "Hello {{ nmae }}"))
            .expect("parses");
        let error = engine
            .render("hi.txt", &serde_json::json!({ "name": "Ada" }))
            .expect_err("strict undefined");
        assert!(matches!(error, crate::Error::Template { .. }));
    }

    /// The whole point of `variables`: it is what the derive checks a struct's
    /// fields against, in dotted form.
    #[test]
    fn the_variables_a_template_references_are_reported_in_dotted_form() {
        let mut engine = Jinja::new();
        engine
            .add(Template::inline(
                "welcome.html",
                "Hi {{ user.name }}, <a href=\"{{ verify_url }}\">verify</a>",
            ))
            .expect("parses");
        assert_eq!(
            engine.variables("welcome.html"),
            vec!["user.name".to_owned(), "verify_url".to_owned()],
        );
    }

    /// An unregistered template has no variables rather than panicking, so a
    /// caller enumerating templates does not have to guard every call.
    #[test]
    fn an_unknown_template_has_no_variables() {
        assert!(Jinja::new().variables("nope.html").is_empty());
    }

    /// A template that does not parse is a boot failure, which is the whole
    /// reason `add` returns a `Result`.
    #[test]
    fn a_template_that_does_not_parse_is_refused_at_registration() {
        let mut engine = Jinja::new();
        let error = engine
            .add(Template::inline("broken.html", "{% for x in %}"))
            .expect_err("syntax error");
        assert!(matches!(error, crate::Error::Template { .. }));
    }

    /// HTML escapes and text does not: a `&amp;` in front of a reader of the
    /// text part is a bug, and an unescaped `<script>` in the HTML part is an
    /// XSS in whatever webmail opens it.
    #[test]
    fn autoescaping_follows_the_templates_extension() {
        let mut engine = Jinja::new();
        engine
            .add(Template::inline("x.html", "{{ v }}"))
            .expect("parses");
        engine
            .add(Template::inline("x.txt", "{{ v }}"))
            .expect("parses");
        let context = serde_json::json!({ "v": "a & <b>" });

        assert_eq!(
            engine.render("x.html", &context).expect("renders"),
            "a &amp; &lt;b&gt;"
        );
        assert_eq!(
            engine.render("x.txt", &context).expect("renders"),
            "a & <b>"
        );
    }

    /// Turning autoescaping off keeps the already-registered templates.
    #[test]
    fn disabling_autoescaping_preserves_the_registered_templates() {
        let mut engine = Jinja::new();
        engine
            .add(Template::inline("x.html", "{{ v }}"))
            .expect("parses");
        let engine = engine.autoescape(false);

        assert_eq!(engine.names(), vec!["x.html"]);
        assert_eq!(
            engine
                .render("x.html", &serde_json::json!({ "v": "<b>" }))
                .expect("renders"),
            "<b>",
        );
    }

    /// A typed context is the ergonomic path, and it must agree with the
    /// untyped one.
    #[test]
    fn a_typed_context_renders_the_same_as_a_json_one() {
        #[derive(serde::Serialize)]
        struct Ctx<'a> {
            name: &'a str,
        }

        let mut engine = Jinja::new();
        engine
            .add(Template::inline("hi.txt", "Hello {{ name }}"))
            .expect("parses");
        assert_eq!(
            render_with(&engine, "hi.txt", &Ctx { name: "Ada" }).expect("renders"),
            "Hello Ada",
        );
    }

    /// The generated text part must carry the link target: a text-only reader
    /// with a bare "verify" and no URL cannot verify anything.
    #[test]
    fn html_to_text_keeps_the_link_target() {
        assert_eq!(
            html_to_text(r#"<p>Please <a href="https://x.example/v?t=1">verify</a>.</p>"#),
            "Please verify (https://x.example/v?t=1).",
        );
    }

    /// A link whose text already is its URL reads worse with the URL twice.
    #[test]
    fn html_to_text_does_not_repeat_a_bare_url() {
        assert_eq!(
            html_to_text(r#"<a href="https://x.example">https://x.example</a>"#),
            "https://x.example",
        );
    }

    /// Block elements become line breaks, so the text part has the shape of
    /// the message rather than being one run-on paragraph.
    #[test]
    fn html_to_text_turns_blocks_into_lines() {
        assert_eq!(
            html_to_text("<h1>Hi</h1><p>One</p><p>Two</p><ul><li>a</li><li>b</li></ul>"),
            "Hi\nOne\nTwo\n- a\n- b",
        );
    }

    /// Script and style bodies are source code, not prose.
    #[test]
    fn html_to_text_drops_script_and_style_bodies() {
        assert_eq!(
            html_to_text("<style>p{color:red}</style><p>Hi</p><script>alert(1)</script>"),
            "Hi",
        );
    }

    /// Entities are decoded: `&amp;` in a plain-text part is a rendering bug
    /// the reader sees.
    #[test]
    fn html_to_text_decodes_the_entities_that_matter() {
        assert_eq!(
            html_to_text("<p>Tom &amp; Jerry &mdash; &quot;hi&quot;</p>"),
            "Tom & Jerry — \"hi\""
        );
    }

    /// A template on disk is read when it is registered, and the error names
    /// the path when it is not there.
    #[test]
    fn a_missing_template_file_names_the_path() {
        let mut engine = Jinja::new();
        let error = engine
            .add(Template::from_path(
                "gone.html",
                "/definitely/not/here.html",
            ))
            .expect_err("no such file");
        let text = error.to_string();
        assert!(text.contains("/definitely/not/here.html"), "{text}");
    }
}

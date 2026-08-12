//! `sql!`: the raw-SQL escape hatch — non-negotiable N8.
//!
//! ```text
//! moso::sql!("select id, email from users where created_at > {since}")
//! // →
//! ::moso::__private::RawQuery::new("select id, email from users where created_at > $1")
//!     .bind(since)
//! ```
//!
//! # The one security property
//!
//! **An interpolation is always a bind parameter.** There is no syntax that
//! concatenates a runtime string into the statement text, so `sql!` cannot
//! produce an injection even when it is handed a request body. `{table}` binds
//! a *value* named `table`; it does not name a table.
//!
//! # The grammar
//!
//! | Written | Means |
//! | --- | --- |
//! | `{name}` | bind the variable `name` |
//! | `{a.b().c}` | bind the value of any Rust expression |
//! | `{value as Cents}` | bind it as a `Cents`, when inference needs telling |
//! | `{{` / `}}` | a literal `{` / `}` |

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::{Expr, LitStr, Token, Type};

use crate::shared::{err, private_path};

/// The macro's input: one string literal, and nothing else.
pub struct SqlInput {
    /// The statement, with `{…}` interpolations.
    pub template: LitStr,
}

impl Parse for SqlInput {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let template: LitStr = input.parse().map_err(|_| {
            err(
                input.span(),
                "`sql!` takes one string literal",
                "write `sql!(\"select 1\")` — the statement has to be visible at compile time, \
                 which is what makes an interpolation a bind parameter rather than text",
            )
        })?;
        if !input.is_empty() {
            let _: TokenStream = input.parse()?;
            return Err(err(
                template.span(),
                "`sql!` takes one string literal and no arguments",
                "interpolate the values into the statement: `sql!(\"… where id = {id}\")`",
            ));
        }
        Ok(Self { template })
    }
}

/// One piece of a parsed template.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// Statement text, with `{{` and `}}` already unescaped.
    Text(String),
    /// An interpolation's source, to be parsed as a Rust expression.
    Binding(String),
}

/// Splits a template into text and interpolations.
fn split(template: &str, span: proc_macro2::Span) -> syn::Result<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut characters = template.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                text.push('{');
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                text.push('}');
            }
            '}' => {
                return Err(err(
                    span,
                    "this `}` closes nothing",
                    "write `}}` for a literal brace",
                ));
            }
            '{' => {
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                let mut source = String::new();
                let mut depth = 0_usize;
                let mut closed = false;
                for inner in characters.by_ref() {
                    match inner {
                        '}' if depth == 0 => {
                            closed = true;
                            break;
                        }
                        '{' => {
                            depth += 1;
                            source.push(inner);
                        }
                        '}' => {
                            depth -= 1;
                            source.push(inner);
                        }
                        _ => source.push(inner),
                    }
                }
                if !closed {
                    return Err(err(
                        span,
                        "this `{` is never closed",
                        "every interpolation ends with `}` — `{user_id}`",
                    ));
                }
                if source.trim().is_empty() {
                    return Err(err(
                        span,
                        "an empty `{}` binds nothing",
                        "name the value: `{user_id}`",
                    ));
                }
                pieces.push(Piece::Binding(source));
            }
            _ => text.push(character),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// One interpolation, parsed.
struct Binding {
    /// The expression to bind.
    expression: Expr,
    /// The type it should be bound as, when `as` named one.
    ascription: Option<Type>,
}

impl Parse for Binding {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let expression: Expr = input.parse()?;
        if let Expr::Cast(cast) = &expression {
            return Ok(Self {
                expression: (*cast.expr).clone(),
                ascription: Some((*cast.ty).clone()),
            });
        }
        let ascription = if input.peek(Token![as]) {
            input.parse::<Token![as]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self {
            expression,
            ascription,
        })
    }
}

/// Expands `sql!`.
pub fn expand(input: TokenStream) -> TokenStream {
    let parsed: SqlInput = match syn::parse2(input) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error(),
    };
    let span = parsed.template.span();
    let pieces = match split(&parsed.template.value(), span) {
        Ok(pieces) => pieces,
        Err(error) => return error.to_compile_error(),
    };

    let private = private_path();
    let mut text = String::new();
    let mut binds = Vec::new();
    for piece in pieces {
        match piece {
            Piece::Text(fragment) => text.push_str(&fragment),
            Piece::Binding(source) => {
                let binding: Binding = match syn::parse_str(&source) {
                    Ok(binding) => binding,
                    Err(error) => {
                        return err(
                            span,
                            &format!("`{{{source}}}` is not a Rust expression: {error}"),
                            "an interpolation is a value, not SQL — `{user.id}`, `{limit as i64}`",
                        )
                        .to_compile_error();
                    }
                };
                text.push('$');
                text.push_str(&(binds.len() + 1).to_string());
                let expression = binding.expression;
                binds.push(match binding.ascription {
                    Some(ty) => quote! {
                        .bind({
                            let __value: #ty = #expression;
                            __value
                        })
                    },
                    None => quote!(.bind(#expression)),
                });
            }
        }
    }

    quote! {
        #private::RawQuery::new(#text) #(#binds)*
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expand_str(source: &str) -> String {
        let input: TokenStream = source.parse().expect("the test source lexes");
        expand(input).to_string()
    }

    #[test]
    fn a_statement_with_no_interpolation_binds_nothing() {
        let out = expand_str(r#""select 1""#);
        assert!(out.contains("RawQuery :: new (\"select 1\")"), "{out}");
        assert!(!out.contains(". bind"), "{out}");
    }

    #[test]
    fn an_interpolation_becomes_a_numbered_placeholder_and_a_bind() {
        let out = expand_str(r#""select * from users where email = {email}""#);
        assert!(
            out.contains("RawQuery :: new (\"select * from users where email = $1\")"),
            "{out}"
        );
        assert!(out.contains(". bind (email)"), "{out}");
        assert!(!out.contains("format !"), "never text interpolation: {out}");
    }

    #[test]
    fn placeholders_are_numbered_in_the_order_they_appear() {
        let out = expand_str(r#""select {a}, {b} where c = {a}""#);
        assert!(out.contains("\"select $1, $2 where c = $3\""), "{out}");
        assert_eq!(out.matches(". bind").count(), 3, "{out}");
    }

    #[test]
    fn an_arbitrary_expression_is_bound_by_value() {
        let out = expand_str(r#""where id = {user.id}""#);
        assert!(out.contains(". bind (user . id)"), "{out}");
    }

    #[test]
    fn an_ascription_tells_inference_what_the_value_is() {
        let out = expand_str(r#""where n = {count as i64}""#);
        assert!(out.contains("let __value : i64 = count"), "{out}");
    }

    #[test]
    fn doubled_braces_are_literal_braces() {
        let out = expand_str(r#""select '{{\"a\": 1}}'::jsonb""#);
        assert!(out.contains(r#"'{\"a\": 1}'::jsonb"#), "{out}");
        assert!(!out.contains(". bind"), "{out}");
    }

    #[test]
    fn an_unclosed_interpolation_says_so() {
        let out = expand_str(r#""select {a""#);
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("never closed"), "{out}");
    }

    #[test]
    fn a_stray_closing_brace_says_so() {
        let out = expand_str(r#""select a}""#);
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("closes nothing"), "{out}");
    }

    #[test]
    fn an_empty_interpolation_says_so() {
        let out = expand_str(r#""select {}""#);
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("binds nothing"), "{out}");
    }

    #[test]
    fn a_non_literal_argument_is_refused_because_it_could_be_runtime_text() {
        let out = expand_str("some_variable");
        assert!(out.contains("compile_error"), "{out}");
        assert!(out.contains("one string literal"), "{out}");
    }

    #[test]
    fn the_splitter_keeps_text_and_bindings_in_order() {
        let span = proc_macro2::Span::call_site();
        let pieces = split("a {b} c", span).expect("a template");
        assert_eq!(
            pieces,
            vec![
                Piece::Text("a ".into()),
                Piece::Binding("b".into()),
                Piece::Text(" c".into()),
            ]
        );
    }
}

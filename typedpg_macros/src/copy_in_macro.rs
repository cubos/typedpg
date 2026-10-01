//! `copy_in!`: bulk-load rows with a binary `COPY ... FROM STDIN`.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen;
use crate::query_macro::{catalog_for, load_config};

/// `copy_in!([db = name,] executor, "table (columns)", source { field, ... })`.
pub struct CopyInInput {
    db_name: Option<Ident>,
    executor: Expr,
    target: LitStr,
    source: Expr,
    fields: Vec<Ident>,
    fields_span: proc_macro2::Span,
}

impl Parse for CopyInInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let db_name = if input.peek(Ident) && input.peek2(Token![=]) {
            let ident: Ident = input.fork().parse()?;
            if ident == "db" {
                input.parse::<Ident>()?;
                input.parse::<Token![=]>()?;
                let name: Ident = input.parse()?;
                input.parse::<Token![,]>()?;
                Some(name)
            } else {
                None
            }
        } else {
            None
        };
        let executor: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let target: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;

        // The source expression, then the `{ field, ... }` projection: the
        // last token tree is the brace group, everything before it the
        // expression.
        let rest: TokenStream = input.parse()?;
        let mut tokens: Vec<TokenTree> = rest.into_iter().collect();
        if matches!(tokens.last(), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
            tokens.pop();
        }
        let Some(TokenTree::Group(group)) = tokens.pop() else {
            return Err(input.error("expected the rows and their fields: `source { field, ... }`"));
        };
        if group.delimiter() != Delimiter::Brace {
            return Err(syn::Error::new(
                group.span(),
                "expected the fields in braces: `source { field, ... }`",
            ));
        }
        if tokens.is_empty() {
            return Err(syn::Error::new(
                group.span(),
                "expected the rows before their fields: `source { field, ... }`",
            ));
        }
        let source: Expr = syn::parse2(tokens.into_iter().collect())?;
        let fields: Punctuated<Ident, Token![,]> =
            syn::parse::Parser::parse2(Punctuated::parse_terminated, group.stream())?;
        Ok(CopyInInput {
            db_name,
            executor,
            target,
            source,
            fields: fields.into_iter().collect(),
            fields_span: group.span(),
        })
    }
}

pub fn expand(input: CopyInInput) -> Result<TokenStream, syn::Error> {
    let config = load_config()?;
    let (catalog, resolved) = catalog_for(&config, input.db_name.as_ref())?;
    let target = catalog
        .analyze_copy_in(&input.target.value())
        .map_err(|e| syn::Error::new(input.target.span(), e.to_string()))?;

    if input.fields.len() != target.columns.len() {
        let columns: Vec<&str> = target.columns.iter().map(|c| c.name.as_str()).collect();
        return Err(syn::Error::new(
            input.fields_span,
            format!(
                "{} field(s) for {} column(s): each row supplies, in order, {}",
                input.fields.len(),
                columns.len(),
                columns.join(", ")
            ),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for field in &input.fields {
        if !seen.insert(field.to_string()) {
            return Err(syn::Error::new(
                field.span(),
                format!("field `{field}` listed more than once"),
            ));
        }
    }

    codegen::generate_copy_in(
        &target,
        &resolved,
        &input.executor,
        &input.source,
        &input.fields,
    )
}

#[cfg(test)]
mod tests {
    use super::CopyInInput;

    fn parse(src: &str) -> syn::Result<CopyInInput> {
        syn::parse_str(src)
    }

    #[test]
    fn parses_the_source_expression_and_its_fields() {
        let input = parse(r#"&pool, "users (name, email)", users { name, email }"#).unwrap();
        assert_eq!(input.target.value(), "users (name, email)");
        let source = &input.source;
        assert_eq!(quote::quote!(#source).to_string(), "users");
        let fields: Vec<String> = input.fields.iter().map(|f| f.to_string()).collect();
        assert_eq!(fields, ["name", "email"]);
        assert!(input.db_name.is_none());

        let input = parse(r#"db = analytics, &tx, "t", rows.iter().take(10) { a, b, },"#).unwrap();
        assert_eq!(input.db_name.unwrap().to_string(), "analytics");
        let source = &input.source;
        assert_eq!(
            quote::quote!(#source).to_string(),
            "rows . iter () . take (10)"
        );
        assert_eq!(input.fields.len(), 2);
    }

    #[test]
    fn rejects_a_missing_field_list() {
        for src in [
            r#"&pool, "users", users"#,
            r#"&pool, "users", users (name, email)"#,
            r#"&pool, "users", { name }"#,
        ] {
            assert!(parse(src).is_err(), "{src}");
        }
    }
}

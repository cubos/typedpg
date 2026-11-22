//! `copy_in!`: bulk-load rows with a binary `COPY ... FROM STDIN`.

use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, Ident, LitStr, Token};

use crate::codegen;
use crate::query_macro::{catalog_for, load_config, parse_prefixes};

/// `copy_in!([db = name,] [role = "role",] executor, "table (columns)",
/// source { field, ... })`.
pub struct CopyInInput {
    db_name: Option<Ident>,
    /// The role the COPY is checked against, replacing the database's
    /// configured `roles`, as `sql!`'s.
    role: Option<LitStr>,
    executor: Expr,
    target: LitStr,
    source: Expr,
    fields: Vec<Ident>,
    fields_span: proc_macro2::Span,
}

impl Parse for CopyInInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let (db_name, role) = parse_prefixes(input)?;
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
            role,
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
    let built = catalog_for(&config, input.db_name.as_ref())?;
    let (catalog, resolved) = (&built.catalog, &built.resolved);
    // As `sql!`: checked against the call site's role, or the database's
    // configured ones, each of which must resolve the target to the same
    // table.
    let roles: Vec<String> = match &input.role {
        Some(role) => vec![role.value()],
        None => resolved.roles().to_vec(),
    };
    let target_text = input.target.value();
    let target = if roles.is_empty() {
        catalog
            .analyze_copy_in(&target_text)
            .map_err(|e| syn::Error::new(input.target.span(), built.annotate(e.to_string())))?
    } else {
        let mut first: Option<(String, typedpg_analyzer::AnalyzedCopyIn)> = None;
        for role in &roles {
            let analyzed = catalog
                .analyze_copy_in_as(&target_text, role)
                .map_err(|e| {
                    let span = match &input.role {
                        Some(literal) if !catalog.role_exists(role) => literal.span(),
                        _ => input.target.span(),
                    };
                    syn::Error::new(span, built.annotate(e.to_string()))
                })?;
            match &first {
                None => first = Some((role.clone(), analyzed)),
                Some((first_role, first)) if first.copy_sql != analyzed.copy_sql => {
                    return Err(syn::Error::new(
                        input.target.span(),
                        format!(
                            "copy target resolves differently for roles {first_role:?} and \
                             {role:?}: `{}` for the first and `{}` for the second (the roles' \
                             search_path settings differ)",
                            first.copy_sql, analyzed.copy_sql
                        ),
                    ));
                }
                Some(_) => {}
            }
        }
        first.expect("at least one role").1
    };

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

    let generated = codegen::generate_copy_in(
        &target,
        resolved,
        &input.executor,
        &input.source,
        &input.fields,
    )?;

    // The target's columns come from the migrations: Cargo must know the
    // crate depends on them, so that a change to one re-runs the analysis.
    let tracked = crate::query_macro::tracked_migrations_items(&built.tracked_migrations);

    Ok(quote::quote! {{
        #(#tracked)*
        #generated
    }})
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

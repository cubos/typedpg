use proc_macro2::TokenStream;
use quote::{quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Fields, GenericParam, parse_quote};

/// The key `typedpg::from_row::Column<K>` uses for the column (or field)
/// named `name` — the name as the Rust identifier of the field (without
/// `r#`). A const generic can't be a string on stable Rust, so the name
/// travels as its 128-bit FNV-1a hash; both sides compute it here.
pub(crate) fn field_key(name: &str) -> proc_macro2::Literal {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013B;
    let mut hash = OFFSET;
    for byte in name.bytes() {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    proc_macro2::Literal::u128_suffixed(hash)
}

pub fn expand(input: DeriveInput) -> Result<TokenStream, syn::Error> {
    let name = &input.ident;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => &fields.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    name,
                    "FromRow can only be derived for structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                name,
                "FromRow can only be derived for structs",
            ));
        }
    };

    let field_extractions: Vec<TokenStream> = fields
        .iter()
        .map(|f| {
            let field_name = f.ident.as_ref().unwrap();
            let col_name = field_name.unraw().to_string();
            quote! {
                #field_name: ::typedpg::__private::read_named_column(__row, #col_name)?
            }
        })
        .collect();

    // `FromQueryRow<__Q>` for every query `__Q` with a column per field
    // that the field's type can hold. The bounds carry the fields' spans,
    // so a mismatch points at the field.
    let mut query_generics = generics.clone();
    query_generics
        .params
        .push(GenericParam::Type(parse_quote!(__TypedpgQuery)));
    let (query_impl_generics, _, _) = query_generics.split_for_impl();
    let mut bounds = TokenStream::new();
    if let Some(wc) = where_clause {
        for p in &wc.predicates {
            bounds.extend(quote! { #p, });
        }
    }
    let mut query_fields = TokenStream::new();
    for f in fields {
        let field_name = f.ident.as_ref().unwrap();
        let key = field_key(&field_name.unraw().to_string());
        let ty = &f.ty;
        let span = f.span();
        bounds.extend(quote_spanned! {span=>
            __TypedpgQuery: ::typedpg::from_row::Column<#key>,
            #ty: ::typedpg::from_row::FromColumn<
                <__TypedpgQuery as ::typedpg::from_row::Column<#key>>::Type,
            >,
        });
        query_fields.extend(quote! {
            #field_name: ::typedpg::from_row::FromColumn::from_column(
                <__TypedpgQuery as ::typedpg::from_row::Column<#key>>::decode(__row)?,
            ),
        });
    }

    // `FromRow::from_row` reads each field with its own `FromSql` impl: it
    // exists when every field has one. The bounds are higher-ranked so that
    // a field without one (an enum, a JSONB-domain struct) leaves the impl
    // unusable rather than failing the derive — `fetch_*_as` only needs
    // `FromQueryRow`, which decodes the way `sql!` does.
    let mut from_row_bounds = TokenStream::new();
    if let Some(wc) = where_clause {
        for p in &wc.predicates {
            from_row_bounds.extend(quote! { #p, });
        }
    }
    for f in fields {
        let ty = &f.ty;
        from_row_bounds.extend(quote! {
            for<'__typedpg_r> #ty: ::typedpg::__private::tokio_postgres::types::FromSql<'__typedpg_r>,
        });
    }

    Ok(quote! {
        impl #impl_generics typedpg::FromRow for #name #ty_generics where #from_row_bounds {
            fn from_row(__row: &::typedpg::__private::tokio_postgres::Row) -> ::std::result::Result<Self, typedpg::Error> {
                ::std::result::Result::Ok(Self {
                    #(#field_extractions),*
                })
            }
        }

        impl #query_impl_generics ::typedpg::from_row::FromQueryRow<__TypedpgQuery>
            for #name #ty_generics
        where
            #bounds
        {
            fn from_query_row(
                __row: &::typedpg::__private::tokio_postgres::Row,
            ) -> ::std::result::Result<Self, ::typedpg::Error> {
                ::std::result::Result::Ok(Self { #query_fields })
            }
        }
    })
}

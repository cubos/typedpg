use proc_macro2::TokenStream;
use quote::quote;
use syn::ext::IdentExt;
use syn::{Data, DeriveInput, Fields};

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

    Ok(quote! {
        impl #impl_generics typedpg::FromRow for #name #ty_generics #where_clause {
            fn from_row(__row: &::typedpg::__private::tokio_postgres::Row) -> ::std::result::Result<Self, typedpg::Error> {
                ::std::result::Result::Ok(Self {
                    #(#field_extractions),*
                })
            }
        }
    })
}

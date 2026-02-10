use proc_macro::TokenStream;
use proc_macro2::TokenTree;
use quote::quote;

#[proc_macro_derive(SEncode)]
pub fn derive_encode(input: TokenStream) -> TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);

    let ret = match ast.data {
        syn::Data::Struct(_) => impl_struct_encode(&ast),
        syn::Data::Enum(_) => impl_enum_encode(&ast),
        syn::Data::Union(..) => Err(syn::Error::new_spanned(
            ast,
            "Union is not supported by SEncode macro".to_string(),
        )),
    };
    match ret {
        Ok(token_stream) => token_stream.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// A simple check for common primitive repr types.
fn is_valid_repr_type(s: &str) -> bool {
    matches!(
        s,
        "u8" | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
    )
}
/// Helper function to extract the Type from the #[repr(...)] attribute
fn get_repr_type(attrs: &[syn::Attribute]) -> Option<syn::Type> {
    for attr in attrs {
        if attr.path().is_ident("repr") {
            // Parse the meta items inside the attribute (e.g., repr(u8))
            if let syn::Meta::List(meta_list) = &attr.meta {
                //eprintln!("{:#?}", meta_list);
                {
                    if let Some(TokenTree::Ident(repr_type)) =
                        meta_list.tokens.clone().into_iter().next()
                    {
                        // Check if the path is a valid primitive (u8, u16, etc.)
                        // and convert the path to a Type
                        // Note: A real implementation should validate this further.

                        let ident_str = repr_type.to_string();
                        if is_valid_repr_type(&ident_str) {
                            return Some(syn::parse_quote! { #repr_type });
                        }
                    }
                }
            }
        }
    }
    None
}

fn impl_enum_encode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_ident = &st.ident;
    let fields = get_variants_from_derive_input(st)?;
    let repr_type = get_repr_type(&st.attrs)
        .ok_or_else(|| syn::Error::new_spanned(st, "Missing repr attribute"))?;
    let builder_struct_fields_def = generate_enum_encode_varints(fields, &repr_type)?;

    //eprintln!("{:#?}",fields);
    //eprintln!("{:#?}",st);
    let ret = quote! {
        impl SEncode for #struct_ident {
            async fn encode<T: tokio::io::AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError> {
                let x = unsafe { *<*const Self>::from(self).cast::<#repr_type>() }.clone();
                x.encode(s).await?;
                match self {
                    #builder_struct_fields_def
                }

                Ok(())
            }
        }
    };

    Ok(ret)
}
fn impl_enum_decode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_ident = &st.ident;
    let fields = get_variants_from_derive_input(st)?;
    let repr_type = get_repr_type(&st.attrs).ok_or_else(|| {
        syn::Error::new_spanned(
            st,
            "Missing repr attribute, adding attribute like `#[repr(u8)]`",
        )
    })?;

    let discrims = generate_enum_discriminants(fields, &repr_type)?;
    let builder_struct_fields_def = generate_enum_decode_varints(fields, &repr_type)?;

    //eprintln!("{:#?}",fields);
    //eprintln!("{:#?}",st);
    let ret = quote! {
        impl SDecode for #struct_ident {
            async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
                let disval = #repr_type::decode(s).await?;

                #discrims

                let ret = match disval {
                    #builder_struct_fields_def
                    _ => return Err(SError::ProtocolViolation),
                };
                Ok(ret)
            }
        }
    };

    Ok(ret)
}

fn impl_struct_encode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_ident = &st.ident;
    let fields = get_fields_from_derive_input(st)?;
    let builder_struct_fields_def = generate_struct_encode_fields(fields)?;

    //eprintln!("{:#?}",st);
    let ret = quote! {
        impl SEncode for #struct_ident {
            async fn encode<T: tokio::io::AsyncWrite + Unpin>(&self, s: &mut T) -> Result<(), SError> {
                #builder_struct_fields_def
                Ok(())
            }
        }
    };

    Ok(ret)
}

type StructFields = syn::punctuated::Punctuated<syn::Field, syn::Token!(,)>;

fn get_fields_from_derive_input(d: &syn::DeriveInput) -> syn::Result<&StructFields> {
    if let syn::Data::Struct(syn::DataStruct {
        fields: syn::Fields::Named(syn::FieldsNamed { ref named, .. }),
        ..
    }) = d.data
    {
        return Ok(named);
    }
    Err(syn::Error::new_spanned(
        d,
        "Must define on a Struct, not Enum".to_string(),
    ))
}
type EnumVariants = syn::punctuated::Punctuated<syn::Variant, syn::Token!(,)>;

fn get_variants_from_derive_input(d: &syn::DeriveInput) -> syn::Result<&EnumVariants> {
    if let syn::Data::Enum(syn::DataEnum {
        variants: ref vars, ..
    }) = d.data
    {
        return Ok(vars);
    }
    Err(syn::Error::new_spanned(
        d,
        "Must define on a Struct, not Enum".to_string(),
    ))
}
fn generate_enum_encode_varints(
    fields: &EnumVariants,
    _repr_type: &syn::Type,
) -> syn::Result<proc_macro2::TokenStream> {
    // eprintln!("{:#?}", idents);

    // eprintln!("{:#?}", fields);
    let mut token_stream = quote! {};
    let unit_idents: Vec<_> = fields
        .iter()
        .filter(|x| x.fields == syn::Fields::Unit)
        .map(|f| &f.ident)
        .collect();
    for ident in unit_idents {
        let tokenstream_piece = quote! {
            Self::#ident => { },

        };
        token_stream.extend(tokenstream_piece);
    }
    for idents in fields
        .iter()
        .filter(|x| x.fields != syn::Fields::Unit)
        .map(|f| &f.ident)
    {
        let tokenstream_piece = quote! {
            Self::#idents(val) => {
                val.encode(s).await?;
            },

        };
        token_stream.extend(tokenstream_piece);
    }
    Ok(token_stream)
}

fn generate_enum_decode_varints(
    fields: &EnumVariants,
    _repr_type: &syn::Type,
) -> syn::Result<proc_macro2::TokenStream> {
    // eprintln!("{:#?}", idents);

    // eprintln!("{:#?}", fields);
    let fields = fields.iter();
    let mut token_stream = quote! {};
    for ident in fields {
        if ident.fields == syn::Fields::Unit {
            //eprintln!("{:#?}", ident);
            let ident = &ident.ident;
            let ident_name = quote::format_ident!("{}_TAG", ident)
                .to_string()
                .to_uppercase();
            let ident_name = quote::format_ident!("{}", ident_name);
            let tokenstream_piece = quote! {
                #ident_name => Self::#ident,
            };
            token_stream.extend(tokenstream_piece);
        } else {
            //eprintln!("{:#?}", ident);
            let ident_name = ident.ident.clone();
            let ident_tag = quote::format_ident!("{}_TAG", ident.ident.to_string().to_uppercase());
            let field_type = ident.fields.iter().next().unwrap().ty.clone();
            if ident.fields.iter().count() > 1 {
                return Err(syn::Error::new_spanned(
                    ident,
                    "Only one field is supported for non-unit variants".to_string(),
                ));
            }
            let tokenstream_piece = quote! {
                #ident_tag => {
                    let val = <#field_type as SDecode>::decode(s).await?;
                    Self::#ident_name(val)
                },

            };
            token_stream.extend(tokenstream_piece);
        }
    }

    Ok(token_stream)
}

fn generate_enum_discriminants(
    fields: &EnumVariants,
    repr: &syn::Type,
) -> syn::Result<proc_macro2::TokenStream> {
    // eprintln!("{:#?}", idents);

    //eprintln!("{:#?}", fields);
    let mut ret = quote! {};
    let mut counter = 0;
    let mut lit = syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Int(syn::LitInt::new("0", proc_macro2::Span::call_site())),
        attrs: vec![],
    });
    for idents in fields {
        if let Some((
            _,
            lit_int, // syn::Expr::Lit(syn::ExprLit {
                     //     lit: syn::Lit::Int(lit_int),
                     //     ..
                     // }),
        )) = &idents.discriminant
        {
            let ident = &idents.ident;
            let tag_ident = quote::format_ident!("{}_TAG", ident.to_string().to_uppercase());
            ret.extend(quote! {
                const #tag_ident : #repr = #lit_int;
            });
            counter = 1;
            lit = lit_int.clone();

            //eprintln!("{:#?}", lit_int);
        } else {
            let ident = &idents.ident;
            let tag_ident = quote::format_ident!("{}_TAG", ident.to_string().to_uppercase());
            ret.extend(quote! {
                      const #tag_ident : #repr = #lit + (#counter as #repr);
            });
            counter += 1;
        }
    }
    //eprint!("{:#?}", ret);
    Ok(ret)
}

fn generate_struct_encode_fields(fields: &StructFields) -> syn::Result<proc_macro2::TokenStream> {
    let idents: Vec<_> = fields.iter().map(|f| &f.ident).collect();
    let types: Vec<_> = fields.iter().map(|f| &f.ty).collect();

    let mut token_stream = quote! {};
    for (ident, _type) in idents.iter().zip(types.iter()) {
        let tokenstream_piece = quote! {
            self.#ident.encode(s).await?;

        };
        token_stream.extend(tokenstream_piece);
    }
    Ok(token_stream)
}

#[proc_macro_derive(SDecode)]
pub fn derive_decode(input: TokenStream) -> TokenStream {
    let ast = syn::parse_macro_input!(input as syn::DeriveInput);

    let ret = match ast.data {
        syn::Data::Struct(_) => impl_struct_decode(&ast),
        syn::Data::Enum(_) => impl_enum_decode(&ast),
        syn::Data::Union(..) => Err(syn::Error::new_spanned(
            ast,
            "Union is not supported by SEncode macro".to_string(),
        )),
    };
    match ret {
        Ok(token_stream) => token_stream.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn impl_struct_decode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let struct_ident = &st.ident;
    let fields = get_fields_from_derive_input(st)?;
    let builder_struct_fields_def = generate_decode_fields(fields)?;

    //eprintln!("{:#?}",st);
    let ret = quote! {
        impl SDecode for #struct_ident {
            async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
               Ok(Self {
                #builder_struct_fields_def
               })
            }
        }
    };

    Ok(ret)
}

fn generate_decode_fields(fields: &StructFields) -> syn::Result<proc_macro2::TokenStream> {
    let idents: Vec<_> = fields.iter().map(|f| &f.ident).collect();
    let types: Vec<_> = fields.iter().map(|f| &f.ty).collect();

    let mut token_stream = quote! {};
    for (ident, type_) in idents.iter().zip(types.iter()) {
        let tokenstream_piece = quote! {
            #ident: #type_::decode(s).await?,
        };

        token_stream.extend(tokenstream_piece);
    }
    Ok(token_stream)
}

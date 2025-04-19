use proc_macro::TokenStream;
use syn;
use quote::quote;

#[proc_macro_derive(SEncode)]
pub fn derive_encode(input: TokenStream) -> TokenStream {
    let st = syn::parse_macro_input!(input as syn::DeriveInput);

    match do_expand_encode(&st) {
        Ok(token_stream) => token_stream.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn do_expand_encode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {

    let struct_ident = &st.ident;  
    let fields = get_fields_from_derive_input(st)?;
    let builder_struct_fields_def = generate_encode_fields(fields)?;

    //eprintln!("{:#?}",st);
    let ret = quote! {                                               
        impl SEncode for #struct_ident {                          
            async fn encode<T: tokio::io::AsyncWrite + Unpin>(self, s: &mut T) -> Result<(), SError> {
                #builder_struct_fields_def
                Ok(())
            }  
        }                                     
    };                                 

    return Ok(ret);
}


type StructFields = syn::punctuated::Punctuated<syn::Field,syn::Token!(,)>;

fn get_fields_from_derive_input(d: &syn::DeriveInput) -> syn::Result<&StructFields> {
    if let syn::Data::Struct(syn::DataStruct {
        fields: syn::Fields::Named(syn::FieldsNamed { ref named, .. }),
        ..
    }) = d.data{
        return Ok(named)
    }
    Err(syn::Error::new_spanned(d, "Must define on a Struct, not Enum".to_string()))
}


fn generate_encode_fields(fields: &StructFields) -> syn::Result<proc_macro2::TokenStream>{
    let idents:Vec<_> = fields.iter().map(|f| {&f.ident}).collect();
    let types:Vec<_> = fields.iter().map(|f| {&f.ty}).collect();

    let mut token_stream = quote!{
    };
    for (ident, _type) in idents.iter().zip(types.iter()) {
        let tokenstream_piece = quote!{
            self.#ident.encode(s).await?;
            
        };
        token_stream.extend(tokenstream_piece);
    }
    Ok(token_stream)
}

#[proc_macro_derive(SDecode)]
pub fn derive_decode(input: TokenStream) -> TokenStream {
    let st = syn::parse_macro_input!(input as syn::DeriveInput);

    match do_expand_decode(&st) {
        Ok(token_stream) => token_stream.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn do_expand_decode(st: &syn::DeriveInput) -> syn::Result<proc_macro2::TokenStream> {

    let struct_ident = &st.ident;  
    let fields = get_fields_from_derive_input(st)?;
    let builder_struct_fields_def = generate_decode_fields(fields)?;

    eprintln!("{:#?}",st);
    let ret = quote! {                                             
        impl SDecode for #struct_ident {                               
            async fn decode<T: tokio::io::AsyncRead + Unpin>(s: &mut T) -> Result<Self, SError> {
               Ok(Self {
                #builder_struct_fields_def
               }) 
            }  
        }                            
    };                                   

    return Ok(ret);
}

fn generate_decode_fields(fields: &StructFields) -> syn::Result<proc_macro2::TokenStream>{
    let idents:Vec<_> = fields.iter().map(|f| {&f.ident}).collect();
    let types:Vec<_> = fields.iter().map(|f| {&f.ty}).collect();

    let mut token_stream = quote!{
    };
    for (ident, type_) in idents.iter().zip(types.iter()) {
        let tokenstream_piece = quote!{
            #ident: #type_::decode(s).await?,
        };

        token_stream.extend(tokenstream_piece);
    }
    Ok(token_stream)
}
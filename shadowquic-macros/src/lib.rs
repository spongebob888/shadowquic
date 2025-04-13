use proc_macro::TokenStream;
use syn;

#[proc_macro_derive(SEncoder)] // 注意，这里和第一篇文章里的 #[proc_macro_attribute]不同
pub fn derive(input: TokenStream) -> TokenStream {
    let st = syn::parse_macro_input!(input as syn::DeriveInput);

    TokenStream::new()
}

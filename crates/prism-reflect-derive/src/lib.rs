//! `#[derive(Reflect)]` generates `prism_reflect::Reflect` and
//! `prism_reflect::ReflectType` implementations for structs.
//!
//! ```ignore
//! #[derive(Default, Reflect)]
//! struct Camera {
//!     pub fov_y_degrees: f32,
//!     #[reflect(skip)]
//!     pub runtime_cache: u64,
//! }
//! ```
//!
//! Container attributes:
//!
//! - `#[reflect(name = "camera")]` overrides the stable type name, which
//!   defaults to `module_path!()::Type`.
//! - `#[reflect(version = 2)]` sets the type version stored in `TypeDesc` and
//!   used for migration.
//!
//! Field attributes:
//!
//! - `#[reflect(skip)]` excludes the field from reflection; `from_object`
//!   leaves it at its `Default` value.
//! - `#[reflect(rename = "fov")]` sets the name the field uses in the schema.
//! - `#[reflect(read_only)]` makes `set_field` reject writes.
//!
//! Only named and tuple structs are supported; generic types, enums and unions
//! are rejected with a compile error.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::spanned::Spanned;
use syn::{parse_macro_input, Data, DeriveInput, LitInt, LitStr};

#[proc_macro_derive(Reflect, attributes(reflect))]
pub fn derive_reflect(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;

    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "Reflect does not support generic types yet",
        ));
    }

    let mut custom_name: Option<String> = None;
    let mut version: u32 = 1;

    for attr in &input.attrs {
        if !attr.path().is_ident("reflect") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let lit: LitStr = meta.value()?.parse()?;
                custom_name = Some(lit.value());
                Ok(())
            } else if meta.path.is_ident("version") {
                let lit: LitInt = meta.value()?.parse()?;
                version = lit.base10_parse()?;
                Ok(())
            } else {
                Err(meta.error("unknown container attribute; supported: name, version"))
            }
        })?;
    }

    let fields = match &input.data {
        Data::Struct(data) => &data.fields,
        _ => {
            return Err(syn::Error::new(
                input.span(),
                "Reflect can only be derived for structs",
            ))
        }
    };

    let name_tokens = match custom_name {
        Some(name) => quote!(#name),
        None => quote!(concat!(module_path!(), "::", stringify!(#ident))),
    };

    let mut descriptors = Vec::new();
    let mut getters = Vec::new();
    let mut setters = Vec::new();
    let mut force_setters = Vec::new();
    let mut field_names = std::collections::HashSet::new();

    for (index, field) in fields.iter().enumerate() {
        let mut skip = false;
        let mut rename: Option<String> = None;
        let mut read_only = false;

        for attr in &field.attrs {
            if !attr.path().is_ident("reflect") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("skip") {
                    skip = true;
                    Ok(())
                } else if meta.path.is_ident("rename") {
                    let lit: LitStr = meta.value()?.parse()?;
                    rename = Some(lit.value());
                    Ok(())
                } else if meta.path.is_ident("read_only") {
                    read_only = true;
                    Ok(())
                } else {
                    Err(meta.error("unknown field attribute; supported: skip, rename, read_only"))
                }
            })?;
        }

        if skip {
            continue;
        }

        let access = match &field.ident {
            Some(name) => quote!(#name),
            None => {
                let idx = syn::Index::from(index);
                quote!(#idx)
            }
        };
        let field_name = rename.unwrap_or_else(|| match &field.ident {
            Some(name) => name.to_string(),
            None => index.to_string(),
        });
        if !field_names.insert(field_name.clone()) {
            return Err(syn::Error::new(
                field.span(),
                format!(
                    "duplicate reflect field name `{field_name}`; \
                     use #[reflect(rename = \"...\")] to disambiguate"
                ),
            ));
        }
        let ty = &field.ty;

        descriptors.push(quote! {
            ::prism_reflect::FieldDesc {
                name: #field_name,
                offset: ::core::mem::offset_of!(Self, #access),
                ty: <#ty as ::prism_reflect::ReflectType>::field_type(),
                read_only: #read_only,
                enum_variants: <#ty as ::prism_reflect::ReflectType>::enum_variants(),
            }
        });

        getters.push(quote! {
            #field_name => ::core::option::Option::Some(
                <#ty as ::prism_reflect::ReflectType>::to_value(&self.#access)
            ),
        });

        if read_only {
            setters.push(quote! {
                #field_name => {
                    let _ = value;
                    ::core::result::Result::Err(
                        ::prism_reflect::ReflectError::ReadOnly(#field_name.to_owned())
                    )
                }
            });
        } else {
            setters.push(quote! {
                #field_name => {
                    self.#access = <#ty as ::prism_reflect::ReflectType>::from_value(value)?;
                    ::core::result::Result::Ok(())
                }
            });
        }

        force_setters.push(quote! {
            #field_name => {
                self.#access = <#ty as ::prism_reflect::ReflectType>::from_value(value)?;
                ::core::result::Result::Ok(())
            }
        });
    }

    Ok(quote! {
        impl ::prism_reflect::Reflect for #ident {
            fn type_desc() -> &'static ::prism_reflect::TypeDesc {
                static DESC: ::std::sync::OnceLock<::prism_reflect::TypeDesc> =
                    ::std::sync::OnceLock::new();
                DESC.get_or_init(|| ::prism_reflect::TypeDesc {
                    id: ::prism_reflect::TypeId::from_name(#name_tokens),
                    name: #name_tokens,
                    version: #version,
                    size: ::core::mem::size_of::<Self>(),
                    align: ::core::mem::align_of::<Self>(),
                    kind: ::prism_reflect::TypeKind::Struct,
                    fields: ::std::vec![#(#descriptors),*],
                })
            }

            fn get_field(&self, field: &str) -> ::core::option::Option<::prism_reflect::Value> {
                match field {
                    #(#getters)*
                    _ => ::core::option::Option::None,
                }
            }

            fn set_field(
                &mut self,
                field: &str,
                value: ::prism_reflect::Value,
            ) -> ::core::result::Result<(), ::prism_reflect::ReflectError> {
                match field {
                    #(#setters)*
                    _ => ::core::result::Result::Err(
                        ::prism_reflect::ReflectError::UnknownField(field.to_owned())
                    ),
                }
            }

            fn set_field_force(
                &mut self,
                field: &str,
                value: ::prism_reflect::Value,
            ) -> ::core::result::Result<(), ::prism_reflect::ReflectError> {
                match field {
                    #(#force_setters)*
                    _ => ::core::result::Result::Err(
                        ::prism_reflect::ReflectError::UnknownField(field.to_owned())
                    ),
                }
            }
        }

        impl ::prism_reflect::ReflectType for #ident {
            fn field_type() -> ::prism_reflect::FieldType {
                ::prism_reflect::FieldType::Struct(
                    <Self as ::prism_reflect::Reflect>::type_desc().id
                )
            }

            fn to_value(&self) -> ::prism_reflect::Value {
                ::prism_reflect::Value::Object(
                    <Self as ::prism_reflect::Reflect>::to_object(self)
                )
            }

            fn from_value(
                value: ::prism_reflect::Value,
            ) -> ::core::result::Result<Self, ::prism_reflect::ReflectError> {
                match value {
                    ::prism_reflect::Value::Object(object) => {
                        <Self as ::prism_reflect::Reflect>::from_object(&object)
                    }
                    other => ::core::result::Result::Err(
                        ::prism_reflect::ReflectError::TypeMismatch {
                            expected: "object",
                            got: other.kind(),
                        }
                    ),
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::expand;

    fn expand_str(source: &str) -> syn::Result<proc_macro2::TokenStream> {
        let input: syn::DeriveInput = syn::parse_str(source).expect("test input parses");
        expand(input)
    }

    #[test]
    fn duplicate_renamed_fields_are_rejected() {
        let error = expand_str(
            r#"
            struct Camera {
                #[reflect(rename = "fov")]
                fov_y: f32,
                #[reflect(rename = "fov")]
                fov_x: f32,
            }
            "#,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("duplicate reflect field name `fov`"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn duplicate_numeric_renames_are_rejected() {
        let error = expand_str(
            r#"
            struct Camera {
                #[reflect(rename = "0")]
                a: f32,
                #[reflect(rename = "0")]
                b: f32,
            }
            "#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate reflect field name"));
    }

    #[test]
    fn unique_fields_still_expand() {
        expand_str(
            r#"
            struct Camera {
                fov: f32,
                #[reflect(rename = "near")]
                near_plane: f32,
            }
            "#,
        )
        .expect("unique field names expand");
    }
}

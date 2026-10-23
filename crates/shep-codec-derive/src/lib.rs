//! Derive macros for `shep-codec`. Use them through its re-exports,
//! `shep_codec::Encode` and `shep_codec::Decode`, whose docs describe the
//! format.
//!
//! Attributes:
//!
//! - container: `#[codec(version = N, compat = M)]` (both default to 0, and
//!   `compat` may not exceed `version`), or `#[codec(transparent)]` on a struct
//!   with exactly one field.
//! - field: `#[codec(default)]` — old data may lack the field; it then takes
//!   its `Default`. Only trailing fields may be marked, since a field can only
//!   be missing if every field after it is too.
//! - variant: `#[codec(tag = N)]` — the variant's tag (default: declaration
//!   index). Tags must be unique.

use proc_macro2::{Literal, TokenStream, TokenTree};
use quote::{format_ident, quote};
use syn::meta::ParseNestedMeta;
use syn::spanned::Spanned;
use syn::{
    Data, DeriveInput, Error, Fields, GenericParam, Generics, Ident, LitInt, Member, Result, Type,
    WherePredicate, parse_macro_input, parse_quote,
};

#[proc_macro_derive(Encode, attributes(codec))]
pub fn derive_encode(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    Container::parse(&input)
        .map(|c| c.encode_impl())
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

#[proc_macro_derive(Decode, attributes(codec))]
pub fn derive_decode(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    Container::parse(&input)
        .map(|c| c.decode_impl())
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

/// A type the derive was applied to, with its attributes checked.
struct Container<'a> {
    ident: &'a Ident,
    generics: &'a Generics,
    version: u8,
    compat: u8,
    shape: Shape<'a>,
}

enum Shape<'a> {
    /// A struct with an envelope.
    Struct(FieldList<'a>),
    /// A single-field struct encoded as its field.
    Transparent(Field<'a>),
    Enum(Vec<Variant<'a>>),
}

struct FieldList<'a> {
    fields: Vec<Field<'a>>,
}

struct Field<'a> {
    member: Member,
    ty: &'a Type,
    default: bool,
}

struct Variant<'a> {
    ident: &'a Ident,
    tag: u8,
    fields: FieldList<'a>,
}

impl<'a> Container<'a> {
    fn parse(input: &'a DeriveInput) -> Result<Container<'a>> {
        let mut version = None;
        let mut compat = None;
        let mut transparent = None;
        for attr in input.attrs.iter().filter(|a| a.path().is_ident("codec")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("version") {
                    version = Some(parse_u8(&meta, "a version")?);
                } else if meta.path.is_ident("compat") {
                    compat = Some(parse_u8(&meta, "a compat version")?);
                } else if meta.path.is_ident("transparent") {
                    transparent = Some(meta.path.span());
                } else {
                    return Err(meta.error(
                        "unknown codec attribute; expected `version`, `compat`, or `transparent`",
                    ));
                }
                Ok(())
            })?;
        }

        if let Some(span) = transparent
            && (version.is_some() || compat.is_some())
        {
            return Err(Error::new(
                span,
                "a transparent struct has no envelope, so it can't have a version",
            ));
        }
        let (version, compat) = (version.unwrap_or(0), compat.unwrap_or(0));
        if compat > version {
            return Err(Error::new(
                input.ident.span(),
                format!(
                    "compat ({compat}) exceeds version ({version}): this definition couldn't read its own data"
                ),
            ));
        }

        let shape = match &input.data {
            Data::Struct(data) => {
                let fields = FieldList::parse(&data.fields)?;
                match transparent {
                    Some(span) => {
                        let mut fields = fields.fields;
                        if fields.len() != 1 {
                            return Err(Error::new(
                                span,
                                "`transparent` requires exactly one field",
                            ));
                        }
                        let field = fields.remove(0);
                        if field.default {
                            return Err(Error::new(
                                field.ty.span(),
                                "a transparent struct's field is always present; remove `default`",
                            ));
                        }
                        Shape::Transparent(field)
                    }
                    None => Shape::Struct(fields),
                }
            }
            Data::Enum(data) => {
                if let Some(span) = transparent {
                    return Err(Error::new(span, "`transparent` applies only to structs"));
                }
                let mut variants: Vec<Variant<'_>> = Vec::new();
                for (index, variant) in data.variants.iter().enumerate() {
                    let mut tag = None;
                    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("codec")) {
                        attr.parse_nested_meta(|meta| {
                            if meta.path.is_ident("tag") {
                                tag = Some(parse_u8(&meta, "a tag")?);
                                Ok(())
                            } else {
                                Err(meta.error("unknown codec attribute; expected `tag`"))
                            }
                        })?;
                    }
                    let tag = match tag {
                        Some(tag) => tag,
                        None => u8::try_from(index).map_err(|_| {
                            Error::new(
                                variant.ident.span(),
                                "more than 256 variants; tags are a u8",
                            )
                        })?,
                    };
                    if let Some(other) = variants.iter().find(|v| v.tag == tag) {
                        return Err(Error::new(
                            variant.ident.span(),
                            format!("tag {tag} is already used by variant `{}`", other.ident),
                        ));
                    }
                    variants.push(Variant {
                        ident: &variant.ident,
                        tag,
                        fields: FieldList::parse(&variant.fields)?,
                    });
                }
                Shape::Enum(variants)
            }
            Data::Union(data) => {
                return Err(Error::new(
                    data.union_token.span,
                    "shep-codec can't derive for unions",
                ));
            }
        };

        Ok(Container {
            ident: &input.ident,
            generics: &input.generics,
            version,
            compat,
            shape,
        })
    }

    fn encode_impl(&self) -> TokenStream {
        let (encode, encoded_len) = match &self.shape {
            Shape::Transparent(field) => {
                let member = &field.member;
                (
                    quote!(::shep_codec::Encode::encode(&self.#member, buf)),
                    quote!(::shep_codec::Encode::encoded_len(&self.#member)),
                )
            }
            Shape::Struct(fields) => {
                let pattern = fields.pattern(quote!(Self));
                let body = fields.encode_each();
                let lens = fields.len_each();
                let (version, compat) = self.versions();
                (
                    quote! {
                        let #pattern = self;
                        ::shep_codec::__private::encode_envelope(buf, #version, #compat, |buf| {
                            #body
                        });
                    },
                    quote! {
                        let #pattern = self;
                        ::shep_codec::__private::ENVELOPE_HEADER_LEN #lens
                    },
                )
            }
            Shape::Enum(variants) => {
                let (version, compat) = self.versions();
                let encode_arms = variants.iter().map(|v| {
                    let ident = v.ident;
                    let pattern = v.fields.pattern(quote!(Self::#ident));
                    let tag = Literal::u8_suffixed(v.tag);
                    let body = v.fields.encode_each();
                    quote! {
                        #pattern => ::shep_codec::__private::encode_envelope(buf, #version, #compat, |buf| {
                            ::shep_codec::Encode::encode(&#tag, buf);
                            #body
                        }),
                    }
                });
                let len_arms = variants.iter().map(|v| {
                    let ident = v.ident;
                    let pattern = v.fields.pattern(quote!(Self::#ident));
                    let lens = v.fields.len_each();
                    quote!(#pattern => ::shep_codec::__private::ENVELOPE_HEADER_LEN + 1 #lens,)
                });
                // A match with no arms is exhaustive only on the place itself,
                // not on a reference to it.
                let scrutinee = if variants.is_empty() {
                    quote!(*self)
                } else {
                    quote!(self)
                };
                (
                    quote!(match #scrutinee { #(#encode_arms)* }),
                    quote!(match #scrutinee { #(#len_arms)* }),
                )
            }
        };

        let ident = self.ident;
        let generics = self.bounded(quote!(::shep_codec::Encode), false);
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        quote! {
            #[automatically_derived]
            impl #impl_generics ::shep_codec::Encode for #ident #ty_generics #where_clause {
                fn encode(&self, buf: &mut ::std::vec::Vec<u8>) {
                    #encode
                }

                fn encoded_len(&self) -> usize {
                    #encoded_len
                }
            }
        }
    }

    fn decode_impl(&self) -> TokenStream {
        let p = quote!(::shep_codec::__private);
        let type_name = quote!(#p::type_name::<Self>());
        let decode = match &self.shape {
            Shape::Transparent(field) => {
                let member = &field.member;
                quote! {
                    #p::Ok(Self { #member: #p::decode_transparent(d)? })
                }
            }
            Shape::Struct(fields) => {
                let (version, compat) = self.versions();
                let body = if fields.fields.is_empty() {
                    quote!(_d)
                } else {
                    quote!(d)
                };
                let decode_fields = fields.decode_each(&type_name, None);
                let construct = fields.pattern(quote!(Self));
                quote! {
                    #p::decode_envelope(d, #type_name, #version, #compat, |#body| {
                        #decode_fields
                        #p::Ok(#construct)
                    })
                }
            }
            Shape::Enum(variants) => {
                let (version, compat) = self.versions();
                let arms = variants.iter().map(|v| {
                    let ident = v.ident;
                    let tag = Literal::u8_suffixed(v.tag);
                    let decode_fields = v.fields.decode_each(&type_name, Some(ident));
                    let construct = v.fields.pattern(quote!(Self::#ident));
                    quote! {
                        #tag => {
                            #decode_fields
                            #p::Ok(#construct)
                        }
                    }
                });
                quote! {
                    #p::decode_envelope(d, #type_name, #version, #compat, |d| {
                        let tag: u8 = ::shep_codec::Decode::decode(d)?;
                        match tag {
                            #(#arms)*
                            tag => #p::Err(::shep_codec::DecodeError::UnknownTag {
                                type_name: #type_name,
                                tag,
                            }),
                        }
                    })
                }
            }
        };

        let ident = self.ident;
        let generics = self.bounded(quote!(::shep_codec::Decode), true);
        let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();
        quote! {
            #[automatically_derived]
            impl #impl_generics ::shep_codec::Decode for #ident #ty_generics #where_clause {
                fn decode(
                    d: &mut #p::Decoder<'_>,
                ) -> #p::Result<Self, ::shep_codec::DecodeError> {
                    #decode
                }
            }
        }
    }

    fn versions(&self) -> (Literal, Literal) {
        (
            Literal::u8_suffixed(self.version),
            Literal::u8_suffixed(self.compat),
        )
    }

    /// The type's generics with `bound` on every type parameter, and, when
    /// decoding, `Default` on every defaulted field whose type mentions one.
    fn bounded(&self, bound: TokenStream, decoding: bool) -> Generics {
        let mut generics = self.generics.clone();
        let params: Vec<Ident> = generics
            .params
            .iter()
            .filter_map(|p| match p {
                GenericParam::Type(t) => Some(t.ident.clone()),
                _ => None,
            })
            .collect();
        let mut predicates: Vec<WherePredicate> = params
            .iter()
            .map(|ident| parse_quote!(#ident: #bound))
            .collect();
        if decoding {
            for field in self.fields().into_iter().filter(|f| f.default) {
                let ty = field.ty;
                if mentions(quote!(#ty).into_iter(), &params) {
                    predicates.push(parse_quote!(#ty: ::std::default::Default));
                }
            }
        }
        generics.make_where_clause().predicates.extend(predicates);
        generics
    }

    fn fields(&self) -> Vec<&Field<'a>> {
        match &self.shape {
            Shape::Struct(fields) => fields.fields.iter().collect(),
            Shape::Transparent(field) => vec![field],
            Shape::Enum(variants) => variants.iter().flat_map(|v| &v.fields.fields).collect(),
        }
    }
}

/// Parses `= N` where `N` fits the wire's `u8`.
fn parse_u8(meta: &ParseNestedMeta<'_>, what: &str) -> Result<u8> {
    let lit = meta.value()?.parse::<LitInt>()?;
    lit.base10_parse::<u8>()
        .map_err(|_| Error::new(lit.span(), format!("{what} is a u8 (0..=255)")))
}

fn mentions(tokens: impl Iterator<Item = TokenTree>, params: &[Ident]) -> bool {
    tokens.into_iter().any(|t| match t {
        TokenTree::Ident(ident) => params.contains(&ident),
        TokenTree::Group(group) => mentions(group.stream().into_iter(), params),
        _ => false,
    })
}

impl<'a> FieldList<'a> {
    fn parse(fields: &'a Fields) -> Result<FieldList<'a>> {
        let mut out = Vec::new();
        let mut first_default: Option<&syn::Field> = None;
        for (index, field) in fields.iter().enumerate() {
            let mut default = false;
            for attr in field.attrs.iter().filter(|a| a.path().is_ident("codec")) {
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("default") {
                        default = true;
                        Ok(())
                    } else {
                        Err(meta.error("unknown codec attribute; expected `default`"))
                    }
                })?;
            }
            if default {
                first_default.get_or_insert(field);
            } else if let Some(earlier) = first_default {
                let span = earlier
                    .ident
                    .as_ref()
                    .map_or_else(|| earlier.ty.span(), Ident::span);
                return Err(Error::new(
                    span,
                    "`#[codec(default)]` fields must be trailing: a field can only be missing \
                     from old data if every field after it is too",
                ));
            }
            let member = match &field.ident {
                Some(ident) => Member::Named(ident.clone()),
                None => Member::Unnamed(index.into()),
            };
            out.push(Field {
                member,
                ty: &field.ty,
                default,
            });
        }
        Ok(FieldList { fields: out })
    }

    fn binding(index: usize) -> Ident {
        format_ident!("__field{}", index)
    }

    /// `path { member: __field0, .. }`, usable as a pattern binding every
    /// field by reference, and as a constructor.
    fn pattern(&self, path: TokenStream) -> TokenStream {
        let entries = self.fields.iter().enumerate().map(|(i, f)| {
            let member = &f.member;
            let binding = Self::binding(i);
            quote!(#member: #binding)
        });
        quote!(#path { #(#entries),* })
    }

    fn encode_each(&self) -> TokenStream {
        let bindings = (0..self.fields.len()).map(Self::binding);
        quote!(#(::shep_codec::Encode::encode(#bindings, buf);)*)
    }

    fn len_each(&self) -> TokenStream {
        let bindings = (0..self.fields.len()).map(Self::binding);
        quote!(#(+ ::shep_codec::Encode::encoded_len(#bindings))*)
    }

    fn decode_each(&self, type_name: &TokenStream, variant: Option<&Ident>) -> TokenStream {
        let lets = self.fields.iter().enumerate().map(|(i, f)| {
            let binding = Self::binding(i);
            if f.default {
                quote! {
                    let #binding = ::shep_codec::__private::decode_field_or_default(d)?;
                }
            } else {
                let member = match &f.member {
                    Member::Named(ident) => ident.to_string(),
                    Member::Unnamed(index) => index.index.to_string(),
                };
                let name = match variant {
                    Some(variant) => format!("{variant}.{member}"),
                    None => member,
                };
                quote! {
                    let #binding = ::shep_codec::__private::decode_field(d, #type_name, #name)?;
                }
            }
        });
        quote!(#(#lets)*)
    }
}

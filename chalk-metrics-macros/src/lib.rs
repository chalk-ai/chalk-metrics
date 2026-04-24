use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{
    Error, Ident, LitStr, Path, Result, Token, Visibility, braced, bracketed, parenthesized,
    parse_macro_input,
};

mod kw {
    syn::custom_keyword!(group);
    syn::custom_keyword!(namespace);
    syn::custom_keyword!(parent);
    syn::custom_keyword!(tags);
    syn::custom_keyword!(optional);
}

#[proc_macro]
pub fn define_tags(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as TagsInput);
    expand_tags(input).into()
}

#[proc_macro]
pub fn define_namespaces(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as NamespacesInput);
    expand_namespaces(input).into()
}

#[proc_macro]
pub fn define_metrics(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as MetricsInput);
    match expand_metrics(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

struct TagsInput {
    tags: Vec<TagDef>,
}

struct TagDef {
    vis: Visibility,
    ident: Ident,
    export_name: LitStr,
    values: Option<Vec<(Ident, LitStr)>>,
}

impl Parse for TagsInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut tags = Vec::new();
        while !input.is_empty() {
            let vis: Visibility = input.parse()?;
            let ident: Ident = input.parse()?;
            input.parse::<Token![=>]>()?;
            let export_name: LitStr = input.parse()?;

            let values = if input.peek(syn::token::Brace) {
                let content;
                braced!(content in input);
                let mut values = Vec::new();
                while !content.is_empty() {
                    let variant: Ident = content.parse()?;
                    content.parse::<Token![=>]>()?;
                    let value: LitStr = content.parse()?;
                    if content.peek(Token![,]) {
                        content.parse::<Token![,]>()?;
                    }
                    values.push((variant, value));
                }
                Some(values)
            } else {
                input.parse::<Token![;]>()?;
                None
            };

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }

            tags.push(TagDef {
                vis,
                ident,
                export_name,
                values,
            });
        }
        Ok(Self { tags })
    }
}

fn expand_tags(input: TagsInput) -> TokenStream2 {
    let tags = input.tags.into_iter().map(|tag| {
        let vis = tag.vis;
        let ident = tag.ident;
        let export_name = tag.export_name;

        if let Some(values) = tag.values {
            let variants = values.iter().map(|(variant, _)| variant);
            let match_arms = values.iter().map(|(variant, value)| {
                quote! { Self::#variant => #value, }
            });

            quote! {
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
                #vis enum #ident {
                    #( #variants, )*
                }

                impl #ident {
                    pub const EXPORT_NAME: &'static str = #export_name;

                    pub fn as_str(&self) -> &'static str {
                        match self {
                            #( #match_arms )*
                        }
                    }
                }

                impl ::std::fmt::Display for #ident {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        f.write_str(self.as_str())
                    }
                }

                impl chalk_metrics::__private::MetricTag for #ident {
                    const EXPORT_NAME: &'static str = #export_name;

                    fn export_value(&self) -> ::std::borrow::Cow<'static, str> {
                        ::std::borrow::Cow::Borrowed(self.as_str())
                    }
                }
            }
        } else {
            quote! {
                #[derive(Debug, Clone, PartialEq, Eq, Hash)]
                #vis struct #ident(pub String);

                impl #ident {
                    pub const EXPORT_NAME: &'static str = #export_name;

                    pub fn as_str(&self) -> &str {
                        &self.0
                    }
                }

                impl ::std::fmt::Display for #ident {
                    fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        f.write_str(&self.0)
                    }
                }

                impl From<&str> for #ident {
                    fn from(s: &str) -> Self {
                        Self(s.to_owned())
                    }
                }

                impl From<String> for #ident {
                    fn from(s: String) -> Self {
                        Self(s)
                    }
                }

                impl chalk_metrics::__private::MetricTag for #ident {
                    const EXPORT_NAME: &'static str = #export_name;

                    fn export_value(&self) -> ::std::borrow::Cow<'static, str> {
                        ::std::borrow::Cow::Owned(self.to_string())
                    }
                }
            }
        }
    });

    quote! { #( #tags )* }
}

struct NamespacesInput {
    namespaces: Vec<NamespaceDef>,
}

struct NamespaceDef {
    vis: Visibility,
    ident: Ident,
    parent: Option<Path>,
    segment: LitStr,
}

impl Parse for NamespacesInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut namespaces = Vec::new();
        while !input.is_empty() {
            let vis: Visibility = input.parse()?;
            let ident: Ident = input.parse()?;

            let parent = if input.peek(syn::token::Paren) {
                let content;
                parenthesized!(content in input);
                content.parse::<kw::parent>()?;
                content.parse::<Token![=]>()?;
                Some(content.parse()?)
            } else {
                None
            };

            input.parse::<Token![=>]>()?;
            let segment: LitStr = input.parse()?;
            input.parse::<Token![;]>()?;

            namespaces.push(NamespaceDef {
                vis,
                ident,
                parent,
                segment,
            });
        }
        Ok(Self { namespaces })
    }
}

fn expand_namespaces(input: NamespacesInput) -> TokenStream2 {
    let namespaces = input.namespaces.into_iter().map(|ns| {
        let vis = ns.vis;
        let ident = ns.ident;
        let segment = ns.segment;

        if let Some(parent) = ns.parent {
            quote! {
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
                #vis struct #ident;

                impl chalk_metrics::__private::MetricNamespace for #ident {
                    fn path() -> &'static [&'static str] {
                        static PATH: ::std::sync::OnceLock<Box<[&'static str]>> =
                            ::std::sync::OnceLock::new();
                        PATH.get_or_init(|| {
                            let mut segments = <#parent as chalk_metrics::__private::MetricNamespace>::path().to_vec();
                            segments.push(#segment);
                            segments.into_boxed_slice()
                        })
                    }
                }
            }
        } else {
            quote! {
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
                #vis struct #ident;

                impl chalk_metrics::__private::MetricNamespace for #ident {
                    #[inline]
                    fn path() -> &'static [&'static str] {
                        &[#segment]
                    }
                }
            }
        }
    });

    quote! { #( #namespaces )* }
}

struct MetricsInput {
    groups: Vec<MetricGroup>,
}

struct MetricGroup {
    namespace: Option<Path>,
    tags: Vec<TagRef>,
    metrics: Vec<MetricDef>,
}

struct MetricDef {
    vis: Visibility,
    kind: MetricKind,
    ident: Ident,
    name: LitStr,
    description: LitStr,
    extra_tags: Vec<TagRef>,
}

#[derive(Clone, Copy)]
enum MetricKind {
    Count,
    Gauge,
    Histogram,
}

#[derive(Clone)]
struct TagRef {
    optional: bool,
    ty: Path,
    alias: Option<Ident>,
}

impl Parse for MetricsInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut groups = Vec::new();
        while !input.is_empty() {
            input.parse::<kw::group>()?;

            let args;
            parenthesized!(args in input);
            let mut namespace = None;
            let mut tags = None;
            while !args.is_empty() {
                if args.peek(kw::namespace) {
                    args.parse::<kw::namespace>()?;
                    args.parse::<Token![=]>()?;
                    namespace = Some(args.parse()?);
                } else if args.peek(kw::tags) {
                    args.parse::<kw::tags>()?;
                    args.parse::<Token![=]>()?;
                    tags = Some(parse_tag_list(&args)?);
                } else {
                    return Err(args.error("expected `namespace = ...` or `tags = [...]`"));
                }

                if args.peek(Token![,]) {
                    args.parse::<Token![,]>()?;
                }
            }

            let content;
            braced!(content in input);
            let mut metrics = Vec::new();
            while !content.is_empty() {
                let vis: Visibility = content.parse()?;
                let kind_ident: Ident = content.parse()?;
                let kind = parse_metric_kind(&kind_ident)?;
                let ident: Ident = content.parse()?;
                content.parse::<Token![=>]>()?;
                let name: LitStr = content.parse()?;
                content.parse::<Token![,]>()?;
                let description: LitStr = content.parse()?;

                let mut extra_tags = Vec::new();
                if content.peek(Token![,]) {
                    content.parse::<Token![,]>()?;
                    if content.peek(kw::tags) {
                        content.parse::<kw::tags>()?;
                        content.parse::<Token![+=]>()?;
                        extra_tags = parse_tag_list(&content)?;
                    }
                }

                content.parse::<Token![;]>()?;

                metrics.push(MetricDef {
                    vis,
                    kind,
                    ident,
                    name,
                    description,
                    extra_tags,
                });
            }

            groups.push(MetricGroup {
                namespace,
                tags: tags.unwrap_or_default(),
                metrics,
            });
        }
        Ok(Self { groups })
    }
}

fn parse_metric_kind(ident: &Ident) -> Result<MetricKind> {
    match ident.to_string().as_str() {
        "count" => Ok(MetricKind::Count),
        "gauge" => Ok(MetricKind::Gauge),
        "histogram" => Ok(MetricKind::Histogram),
        _ => Err(Error::new(
            ident.span(),
            "expected metric type `count`, `gauge`, or `histogram`",
        )),
    }
}

fn parse_tag_list(input: ParseStream<'_>) -> Result<Vec<TagRef>> {
    let content;
    bracketed!(content in input);
    let refs = Punctuated::<TagRef, Token![,]>::parse_terminated(&content)?;
    Ok(refs.into_iter().collect())
}

impl Parse for TagRef {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let optional = if input.peek(kw::optional) {
            input.parse::<kw::optional>()?;
            true
        } else {
            false
        };
        let ty: Path = input.parse()?;
        let alias = if input.peek(Token![as]) {
            input.parse::<Token![as]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self {
            optional,
            ty,
            alias,
        })
    }
}

fn expand_metrics(input: MetricsInput) -> Result<TokenStream2> {
    let mut out = Vec::new();
    for group in input.groups {
        let namespace = group
            .namespace
            .unwrap_or_else(|| syn::parse_quote!(chalk_metrics::__private::NoNamespace));

        for metric in group.metrics {
            let mut tags = group.tags.clone();
            tags.extend(metric.extra_tags.clone());
            out.push(expand_metric(metric, &namespace, &tags)?);
        }
    }
    Ok(quote! { #( #out )* })
}

fn expand_metric(metric: MetricDef, namespace: &Path, tags: &[TagRef]) -> Result<TokenStream2> {
    let vis = metric.vis;
    let ident = metric.ident;
    let name = metric.name;
    let description = metric.description;
    let metric_type = match metric.kind {
        MetricKind::Count => "count",
        MetricKind::Gauge => "gauge",
        MetricKind::Histogram => "histogram",
    };
    let doc = format!("{} ({})", description.value(), metric_type);

    let mut fields = Vec::new();
    let mut export_pairs = Vec::new();
    for tag in tags {
        let ty = &tag.ty;
        let field = tag.alias.clone().unwrap_or_else(|| default_field_ident(ty));

        let export_key = if let Some(alias) = &tag.alias {
            let alias = alias.to_string();
            quote! { #alias }
        } else {
            quote! { <#ty as chalk_metrics::__private::MetricTag>::EXPORT_NAME }
        };

        if tag.optional {
            fields.push(quote! { pub #field: Option<#ty>, });
            export_pairs.push(quote! {
                if let Some(ref value) = self.#field {
                    pairs.push((
                        #export_key,
                        <#ty as chalk_metrics::__private::MetricTag>::export_value(value),
                    ));
                }
            });
        } else {
            fields.push(quote! { pub #field: #ty, });
            export_pairs.push(quote! {
                pairs.push((
                    #export_key,
                    <#ty as chalk_metrics::__private::MetricTag>::export_value(&self.#field),
                ));
            });
        }
    }

    let record_method = match metric.kind {
        MetricKind::Count => quote! {
            /// Record a count increment of 1.
            pub fn record(&self) {
                self.record_value(1);
            }

            /// Record a count delta.
            pub fn record_value(&self, value: i64) {
                let hash = self.tags_hash();
                chalk_metrics::client::record_count(
                    Self::NAME,
                    Self::namespace(),
                    hash,
                    || self.export_pairs(),
                    value,
                );
            }
        },
        MetricKind::Gauge => quote! {
            /// Record a gauge value.
            pub fn record(&self, value: f64) {
                let hash = self.tags_hash();
                chalk_metrics::client::record_gauge(
                    Self::NAME,
                    Self::namespace(),
                    hash,
                    || self.export_pairs(),
                    value,
                );
            }
        },
        MetricKind::Histogram => quote! {
            /// Record a histogram observation.
            pub fn record(&self, value: f64) {
                let hash = self.tags_hash();
                chalk_metrics::client::record_histogram(
                    Self::NAME,
                    Self::namespace(),
                    hash,
                    || self.export_pairs(),
                    value,
                );
            }
        },
    };

    Ok(quote! {
        #[doc = #doc]
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        #vis struct #ident {
            #( #fields )*
        }

        impl #ident {
            /// The metric name without namespace segments.
            pub const NAME: &'static str = #name;

            /// The namespace path. Empty for top-level metrics.
            #[inline]
            pub fn namespace() -> &'static [&'static str] {
                <#namespace as chalk_metrics::__private::MetricNamespace>::path()
            }

            /// Returns `(export_name, value)` pairs for all set tags.
            pub fn export_pairs(&self) -> Vec<(&'static str, ::std::borrow::Cow<'static, str>)> {
                let mut pairs = Vec::new();
                #( #export_pairs )*
                pairs
            }

            #[inline]
            fn tags_hash(&self) -> u64 {
                use ::std::hash::{Hash, Hasher};
                let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
                self.hash(&mut hasher);
                hasher.finish()
            }

            #record_method
        }
    })
}

fn default_field_ident(path: &Path) -> Ident {
    let ident = path
        .segments
        .last()
        .map(|segment| segment.ident.to_string())
        .unwrap_or_else(|| "tag".to_owned());
    format_ident!("{}", to_snake_case(&ident), span = Span::call_site())
}

fn to_snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.extend(ch.to_lowercase());
    }
    result
}

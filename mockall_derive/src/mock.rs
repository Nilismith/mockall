// vim: tw=80
use super::*;
use quote::{ToTokens, quote};
use std::borrow::Borrow;
use syn::{
    braced,
    parse::{Parse, ParseStream},
    spanned::Spanned,
};

pub(crate) struct Mock {
    pub(crate) vis: syn::Visibility,
    pub(crate) name: syn::Ident,
    pub(crate) generics: syn::Generics,
    // The Mock struct's inherent methods.  The blocks will all be empty.
    pub(crate) methods: Vec<syn::ImplItemMethod>,
    pub(crate) traits: Vec<syn::ItemTrait>
}

impl Mock {
    pub(crate) fn gen(&self) -> TokenStream {
        let mut output = TokenStream::new();
        let mut mock_body = TokenStream::new();
        let mut cp_body = TokenStream::new();
        let mut has_new = false;
        let mock_struct_name = gen_mock_ident(&self.name);
        let mock_mod_ident = gen_mod_ident(&self.name, None);
        let subs = self.traits.iter().map(|trait_| {
            (trait_.ident.to_string(), self.generics.clone())
        }).collect::<Vec<_>>();
        // generate the mock structure
        gen_struct(&self.vis, &self.name, &self.generics, &subs, &self.methods)
            .to_tokens(&mut output);
        // generate sub structures
        for trait_ in self.traits.iter() {
            let mut sub_cp_body = TokenStream::new();
            let span = Span::call_site();
            let sub_mock = syn::Ident::new(
                &format!("{}_{}", &self.name, &trait_.ident),
                span);
            let sub_struct = syn::Ident::new(
                &format!("{}_expectations", &trait_.ident),
                span);
            let methods = trait_.items.iter().filter_map(|item| {
                if let syn::TraitItem::Method(m) = item {
                    Some(tim2iim(m))
                } else {
                    None
                }
            }).collect::<Vec<_>>();
            let vis = syn::Visibility::Inherited;
            gen_struct(&vis, &sub_mock, &self.generics, &[], &methods)
                .to_tokens(&mut output);
            let mock_sub_name = gen_mock_ident(&sub_mock);
            for meth in methods {
                let (_, _, cp) = gen_mock_method(Some(&mock_mod_ident),
                                                 &mock_sub_name,
                                                 &meth.borrow().attrs[..],
                                                 &meth.vis, &meth.vis,
                                                 &meth.borrow().sig, None);
                cp.to_tokens(&mut sub_cp_body);
            }
            let (ig, tg, wc) = self.generics.split_for_impl();
            quote!(impl #ig #mock_sub_name #tg #wc {
                fn checkpoint(&mut self) {
                    #sub_cp_body
                }
            }).to_tokens(&mut output);
            quote!(self.#sub_struct.checkpoint();).to_tokens(&mut cp_body);
        }
        // generate methods on the mock structure itself
        for meth in self.methods.iter() {
            has_new |= meth.sig.ident == "new";
            let (mm, em, cp) = gen_mock_method(Some(&mock_mod_ident),
                                               &mock_struct_name,
                                               &meth.borrow().attrs[..],
                                               &meth.vis, &meth.vis,
                                               &meth.sig, None);
            // For inherent methods, use the same visibility for the mock and
            // expectation method as for the original.
            mm.to_tokens(&mut mock_body);
            em.to_tokens(&mut mock_body);
            cp.to_tokens(&mut cp_body);
        }
        // generate the mock struct's inherent methods
        quote!(
            pub fn checkpoint(&mut self) {
                #cp_body
            }
        ).to_tokens(&mut mock_body);
        // Add a "new" method if the struct doesn't already have one.  Add it
        // even if the struct implements a trait that has a new method.  The
        // trait's new method can still be called as `<MockX as TraitY>::new`
        if !has_new {
            quote!(
                pub fn new() -> Self {
                    Self::default()
                }
            ).to_tokens(&mut mock_body);
        }
        // generate methods on traits
        let (ig, tg, wc) = self.generics.split_for_impl();
        quote!(impl #ig #mock_struct_name #tg #wc {#mock_body})
            .to_tokens(&mut output);
        for trait_ in self.traits.iter() {
            mock_trait_methods(&self.name, Some(&self.generics), &trait_)
                .to_tokens(&mut output);
        }
        output
    }
}

impl Parse for Mock {
    fn parse(input: ParseStream) -> syn::parse::Result<Self> {
        let vis: syn::Visibility = input.parse()?;
        let name: syn::Ident = input.parse()?;
        let mut generics: syn::Generics = input.parse()?;
        let wc: Option<syn::WhereClause> = input.parse()?;
        generics.where_clause = wc;

        let impl_content;
        let _brace_token = braced!(impl_content in input);
        let mut methods = Vec::new();
        while !impl_content.is_empty() {
            let method: syn::TraitItem = impl_content.parse()?;
            match &method {
                syn::TraitItem::Method(meth) => {
                    methods.push(tim2iim(meth))
                },
                _ => {
                    return Err(input.error("Unsupported in this context"));
                }
            }
        }

        let mut traits = Vec::new();
        while !input.is_empty() {
            let trait_: syn::ItemTrait = input.parse()?;
            traits.push(trait_);
        }

        Ok(Mock{vis, name, generics, methods, traits})
    }
}

fn format_attrs(attrs: &[syn::Attribute]) -> TokenStream {
    let mut out = TokenStream::new();
    for attr in attrs {
        if attr.path.segments.len() == 1 &&
           attr.path.segments.first().unwrap().value().ident == "doc"
        {
            // Discard doc attributes from the mock object.  They cause a bunch
            // of warnings.
            continue;
        }
        attr.to_tokens(&mut out);
    }
    out
}

/// Generate a mock method and its expectation method
fn gen_mock_method(mod_ident: Option<&syn::Ident>,
                   mock_ident: &syn::Ident,
                   meth_attrs: &[syn::Attribute],
                   meth_vis: &syn::Visibility,
                   expect_vis: &syn::Visibility,
                   sig: &syn::MethodSig,
                   sub: Option<&syn::Ident>)
    -> (TokenStream, TokenStream, TokenStream)
{
    assert!(sig.decl.variadic.is_none(),
        "MockAll does not yet support variadic functions");
    let mut mock_output = TokenStream::new();
    let mut expect_output = TokenStream::new();
    let mut cp_output = TokenStream::new();
    let constness = sig.constness;
    let unsafety = sig.unsafety;
    let asyncness = sig.asyncness;
    let abi = &sig.abi;
    let fn_token = &sig.decl.fn_token;
    let ident = &sig.ident;
    let generics = &sig.decl.generics;
    let (ig, tg, wc) = sig.decl.generics.split_for_impl();
    let call_turbofish = tg.as_turbofish();
    let inputs = demutify(&sig.decl.inputs);
    let output = &sig.decl.output;
    let attrs = format_attrs(meth_attrs);

    // First the mock method
    quote!(#attrs #meth_vis #constness #unsafety #asyncness #abi
           #fn_token #ident #ig (#inputs) #output #wc)
        .to_tokens(&mut mock_output);

    let sub_name = if let Some(s) = sub {
        format!("{}_", s)
    } else {
        "".to_string()
    };
    let meth_types = method_types(sig);
    let expectation = &meth_types.expectation;
    let call = &meth_types.call;
    let mut args = Vec::new();
    let expect_obj_name = if meth_types.is_static {
        let name = syn::Ident::new(
            &format!("{}_{}{}_expectation", mock_ident, sub_name, sig.ident),
            Span::call_site());
        quote!(#name)
    } else if let Some(s) = sub {
        let sub_struct = syn::Ident::new(&format!("{}_expectations", s),
            Span::call_site());
        quote!(self.#sub_struct.#ident)
    } else {
        quote!(self.#ident)
    };
    for p in inputs.iter() {
        match p {
            syn::FnArg::SelfRef(_) | syn::FnArg::SelfValue(_) => {
                // Don't output the "self" argument
            },
            syn::FnArg::Captured(arg) => {
                args.push(arg.pat.clone());
            },
            _ => compile_error(p.span(),
                "Should be unreachable for normal Rust code")
        }
    }

    if meth_types.is_static {
        quote!({
            #expect_obj_name.lock().unwrap().#call#call_turbofish(#(#args),*)
        })
    } else {
        quote!({
            #expect_obj_name.#call#call_turbofish(#(#args),*)
        })
    }.to_tokens(&mut mock_output);

    // Then the expectation method
    let expect_ident = syn::Ident::new(&format!("expect_{}", sig.ident),
                                       sig.ident.span());
    if meth_types.is_static {
        let name = syn::Ident::new(
            &format!("{}_{}{}_expectation", mock_ident, sub_name, sig.ident),
            Span::call_site());
        let mut g = generics.clone();
        let lt = syn::Lifetime::new("'guard", Span::call_site());
        let ltd = syn::LifetimeDef::new(lt);
        g.params.push(syn::GenericParam::Lifetime(ltd.clone()));
        let guard_name = if generics.params.is_empty() {
            syn::Ident::new("ExpectationGuard", Span::call_site())
        } else {
            syn::Ident::new("GenericExpectationGuard", Span::call_site())
        };
        quote!(#attrs #expect_vis fn #expect_ident #g()
               -> #mod_ident::#ident::#guard_name<#ltd>
               #wc
            {
                #mod_ident::#ident::#guard_name::new(#name.lock().unwrap())
            }
        )
    } else {
        quote!(#attrs #expect_vis fn #expect_ident #ig(&mut self)
               -> &mut #mod_ident::#expectation
               #wc
        {
            #expect_obj_name.expect#call_turbofish()
        })
    }.to_tokens(&mut expect_output);

    // Finally this method's contribution to the checkpoint method
    if meth_types.is_static {
        quote!(#attrs { #expect_obj_name.lock().unwrap().checkpoint(); })
    } else {
        quote!(#attrs { #expect_obj_name.checkpoint(); })
    }.to_tokens(&mut cp_output);

    (mock_output, expect_output, cp_output)
}

fn gen_struct<T>(vis: &syn::Visibility,
                 ident: &syn::Ident,
                 generics: &syn::Generics,
                 subs: &[(String, syn::Generics)],
                 methods: &[T]) -> TokenStream
    where T: Borrow<syn::ImplItemMethod>
{
    let mut output = TokenStream::new();
    let mod_ident = gen_mod_ident(&ident, None);
    let ident = gen_mock_ident(&ident);
    let mut body = TokenStream::new();
    let mut mod_body = TokenStream::new();
    let mut statics = TokenStream::new();
    let mut default_body = TokenStream::new();

    // Make Expectation fields for each method
    for (sub, sub_generics) in subs.iter() {
        let spn = Span::call_site();
        let (_, tg, _) = sub_generics.split_for_impl();
        let sub_struct = syn::Ident::new(&format!("{}_expectations", sub), spn);
        let sub_mock = syn::Ident::new(&format!("{}_{}", ident, sub), spn);
        quote!(#sub_struct: #sub_mock #tg,).to_tokens(&mut body);
        quote!(#sub_struct: #sub_mock::default(),)
            .to_tokens(&mut default_body)
    }
    for meth in methods.iter() {
        let attrs = format_attrs(&meth.borrow().attrs);
        let method_ident = &meth.borrow().sig.ident;
        let meth_types = method_types(&meth.borrow().sig);
        let args = &meth.borrow().sig.decl.inputs;
        let expect_obj = &meth_types.expect_obj;
        let expectations = &meth_types.expectations;
        let altargs = &meth_types.altargs;
        let matchexprs = &meth_types.matchexprs;
        let meth_ident = &meth.borrow().sig.ident;
        let return_type = match &meth.borrow().sig.decl.output {
            syn::ReturnType::Type(_, ty) => {
                let mut rt = (**ty).clone();
                deimplify(&mut rt);
                destrify(&mut rt);
                rt
            },
            syn::ReturnType::Default => {
                syn::Type::Tuple(syn::TypeTuple {
                    paren_token: syn::token::Paren::default(),
                    elems: syn::punctuated::Punctuated::new()
                })
            }
        };

        let generics = &meth.borrow().sig.decl.generics;
        let (_, tg, _) = generics.split_for_impl();
        let mut macro_g = TokenStream::new();
        if generics.lt_token.is_some() {
            tg.to_tokens(&mut macro_g);
        } else {
            // expectation! requires the <> even if it's empty
            quote!(<>).to_tokens(&mut macro_g);
        }

        quote!(::mockall::expectation! {
            fn #meth_ident #macro_g (#args) -> #return_type {
                let (#altargs) = (#matchexprs);
            }
        }).to_tokens(&mut mod_body);

        if meth_types.is_static {
            let name = syn::Ident::new(
                &format!("{}_{}_expectation", ident, method_ident),
                Span::call_site());
            quote!(#attrs ::mockall::lazy_static! {
                static ref #name: ::std::sync::Mutex<#mod_ident::#expect_obj> =
                   ::std::sync::Mutex::new(#mod_ident::#expectations::new());
            }).to_tokens(&mut statics);
        } else {
            quote!(#attrs #method_ident: #mod_ident::#expect_obj,)
                .to_tokens(&mut body);
            quote!(#attrs #method_ident: #mod_ident::#expectations::default(),)
                .to_tokens(&mut default_body)
        }
    }

    // Make PhantomData fields, if necessary
    for (count, param) in generics.params.iter().enumerate() {
        let phname = format!("_t{}", count);
        let phident = syn::Ident::new(&phname, Span::call_site());
        match param {
            syn::GenericParam::Lifetime(l) => {
                if !l.bounds.is_empty() {
                    compile_error(l.bounds.span(),
                        "#automock does not yet support lifetime bounds on structs");
                }
                let lifetime = &l.lifetime;
                quote!(#phident: ::std::marker::PhantomData<&#lifetime ()>,)
                    .to_tokens(&mut body);
            },
            syn::GenericParam::Type(tp) => {
                let ty = &tp.ident;
                quote!(#phident: ::std::marker::PhantomData<#ty>,)
                    .to_tokens(&mut body);
            },
            syn::GenericParam::Const(_) => {
                compile_error(param.span(),
                    "#automock does not yet support generic constants");
            }
        }
        quote!(#phident: ::std::marker::PhantomData,)
            .to_tokens(&mut default_body);
    }
    let (ig, tg, wc) = generics.split_for_impl();
    quote!(
        #[allow(non_snake_case)]
        mod #mod_ident {
            #mod_body
        }
        #vis struct #ident #ig #wc {
            #body
        }
    ).to_tokens(&mut output);
    statics.to_tokens(&mut output);
    quote!(impl #ig ::std::default::Default for #ident #tg #wc {
        fn default() -> Self {
            Self {
                #default_body
            }
        }
    }).to_tokens(&mut output);

    output
}

/// Generate mock methods for a Trait
///
/// # Parameters
///
/// * `struct_ident`:       Name of the structure to mock
/// * `struct_generics`:    If provided, use these generic fields for the
///                         Mock struct.  Otherwise, generate the struct's
///                         generics from the Trait
/// * `item`:               The trait whose methods are being mocked
fn mock_trait_methods(struct_ident: &syn::Ident,
                      struct_generics: Option<&syn::Generics>,
                      item: &syn::ItemTrait) -> TokenStream
{
    let mut output = TokenStream::new();
    let mut mock_body = TokenStream::new();
    let mut expect_body = TokenStream::new();
    let mock_ident = gen_mock_ident(&struct_ident);

    let pub_token = syn::token::Pub{span: Span::call_site()};
    let pub_vis = syn::Visibility::Public(syn::VisPublic{pub_token});

    for trait_item in item.items.iter() {
        match trait_item {
            syn::TraitItem::Const(_) => {
                // Nothing to implement
            },
            syn::TraitItem::Method(meth) => {
                let mod_ident = gen_mod_ident(&struct_ident, Some(&item.ident));
                let (mock_meth, expect_meth, _cp) = gen_mock_method(
                    Some(&mod_ident),
                    &mock_ident,
                    &meth.attrs[..],
                    &syn::Visibility::Inherited,
                    &pub_vis,
                    &meth.sig,
                    Some(&item.ident)
                );
                // trait methods must have inherited visibility.  Expectation
                // methods should have public, for lack of any clearer option.
                mock_meth.to_tokens(&mut mock_body);
                expect_meth.to_tokens(&mut expect_body);
            },
            syn::TraitItem::Type(ty) => {
                if !ty.generics.params.is_empty() {
                    compile_error(ty.generics.span(),
                        "Mockall does not yet support generic associated types");
                }
                if ty.default.is_some() {
                    // Trait normally can't get here (unless the
                    // associated_type_defaults feature is enabled), but we can
                    // get here from mock! if invoked like
                    // mock!{
                    //     Foo { }
                    //     trait Bar {
                    //         type A=B;
                    //     }
                    // }
                    ty.to_tokens(&mut mock_body)
                } else {
                    compile_error(ty.span(), "Associated types must be made concrete for mocking.");
                }
            },
            _ => {
                compile_error(trait_item.span(),
                    "This impl item is not yet supported by MockAll");
            }
        }
    }

    // Put all mock methods in one impl block
    item.unsafety.to_tokens(&mut output);
    let ident = &item.ident;
    let (s_ig, s_sg, s_wc) = match struct_generics {
        Some(g) => g.split_for_impl(),
        None => item.generics.split_for_impl()
    };
    let (_t_ig, t_tg, _t_wc) = item.generics.split_for_impl();
    quote!(impl #s_ig #ident #t_tg
           for #mock_ident #s_sg #s_wc {
        #mock_body
    }).to_tokens(&mut output);

    // Put all expect methods in a separate impl block.  This is necessary when
    // mocking a trait impl, where we can't add any new methods
    quote!(impl #s_ig #mock_ident #s_sg #s_wc {
        #expect_body
    }).to_tokens(&mut output);

    output
}

fn tim2iim(m: &syn::TraitItemMethod) -> syn::ImplItemMethod {
    syn::ImplItemMethod{
        attrs: m.attrs.clone(),
        vis: syn::parse2(quote!(pub)).unwrap(),
        defaultness: None,
        sig: m.sig.clone(),
        block: syn::parse2(quote!({})).unwrap(),
    }
}

pub(crate) fn do_mock(input: TokenStream) -> TokenStream {
    let mock: Mock = match syn::parse2(input) {
        Ok(mock) => mock,
        Err(err) => {
            return err.to_compile_error();
        }
    };
    mock.gen()
}

/// Test cases for `mock!{}`.
#[cfg(test)]
mod t {

    use std::str::FromStr;
    use pretty_assertions::assert_eq;
    use super::super::*;

    fn check(desired: &str, code: &str) {
        let ts = proc_macro2::TokenStream::from_str(code).unwrap();
        let output = do_mock(ts).to_string();
        // Let proc_macro2 reformat the whitespace in the expected string
        let expected = proc_macro2::TokenStream::from_str(desired).unwrap()
            .to_string();
        assert_eq!(expected, output);
    }

    #[test]
    fn clone() {
        let desired = r#"
        struct MockA {
            Clone_expectations: MockA_Clone,
        }
        impl ::std::default::Default for MockA {
            fn default() -> Self {
                Self {
                    Clone_expectations: MockA_Clone::default(),
                }
            }
        }
        struct MockA_Clone {
            clone: ::mockall::Expectations<(), MockA> ,
        }
        impl ::std::default::Default for MockA_Clone {
            fn default() -> Self {
                Self {
                    clone: ::mockall::Expectations::default(),
                }
            }
        }
        impl MockA_Clone {
            fn checkpoint(&mut self) {
                { self.clone.checkpoint(); }
            }
        }
        impl MockA {
            pub fn checkpoint(&mut self) {
                self.Clone_expectations.checkpoint();
            }
            pub fn new() -> Self {
                Self::default()
            }
        }
        impl Clone for MockA {
            fn clone(&self) -> Self {
                self.Clone_expectations.clone.call(())
            }
        }
        impl MockA {
            pub fn expect_clone(&mut self)
            -> &mut ::mockall::Expectation<(), MockA>
            {
                self.Clone_expectations.clone.expect()
            }
        }"#;
        let code = r#"
        A {}
        trait Clone {
            fn clone(&self) -> Self;
        }"#;
        check(desired, code);
    }

    /// Mocking a struct with a generic method with mock!{}
    #[test]
    fn generic_method() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation! {
                    fn foo<T>(&self, t:T) -> () {
                        let (p1: &T) = (&t);
                    }
                }
            }
            struct MockFoo {
                foo: __mock_Foo::foo::GenericExpectations,
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self {
                        foo: __mock_Foo::foo::GenericExpectations::default(),
                    }
                }
            }
            impl MockFoo {
                pub fn foo<T: 'static>(&self, t: T) {
                    self.foo.call:: <T>(t)
                }
                pub fn expect_foo<T: 'static>(&mut self)
                    -> &mut __mock_Foo::foo::Expectation<T>
                {
                    self.foo.expect:: <T>()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo {
                fn foo<T: 'static>(&self, t: T);
            }"#;
        check(desired, code);
    }

    #[test]
    fn generic_static_method() {
        let desired = r#"
            struct MockFoo {
            }
            ::mockall::lazy_static!{
                static ref MockFoo_bar_expectation: ::std::sync::Mutex< ::mockall::GenericExpectations >
                = ::std::sync::Mutex::new(::mockall::GenericExpectations::new());
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self { }
                }
            }
            impl MockFoo {
                pub fn bar<T>(x: T) {
                    MockFoo_bar_expectation.lock().unwrap().call:: <(T), ()>((x))
                }
                pub fn expect_bar< 'guard, T, >()
                    -> ::mockall::GenericExpectationGuard< 'guard, (T), ()>
                {
                    ::mockall::GenericExpectationGuard::new(
                        MockFoo_bar_expectation.lock().unwrap()
                    )
                }
                pub fn checkpoint(&mut self) {
                    { MockFoo_bar_expectation.lock().unwrap().checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo {
                fn bar<T>(x: T);
            }
        "#;
        check(desired, code);
    }

    /// Mocking a generic struct that's defined in another crate
    #[test]
    fn generic_struct() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation!{
                    fn foo< >(&self, x: u32) -> i64 {
                        let (p1: &u32) = (&x);
                    }
                }
            }
            struct MockFoo<T: Clone> {
                foo: __mock_Foo::foo::Expectations,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T: Clone> ::std::default::Default for MockFoo<T> {
                fn default() -> Self {
                    Self {
                        foo: __mock_Foo::foo::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            impl<T: Clone> MockFoo<T> {
                pub fn foo(&self, x: u32) -> i64 {
                    self.foo.call(x)
                }
                pub fn expect_foo(&mut self)
                    -> &mut __mock_Foo::foo::Expectation
                {
                    self.foo.expect()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo<T: Clone> {
                fn foo(&self, x: u32) -> i64;
            }
        "#;
        check(desired, code);
    }

    /// Mocking a generic struct that's defined in another crate and has a trait
    /// impl
    #[test]
    fn generic_struct_with_trait() {
        let desired = r#"
            struct MockExternalStruct<T: Copy + 'static> {
                Foo_expectations: MockExternalStruct_Foo<T> ,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T: Copy + 'static>
            ::std::default::Default for MockExternalStruct<T> {
                fn default() -> Self {
                    Self {
                        Foo_expectations: MockExternalStruct_Foo::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            struct MockExternalStruct_Foo<T: Copy + 'static> {
                foo: ::mockall::Expectations<(u32), u32> ,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T: Copy + 'static> ::std::default::Default for MockExternalStruct_Foo<T> {
                fn default() -> Self {
                    Self {
                        foo: ::mockall::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            impl<T: Copy + 'static> MockExternalStruct_Foo<T> {
                fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
            }
            impl<T: Copy + 'static> MockExternalStruct<T> {
                pub fn checkpoint(&mut self) {
                    self.Foo_expectations.checkpoint();
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl<T: Copy + 'static> Foo for MockExternalStruct<T> {
                fn foo(&self, x: u32) -> u32 {
                    self.Foo_expectations.foo.call((x))
                }
            }
            impl<T: Copy + 'static> MockExternalStruct<T> {
                pub fn expect_foo(&mut self)
                    -> &mut ::mockall::Expectation<(u32), u32>
                {
                    self.Foo_expectations.foo.expect()
                }
            }
        "#;
        let code = r#"
            ExternalStruct<T: Copy + 'static> {}
            trait Foo {
                fn foo(&self, x: u32) -> u32;
            }
        "#;
        check(desired, code);
    }

    /// Implement a generic trait on a generic struct with mock!
    #[test]
    fn generic_struct_with_generic_trait() {
        let desired = r#"
            struct MockExternalStruct<T: 'static, Z: 'static> {
                Foo_expectations: MockExternalStruct_Foo<T, Z> ,
                _t0: ::std::marker::PhantomData<T> ,
                _t1: ::std::marker::PhantomData<Z> ,
            }
            impl<T: 'static, Z: 'static>
            ::std::default::Default for MockExternalStruct<T, Z> {
                fn default() -> Self {
                    Self {
                        Foo_expectations: MockExternalStruct_Foo::default(),
                        _t0: ::std::marker::PhantomData,
                        _t1: ::std::marker::PhantomData,
                    }
                }
            }
            struct MockExternalStruct_Foo<T: 'static, Z: 'static> {
                foo: ::mockall::Expectations<(T), T> ,
                _t0: ::std::marker::PhantomData<T> ,
                _t1: ::std::marker::PhantomData<Z> ,
            }
            impl<T: 'static, Z: 'static>
            ::std::default::Default for MockExternalStruct_Foo<T, Z> {
                fn default() -> Self {
                    Self {
                        foo: ::mockall::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                        _t1: ::std::marker::PhantomData,
                    }
                }
            }
            impl<T: 'static, Z: 'static> MockExternalStruct_Foo<T, Z> {
                fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
            }
            impl<T: 'static, Z: 'static> MockExternalStruct<T, Z> {
                pub fn checkpoint(&mut self) {
                    self.Foo_expectations.checkpoint();
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl<T: 'static, Z: 'static> Foo<T> for MockExternalStruct<T, Z> {
                fn foo(&self, x: T) -> T {
                    self.Foo_expectations.foo.call((x))
                }
            }
            impl<T: 'static, Z: 'static> MockExternalStruct<T, Z> {
                pub fn expect_foo(&mut self)
                    -> &mut ::mockall::Expectation<(T), T>
                {
                    self.Foo_expectations.foo.expect()
                }
            }
        "#;
        let code = r#"
            ExternalStruct<T: 'static, Z: 'static> {}
            trait Foo<T: 'static> {
                fn foo(&self, x: T) -> T;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn generic_struct_with_trait_with_associated_types() {
        let desired = r#"
        struct MockFoo<T> {
            Iterator_expectations: MockFoo_Iterator<T> ,
            _t0: ::std::marker::PhantomData<T> ,
        }
        impl<T> ::std::default::Default for MockFoo<T> {
            fn default() -> Self {
                Self {
                    Iterator_expectations: MockFoo_Iterator::default(),
                    _t0: ::std::marker::PhantomData,
                }
            }
        }
        struct MockFoo_Iterator<T> {
            next: ::mockall::Expectations<(), Option<T> > ,
            _t0: ::std::marker::PhantomData<T> ,
        }
        impl<T> ::std::default::Default for MockFoo_Iterator<T> {
            fn default() -> Self {
                Self {
                    next: ::mockall::Expectations::default(),
                    _t0: ::std::marker::PhantomData,
                }
            }
        }
        impl<T> MockFoo_Iterator<T> {
            fn checkpoint(&mut self) {
                {
                    self.next.checkpoint();
                }
            }
        }
        impl<T> MockFoo<T> {
            pub fn checkpoint(&mut self) {
                self.Iterator_expectations.checkpoint();
            }
            pub fn new() -> Self {
                Self::default()
            }
        }
        impl<T> Iterator for MockFoo<T> {
            type Item = T;
            fn next(&mut self) -> Option<T> {
                self.Iterator_expectations.next.call(())
            }
        }
        impl<T> MockFoo<T> {
            pub fn expect_next(&mut self)
                -> &mut ::mockall::Expectation<(), Option<T> >
            {
                self.Iterator_expectations.next.expect()
            }
        }"#;
         let code = r#"
            Foo<T> {}
            trait Iterator {
                type Item=T;

                fn next(&mut self) -> Option<T>;
        }"#;
        check(desired, code);
    }

    #[test]
    fn generic_trait() {
        let desired = r#"
        struct MockExternalStruct<T> {
            GenericTrait_expectations: MockExternalStruct_GenericTrait<T> ,
            _t0: ::std::marker::PhantomData<T> ,
        }
        impl<T> ::std::default::Default for MockExternalStruct<T> {
            fn default() -> Self {
                Self {
                    GenericTrait_expectations: MockExternalStruct_GenericTrait::default(),
                    _t0: ::std::marker::PhantomData,
                }
            }
        }
        struct MockExternalStruct_GenericTrait<T> {
            foo: ::mockall::Expectations<(), ()> ,
            _t0: ::std::marker::PhantomData<T> ,
        }
        impl<T> ::std::default::Default for MockExternalStruct_GenericTrait<T> {
            fn default() -> Self {
                Self {
                    foo: ::mockall::Expectations::default(),
                    _t0: ::std::marker::PhantomData,
                }
            }
        }
        impl<T> MockExternalStruct_GenericTrait<T> {
            fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
        }
        impl<T> MockExternalStruct<T> {
            pub fn checkpoint(&mut self) {
                self.GenericTrait_expectations.checkpoint();
            }
            pub fn new() -> Self {
                Self::default()
            }
        }
        impl<T> GenericTrait<T> for MockExternalStruct<T> {
            fn foo(&self) {
                self.GenericTrait_expectations.foo.call(())
            }
        }
        impl<T> MockExternalStruct<T> {
            pub fn expect_foo(&mut self) -> &mut ::mockall::Expectation<(), ()>
            {
                self.GenericTrait_expectations.foo.expect()
            }
        }"#;
        let code = r#"
        ExternalStruct<T> {}
        trait GenericTrait<T> {
            fn foo(&self);
        }"#;
        check(desired, code);
    }

    /// "impl trait" in method return types should work.
    #[test]
    fn impl_trait() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation! {
                    fn foo< >(&self) -> Box<dyn Debug + Send> {
                        let () = ();
                    }
                }
            }
            struct MockFoo {
                foo: __mock_Foo::foo::Expectations,
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self {
                        foo: __mock_Foo::foo::Expectations::default(),
                    }
                }
            }
            impl MockFoo {
                pub fn foo(&self) -> impl Debug + Send {
                    self.foo.call()
                }
                pub fn expect_foo(&mut self)
                    -> &mut __mock_Foo::foo::Expectation
                {
                    self.foo.expect()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo {
                fn foo(&self) -> impl Debug + Send;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn inherited_trait() {
        trait A {
            fn foo(&self);
        }
        trait B: A {
            fn bar(&self);
        }
        let desired = r#"
        struct MockExternalStruct {
            A_expectations: MockExternalStruct_A,
            B_expectations: MockExternalStruct_B,
        }
        impl ::std::default::Default for MockExternalStruct {
            fn default() -> Self {
                Self {
                    A_expectations: MockExternalStruct_A::default(),
                    B_expectations: MockExternalStruct_B::default(),
                }
            }
        }
        struct MockExternalStruct_A {
            foo: ::mockall::Expectations<(), ()> ,
        }
        impl ::std::default::Default for MockExternalStruct_A {
            fn default() -> Self {
                Self {
                    foo: ::mockall::Expectations::default(),
                }
            }
        }
        impl MockExternalStruct_A {
            fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
        }
        struct MockExternalStruct_B {
            bar: ::mockall::Expectations<(), ()> ,
        }
        impl ::std::default::Default for MockExternalStruct_B {
            fn default() -> Self {
                Self {
                    bar: ::mockall::Expectations::default(),
                }
            }
        }
        impl MockExternalStruct_B {
            fn checkpoint(&mut self) {
                { self.bar.checkpoint(); }
            }
        }
        impl MockExternalStruct {
            pub fn checkpoint(&mut self) {
                self.A_expectations.checkpoint();
                self.B_expectations.checkpoint();
            }
            pub fn new() -> Self {
                Self::default()
            }
        }
        impl A for MockExternalStruct {
            fn foo(&self) {
                self.A_expectations.foo.call(())
            }
        }
        impl MockExternalStruct {
            pub fn expect_foo(&mut self) -> &mut ::mockall::Expectation<(), ()>
            {
                self.A_expectations.foo.expect()
            }
        }
        impl B for MockExternalStruct {
            fn bar(&self) {
                self.B_expectations.bar.call(())
            }
        }
        impl MockExternalStruct {
            pub fn expect_bar(&mut self) -> &mut ::mockall::Expectation<(), ()>
            {
                self.B_expectations.bar.expect()
            }
        }"#;
        let code = r#"
            ExternalStruct {}
            trait A {
                fn foo(&self);
            }
            trait B {
                fn bar(&self);
            }
        "#;
        check(desired, code);
    }

    /// Structs or traits that have a "new" method shouldn't have a "new" method
    /// added to the mock object
    #[test]
    fn new_method() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation!{
                    fn foo< >(&self) -> u32 { let () = (); }
                }
                ::mockall::expectation!{
                    fn new< >(x: u32) -> Self { let (p0: &u32) = (&x); }
                }
            }
            struct MockFoo {
                foo: __mock_Foo::foo::Expectations,
            }
            ::mockall::lazy_static!{
                static ref MockFoo_new_expectation:
                    ::std::sync::Mutex< __mock_Foo::new::Expectations>
                    = ::std::sync::Mutex::new(
                        __mock_Foo::new::Expectations::new()
                    );
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self {
                        foo: __mock_Foo::foo::Expectations::default(),
                    }
                }
            }
            impl MockFoo {
                pub fn foo(&self) -> u32 {
                    self.foo.call()
                }
                pub fn expect_foo(&mut self)
                    -> &mut __mock_Foo::foo::Expectation
                {
                    self.foo.expect()
                }
                pub fn new(x: u32) -> Self {
                    MockFoo_new_expectation.lock().unwrap().call(x)
                }
                pub fn expect_new< 'guard>()
                    -> __mock_Foo::new::ExpectationGuard< 'guard>
                {
                    __mock_Foo::new::ExpectationGuard::new(
                        MockFoo_new_expectation.lock().unwrap()
                    )
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                    { MockFoo_new_expectation.lock().unwrap().checkpoint(); }
                }
            }
        "#;
        let code = r#"
            Foo {
                fn foo(&self) -> u32;
                fn new(x: u32) -> Self;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn reference_arguments() {
        let desired = r#"
        #[allow(non_snake_case)]
        mod __mock_Foo {
            ::mockall::expectation! {
                fn foo< >(&self, x: &u32) -> () {
                    let (p1: &u32) = (x);
                }
            }
            ::mockall::expectation! {
                fn bar< >(&self, y: & 'static u32) -> () {
                    let (p1: & 'static u32) = (y);
                }
            }
        }
        struct MockFoo {
            foo: __mock_Foo::foo::Expectations,
            bar: __mock_Foo::bar::Expectations,
        }
        impl::std::default::Default for MockFoo {
            fn default() -> Self {
                Self {
                    foo: __mock_Foo::foo::Expectations::default(),
                    bar: __mock_Foo::bar::Expectations::default(),
                }
            }
        }
        impl MockFoo {
            pub fn foo(&self, x: &u32) {
                self.foo.call(x)
            }
            pub fn expect_foo(&mut self) -> &mut __mock_Foo::foo::Expectation
            {
                self.foo.expect()
            }
            pub fn bar(&self, y: & 'static u32) {
                self.bar.call(y)
            }
            pub fn expect_bar(&mut self) -> &mut __mock_Foo::bar::Expectation
            {
                self.bar.expect()
            }
            pub fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
                { self.bar.checkpoint(); }
            }
            pub fn new() -> Self {
                Self::default()
            }
        }"#;

        let code = r#"
            Foo {
                fn foo(&self, x: &u32);
                fn bar(&self, y: &'static u32);
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn reference_return() {
        let desired = r#"
        #[allow(non_snake_case)]
        mod __mock_Foo {
            ::mockall::expectation! {
                fn foo< >(&self) -> &u32 {
                    let () =();
                }
            }
        }
        struct MockFoo {
            foo: __mock_Foo::foo::Expectations ,
        }
        impl ::std::default::Default for MockFoo {
            fn default() -> Self {
                Self {
                    foo: __mock_Foo::foo::Expectations::default(),
                }
            }
        }
        impl MockFoo {
            pub fn foo(&self) -> &u32 {
                self.foo.call()
            }
            pub fn expect_foo(&mut self)
                -> &mut __mock_Foo::foo::Expectation
            {
                self.foo.expect()
            }
            pub fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
            pub fn new() -> Self {
                Self::default()
            }
        }"#;

        let code = r#"
            Foo {
                fn foo(&self) -> &u32;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn reference_return_from_trait() {
        let desired = r#"
        #[allow(non_snake_case)]
        mod __mock_X {}
        struct MockX {
            Foo_expectations: MockX_Foo ,
        }
        impl ::std::default::Default for MockX {
            fn default() -> Self {
                Self {
                    Foo_expectations: MockX_Foo::default(),
                }
            }
        }
        #[allow(non_snake_case)]
        mod __mock_X_Foo {
            ::mockall::expectation!{
                fn foo< >(&self) -> &u32 {
                    let () = ();
                }
            }
        }
        struct MockX_Foo {
            foo: __mock_X_Foo::foo::Expectations,
        }
        impl ::std::default::Default for MockX_Foo {
            fn default() -> Self {
                Self {
                    foo: __mock_X_Foo::foo::Expectations::default(),
                }
            }
        }
        impl MockX_Foo {
            fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
        }
        impl MockX {
            pub fn checkpoint(&mut self) {
                self.Foo_expectations.checkpoint();
            }
            pub fn new() -> Self {
                Self::default()
            }
        }
        impl Foo for MockX {
            fn foo(&self) -> &u32 {
                self.Foo_expectations.foo.call()
            }
        }
        impl MockX {
            pub fn expect_foo(&mut self)
                -> &mut __mock_X_Foo::foo::Expectation
            {
                self.Foo_expectations.foo.expect()
            }
        }"#;

        let code = r#"
            X {}
            trait Foo {
                fn foo(&self) -> &u32;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn ref_mut_return() {
        let desired = r#"
        #[allow(non_snake_case)]
        mod __mock_Foo {
            ::mockall::expectation!{
                fn foo< >(&mut self) -> &mut u32 {
                    let () = ();
                }
            }
        }
        struct MockFoo {
            foo: __mock_Foo::foo::Expectations,
        }
        impl ::std::default::Default for MockFoo {
            fn default() -> Self {
                Self {
                    foo: __mock_Foo::foo::Expectations::default(),
                }
            }
        }
        impl MockFoo {
            pub fn foo(&mut self) -> &mut u32 {
                self.foo.call_mut()
            }
            pub fn expect_foo(&mut self)
                -> &mut __mock_Foo::foo::Expectation
            {
                self.foo.expect()
            }
            pub fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
            pub fn new() -> Self {
                Self::default()
            }
        }"#;

        let code = r#"
            Foo {
                fn foo(&mut self) -> &mut u32;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn ref_str_return() {
        let desired = r#"
        #[allow(non_snake_case)]
        mod __mock_Foo {
            ::mockall::expectation!{
                fn foo< >(&self) -> & ::std::string::String {
                    let () = ();
                }
            }
        }
        struct MockFoo {
            foo: __mock_Foo::foo::Expectations,
        }
        impl ::std::default::Default for MockFoo {
            fn default() -> Self {
                Self {
                    foo: __mock_Foo::foo::Expectations::default(),
                }
            }
        }
        impl MockFoo {
            pub fn foo(&self) -> &str {
                self.foo.call()
            }
            pub fn expect_foo(&mut self)
                -> &mut __mock_Foo::foo::Expectation
            {
                self.foo.expect()
            }
            pub fn checkpoint(&mut self) {
                { self.foo.checkpoint(); }
            }
            pub fn new() -> Self {
                Self::default()
            }
        }"#;

        let code = r#"
            Foo {
                fn foo(&self) -> &str;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn static_method() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation!{
                    fn bar< >(x: u32) -> u64 {
                        let (p0: &u32) = (&x);
                    }
                }
            }
            struct MockFoo {}
            ::mockall::lazy_static!{
                static ref MockFoo_bar_expectation:
                    ::std::sync::Mutex< __mock_Foo::bar::Expectations >
                = ::std::sync::Mutex::new(__mock_Foo::bar::Expectations::new());
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self { }
                }
            }
            impl MockFoo {
                pub fn bar(x: u32) -> u64 {
                    MockFoo_bar_expectation.lock().unwrap().call(x)
                }
                pub fn expect_bar< 'guard>()
                    -> __mock_Foo::bar::ExpectationGuard< 'guard>
                {
                    __mock_Foo::bar::ExpectationGuard::new(
                        MockFoo_bar_expectation.lock().unwrap()
                    )
                }
                pub fn checkpoint(&mut self) {
                    { MockFoo_bar_expectation.lock().unwrap().checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo {
                fn bar(x: u32) -> u64;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn static_trait_method() {
        let desired = r#"
            struct MockFoo {
                Bar_expectations: MockFoo_Bar,
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self {
                        Bar_expectations: MockFoo_Bar::default(),
                    }
                }
            }
            struct MockFoo_Bar {}
            ::mockall::lazy_static!{
                static ref MockFoo_Bar_baz_expectation: ::std::sync::Mutex< ::mockall::Expectations<(u32), u64> >
                = ::std::sync::Mutex::new(::mockall::Expectations::new());
            }
            impl ::std::default::Default for MockFoo_Bar {
                fn default() -> Self {
                    Self {}
                }
            }
            impl MockFoo_Bar {
                fn checkpoint(&mut self) {
                    { MockFoo_Bar_baz_expectation.lock().unwrap().checkpoint(); }
                }
            }
            impl MockFoo {
                pub fn checkpoint(&mut self) {
                    self.Bar_expectations.checkpoint();
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl Bar for MockFoo {
                fn baz(x: u32) -> u64 {
                    MockFoo_Bar_baz_expectation.lock().unwrap().call((x))
                }
            }
            impl MockFoo {
                pub fn expect_baz< 'guard>()
                    -> ::mockall::ExpectationGuard< 'guard, (u32), u64>
                {
                    ::mockall::ExpectationGuard::new(
                        MockFoo_Bar_baz_expectation.lock().unwrap()
                    )
                }
            }
        "#;
        let code = r#"
            Foo {}
            trait Bar {
                fn baz(x: u32) -> u64;
            }
        "#;
        check(desired, code);
    }

    /// Mocking a struct that's defined in another crate with mock!
    #[test]
    fn struct_() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Foo {
                ::mockall::expectation!{
                    fn foo< >(&self, x: u32) -> i64 { let (p1: &u32) = (&x);}
                }
                ::mockall::expectation!{
                    fn bar< >(&self, y: u64) -> i32 { let (p1: &u64) = (&y);}
                }
            }
            struct MockFoo {
                foo: __mock_Foo::foo::Expectations,
                bar: __mock_Foo::bar::Expectations,
            }
            impl ::std::default::Default for MockFoo {
                fn default() -> Self {
                    Self {
                        foo: __mock_Foo::foo::Expectations::default(),
                        bar: __mock_Foo::bar::Expectations::default(),
                    }
                }
            }
            impl MockFoo {
                pub fn foo(&self, x: u32) -> i64 {
                    self.foo.call(x)
                }
                pub fn expect_foo(&mut self)
                    -> &mut __mock_Foo::foo::Expectation
                {
                    self.foo.expect()
                }
                pub fn bar(&self, y: u64) -> i32 {
                    self.bar.call(y)
                }
                pub fn expect_bar(&mut self)
                    -> &mut __mock_Foo::bar::Expectation
                {
                    self.bar.expect()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                    { self.bar.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo {
                fn foo(&self, x: u32) -> i64;
                fn bar(&self, y: u64) -> i32;
            }
        "#;
        check(desired, code);
    }

    /// Mocking a struct that's defined in another crate, and has a trait
    /// implementation
    #[test]
    fn struct_with_trait() {
        let desired = r#"
            #[allow(non_snake_case)]
            mod __mock_Bar {}
            struct MockBar {
                Foo_expectations: MockBar_Foo,
            }
            impl ::std::default::Default for MockBar {
                fn default() -> Self {
                    Self {
                        Foo_expectations: MockBar_Foo::default(),
                    }
                }
            }
            #[allow(non_snake_case)]
            mod __mock_Bar_Foo {
                ::mockall::expectation! {
                    fn foo< >(&self, x: u32) -> i64 {
                        let (p1: &u32) = (&x);
                    }
                }
            }
            struct MockBar_Foo {
                foo: __mock_Bar_Foo::foo::Expectations,
            }
            impl ::std::default::Default for MockBar_Foo {
                fn default() -> Self {
                    Self {
                        foo: __mock_Bar_Foo::foo::Expectations::default(),
                    }
                }
            }
            impl MockBar_Foo  {
                fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
            }
            impl MockBar {
                pub fn checkpoint(&mut self) {
                    self.Foo_expectations.checkpoint();
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl Foo for MockBar {
                fn foo(&self, x: u32) -> i64 {
                    self.Foo_expectations.foo.call(x)
                }
            }
            impl MockBar {
                pub fn expect_foo(&mut self)
                    -> &mut __mock_Bar_Foo::foo::Expectation
                {
                    self.Foo_expectations.foo.expect()
                }
            }
        "#;
        let code = r#"
            Bar {}
            trait Foo {
                fn foo(&self, x: u32) -> i64;
            }
        "#;
        check(desired, code);
    }

    /// Mocking a struct that's defined in another crate, and has a trait
    /// implementation that includes an associated type
    #[test]
    fn struct_with_trait_with_associated_types() {
        let desired = r#"
            struct MockMyIter {
                Iterator_expectations: MockMyIter_Iterator,
            }
            impl ::std::default::Default for MockMyIter {
                fn default() -> Self {
                    Self {
                        Iterator_expectations: MockMyIter_Iterator::default(),
                    }
                }
            }
            struct MockMyIter_Iterator {
                next: ::mockall::Expectations<(), Option<u32> > ,
            }
            impl ::std::default::Default for MockMyIter_Iterator {
                fn default() -> Self {
                    Self {
                        next: ::mockall::Expectations::default(),
                    }
                }
            }
            impl MockMyIter_Iterator {
                fn checkpoint(&mut self) {
                    { self.next.checkpoint(); }
                }
            }
            impl MockMyIter {
                pub fn checkpoint(&mut self) {
                    self.Iterator_expectations.checkpoint();
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl Iterator for MockMyIter {
                type Item=u32;
                fn next(&mut self) -> Option<u32> {
                    self.Iterator_expectations.next.call(())
                }
            }
            impl MockMyIter {
                pub fn expect_next(&mut self)
                    -> &mut ::mockall::Expectation<(), Option<u32> >
                {
                    self.Iterator_expectations.next.expect()
                }
            }
        "#;
        let code = r#"
            MyIter {}
            trait Iterator {
                type Item=u32;

                fn next(&mut self) -> Option<u32>;
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn where_clause_on_struct() {
        let desired = r#"
            struct MockFoo<T>
                where T: Clone
            {
                foo: ::mockall::Expectations<(), ()> ,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T> ::std::default::Default for MockFoo<T>
                where T: Clone
            {
                fn default() -> Self {
                    Self {
                        foo: ::mockall::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            impl<T> MockFoo<T>
                where T: Clone
            {
                pub fn foo(&self) {
                    self.foo.call(())
                }
                pub fn expect_foo(&mut self)
                    -> &mut ::mockall::Expectation<(), ()>
                {
                    self.foo.expect()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            Foo<T>
                where T: Clone
            {
                fn foo(&self);
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn where_clause_on_struct_with_trait() {
        let desired = r#"
            struct MockFoo<T>
                where T: Clone
            {
                Bar_expectations: MockFoo_Bar<T> ,
                foo: ::mockall::Expectations<(), ()> ,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T> ::std::default::Default for MockFoo<T>
                where T: Clone
            {
                fn default() -> Self {
                    Self {
                        Bar_expectations: MockFoo_Bar::default(),
                        foo: ::mockall::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            struct MockFoo_Bar<T> where T: Clone {
                bar: ::mockall::Expectations<(), ()> ,
                _t0: ::std::marker::PhantomData<T> ,
            }
            impl<T> ::std::default::Default for MockFoo_Bar<T> where T: Clone {
                fn default() -> Self {
                    Self {
                        bar: ::mockall::Expectations::default(),
                        _t0: ::std::marker::PhantomData,
                    }
                }
            }
            impl<T> MockFoo_Bar<T> where T: Clone {
                fn checkpoint(&mut self) {
                    { self.bar.checkpoint(); }
                }
            }
            impl<T> MockFoo<T>
                where T: Clone
            {
                pub fn foo(&self) {
                    self.foo.call(())
                }
                pub fn expect_foo(&mut self)
                    -> &mut ::mockall::Expectation<(), ()>
                {
                    self.foo.expect()
                }
                pub fn checkpoint(&mut self) {
                    self.Bar_expectations.checkpoint();
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
            impl<T> Bar for MockFoo<T> where T: Clone {
                fn bar (&self) {
                    self.Bar_expectations.bar.call(())
                }
            }
            impl<T> MockFoo<T> where T: Clone {
                pub fn expect_bar(&mut self)
                    -> &mut::mockall::Expectation<(), ()>
                {
                    self. Bar_expectations.bar.expect()
                }
            }"#;
        let code = r#"
            Foo<T>
                where T: Clone
            {
                fn foo(&self);
            }
            trait Bar {
                fn bar(&self);
            }
        "#;
        check(desired, code);
    }

    #[test]
    fn where_clause_on_method() {
        let desired = r#"
            struct MockSomeStruct {
                foo: ::mockall::GenericExpectations,
            }
            impl ::std::default::Default for MockSomeStruct {
                fn default() -> Self {
                    Self {
                        foo: ::mockall::GenericExpectations::default(),
                    }
                }
            }
            impl MockSomeStruct {
                pub fn foo<T>(&self, t: T)
                    where T: 'static
                {
                    self.foo.call:: <(T), ()>((t))
                }
                pub fn expect_foo<T>(&mut self)
                    -> &mut ::mockall::Expectation<(T), ()>
                    where T: 'static
                {
                    self.foo.expect:: <(T), ()>()
                }
                pub fn checkpoint(&mut self) {
                    { self.foo.checkpoint(); }
                }
                pub fn new() -> Self {
                    Self::default()
                }
            }
        "#;
        let code = r#"
            SomeStruct {
                fn foo<T>(&self, t: T)
                    where T: 'static;
            }"#;
        check(desired, code);
    }

    #[test]
    fn where_clause_on_static_method() {
        let desired = r#"
        struct MockFoo<T: Clone> {
            _t0: ::std::marker::PhantomData<T> ,
        }
        ::mockall::lazy_static!{
            static ref MockFoo_new_expectation:
                ::std::sync::Mutex< ::mockall::GenericExpectations>
                = ::std::sync::Mutex::new(::mockall::GenericExpectations::new());
        }
        impl<T: Clone> ::std::default::Default for MockFoo<T> {
            fn default() -> Self {
                Self {
                    _t0: ::std::marker::PhantomData,
                }
            }
        }
        impl<T: Clone> MockFoo<T> {
            pub fn new<T2>(t: T2) -> MockFoo<T2> where T2: Clone {
                MockFoo_new_expectation.lock().unwrap()
                .call:: <(T2), MockFoo<T2> >((t))
            }
            pub fn expect_new< 'guard, T2, >()
                -> ::mockall::GenericExpectationGuard< 'guard, (T2), MockFoo<T2> >
                where T2: Clone
            {
                ::mockall::GenericExpectationGuard::new(
                    MockFoo_new_expectation.lock().unwrap()
                )
            }
            pub fn checkpoint(&mut self) {
                { MockFoo_new_expectation.lock().unwrap().checkpoint(); }
            }
        }"#;
        let code = r#"
        Foo<T: Clone> {
            fn new<T2>(t: T2) -> MockFoo<T2> where T2: Clone;
        }"#;
        check(desired, code);
    }
}

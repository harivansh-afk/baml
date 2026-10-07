//! Test-only observation at generated entry points. Parse Rust syntax rather
//! than patching a particular spelling or whitespace in an emitted signature.

pub(super) fn source(source: &str) -> String {
    let mut file = syn::parse_file(source).expect("generated Rust parses");
    let mut resumes = 0;
    for item in &mut file.items {
        match item {
            syn::Item::Impl(implementation)
                if implementation.trait_.as_ref().is_some_and(|(_, path, _)| {
                    path.segments.last().unwrap().ident == "CompiledFrame"
                }) =>
            {
                let syn::Type::Path(ty) = implementation.self_ty.as_ref() else {
                    panic!("compiled frame is a named type")
                };
                let object = object_id(&ty.path.segments.last().unwrap().ident, "Frame");
                let method = implementation
                    .items
                    .iter_mut()
                    .find_map(|item| match item {
                        syn::ImplItem::Fn(method) if method.sig.ident == "resume" => Some(method),
                        _ => None,
                    })
                    .expect("compiled frame has a resume method");
                method
                    .block
                    .stmts
                    .insert(0, syn::parse_quote! { record_entry(#object, false); });
                resumes += 1;
            }
            syn::Item::Fn(function) if function.sig.ident.to_string().starts_with("direct_") => {
                let object = object_id(&function.sig.ident, "direct_");
                function
                    .block
                    .stmts
                    .insert(0, syn::parse_quote! { record_entry(#object, true); });
            }
            _ => {}
        }
    }
    assert!(resumes > 0, "instrumentation found no compiled frames");
    let counters = syn::parse2::<syn::File>(quote::quote! {
        pub static EXECUTIONS: std::sync::Mutex<std::collections::BTreeSet<(usize, bool)>> =
            std::sync::Mutex::new(std::collections::BTreeSet::new());
        pub static DIRECT_ENTRIES: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        fn record_entry(object: usize, direct: bool) {
            EXECUTIONS.lock().unwrap().insert((object, direct));
            if direct {
                DIRECT_ENTRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    file.items.extend(counters.items);
    prettyplease::unparse(&file)
}

fn object_id(ident: &syn::Ident, prefix: &str) -> usize {
    ident
        .to_string()
        .strip_prefix(prefix)
        .unwrap()
        .parse()
        .expect("generated object ID")
}

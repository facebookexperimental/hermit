/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keep the pidfd spelling adapter complete when Reverie's Guest gains methods.

use std::collections::BTreeSet;

use quote::ToTokens;
use syn::ImplItem;
use syn::Item;
use syn::ItemImpl;
use syn::ItemTrait;
use syn::TraitItem;
use syn::Type;

// The initialized Reverie gitlink is the source input, as for the loader
// fixtures in first_instruction_cpuid. The repository pin check binds it to
// Cargo's Reverie revision. Embedding both files keeps the pinned-root test
// independent of runtime source paths, Cargo, and Git.
const GUEST_SOURCE: &str = include_str!("../../../reverie/reverie/src/guest.rs");
const ADAPTER_SOURCE: &str = include_str!("../../../detcore/src/pidfd_self.rs");

fn guest_trait() -> ItemTrait {
    let file = syn::parse_file(GUEST_SOURCE).expect("parse the pinned Guest source");
    let mut guests = file.items.into_iter().filter_map(|item| match item {
        Item::Trait(item) if item.ident == "Guest" => Some(item),
        _ => None,
    });
    let guest = guests.next().expect("the pinned source defines Guest");
    assert!(guests.next().is_none(), "ambiguous Guest trait source");
    guest
}

fn physical_spelling_impl() -> ItemImpl {
    let file = syn::parse_file(ADAPTER_SOURCE).expect("parse PhysicalSpelling source");
    let mut adapters = file.items.into_iter().filter_map(|item| {
        let Item::Impl(item) = item else {
            return None;
        };
        let guest = item.trait_.as_ref().is_some_and(|(_, path, _)| {
            path.segments
                .last()
                .is_some_and(|part| part.ident == "Guest")
        });
        let adapter = matches!(item.self_ty.as_ref(), Type::Path(path)
            if path.path.segments.last().is_some_and(|part| part.ident == "PhysicalSpelling"));
        (guest && adapter).then_some(item)
    });
    let adapter = adapters.next().expect("PhysicalSpelling implements Guest");
    assert!(adapters.next().is_none(), "ambiguous PhysicalSpelling impl");
    adapter
}

fn forwarding_difference(
    guest: &ItemTrait,
    adapter: &ItemImpl,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut required = BTreeSet::new();
    for item in &guest.items {
        match item {
            TraitItem::Fn(method) => {
                assert!(required.insert(method.sig.ident.to_string()));
                if method.sig.ident == "into_guest" {
                    // This default must wrap the adapter itself. Forwarding it
                    // to inner would discard pidfd respelling. This is the only
                    // default deliberately inherited; every other default is
                    // included in the required set, including future ones.
                    let body = method.default.as_ref().expect("into_guest has a default");
                    assert_eq!(
                        body.to_token_stream().to_string(),
                        quote::quote!({ IntoGuest::new(self) }).to_string(),
                        "reconsider the into_guest exception when its default changes"
                    );
                }
            }
            TraitItem::Const(_) | TraitItem::Type(_) => {}
            _ => panic!("opaque Guest trait item cannot be audited for forwarding"),
        }
    }
    assert!(
        required.remove("into_guest"),
        "the default exception must exist"
    );

    let mut implemented = BTreeSet::new();
    for item in &adapter.items {
        match item {
            ImplItem::Fn(method) => {
                assert!(implemented.insert(method.sig.ident.to_string()));
            }
            ImplItem::Const(_) | ImplItem::Type(_) => {}
            _ => panic!("opaque PhysicalSpelling item cannot be audited for forwarding"),
        }
    }
    (
        required.difference(&implemented).cloned().collect(),
        implemented.difference(&required).cloned().collect(),
    )
}

#[test]
fn physical_spelling_covers_the_pinned_guest_methods() {
    let guest = guest_trait();
    let adapter = physical_spelling_impl();
    let (missing, unexpected) = forwarding_difference(&guest, &adapter);
    assert!(
        missing.is_empty() && unexpected.is_empty(),
        "PhysicalSpelling forwarding differs from pinned Guest: missing {missing:?}, unexpected {unexpected:?}"
    );

    // A removed defaulted method still compiles, but silently loses the
    // backend's typed refusal. Remove the real method from the parsed impl,
    // rather than changing an expected-name list or constructing a toy impl.
    let omitted = "take_unsupported_refusal";
    assert!(guest.items.iter().any(|item| matches!(item,
        TraitItem::Fn(method) if method.sig.ident == omitted && method.default.is_some()
    )));
    let mut incomplete = adapter.clone();
    incomplete
        .items
        .retain(|item| !matches!(item, ImplItem::Fn(method) if method.sig.ident == omitted));
    assert_eq!(
        forwarding_difference(&guest, &incomplete),
        (BTreeSet::from([omitted.to_owned()]), BTreeSet::new()),
        "a real omitted defaulted forwarder must be diagnosed by name"
    );

    // Trait growth must change the required set even though adding a default
    // needs no corresponding impl change to compile.
    let mut grown = guest.clone();
    grown.items.push(syn::parse_quote! {
        fn future_guest_default(&self) -> bool { false }
    });
    assert_eq!(
        forwarding_difference(&grown, &adapter),
        (
            BTreeSet::from(["future_guest_default".to_owned()]),
            BTreeSet::new()
        ),
        "a new defaulted Guest method must require an explicit forwarder"
    );
}

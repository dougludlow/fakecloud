use fakecloud_conformance_checksum as checksum;
use proc_macro::TokenStream;
use quote::quote;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::{parse_macro_input, ItemFn, LitStr, Token};

// Macro argument parsing

struct TestActionArgs {
    service: String,
    action: String,
    checksum: String,
}

impl Parse for TestActionArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let service: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;
        let action: LitStr = input.parse()?;
        input.parse::<Token![,]>()?;

        // Parse `checksum = "..."`
        let ident: syn::Ident = input.parse()?;
        if ident != "checksum" {
            return Err(syn::Error::new(ident.span(), "expected `checksum`"));
        }
        input.parse::<Token![=]>()?;
        let checksum: LitStr = input.parse()?;

        Ok(TestActionArgs {
            service: service.value(),
            action: action.value(),
            checksum: checksum.value(),
        })
    }
}

// Cached model store (thread-local to avoid re-parsing per invocation)

thread_local! {
    #[allow(clippy::missing_const_for_thread_local)]
    static MODEL_CACHE: RefCell<HashMap<String, serde_json::Value>> = RefCell::new(HashMap::new());
    static SERVICE_MAP_CACHE: RefCell<Option<serde_json::Value>> = const { RefCell::new(None) };
}

fn aws_models_dir() -> PathBuf {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    PathBuf::from(manifest_dir)
        .join("..")
        .join("..")
        .join("aws-models")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", path.display(), e));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {}", path.display(), e))
}

/// Resolve a service argument (model key like `"cloudwatch"` or service name
/// like `"monitoring"`) to its `aws-models/<key>.json` model key.
fn resolve_model_key(service: &str) -> Option<String> {
    SERVICE_MAP_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        let map =
            cache.get_or_insert_with(|| read_json(&aws_models_dir().join("service-map.json")));
        checksum::resolve_model_key(map, service)
    })
}

fn load_model(model_key: &str) -> serde_json::Value {
    MODEL_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(model_key.to_string())
            .or_insert_with(|| read_json(&aws_models_dir().join(format!("{}.json", model_key))))
            .clone()
    })
}

// The proc macro itself

/// Attribute macro for Level 2 conformance tests.
///
/// Usage:
///
/// ```text
/// #[test_action("sqs", "CreateQueue", checksum = "a3f8b2c1")]
/// fn test_create_queue() { ... }
/// ```
///
/// At compile time this validates:
/// 1. The action exists in the Smithy model for the given service.
/// 2. The checksum matches the current model's operation signature.
///
/// The function is passed through unchanged.
#[proc_macro_attribute]
pub fn test_action(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as TestActionArgs);
    let input_fn = parse_macro_input!(item as ItemFn);

    // Resolve the model key
    let model_key = match resolve_model_key(&args.service) {
        Some(k) => k,
        None => {
            let msg = format!(
                "Unknown service '{}': not found in aws-models/service-map.json",
                args.service
            );
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };

    // Load the model
    let root = load_model(&model_key);

    // Find the operation
    let op = match checksum::find_operation(&root, &args.action) {
        Some(o) => o,
        None => {
            let msg = format!(
                "Action '{}' not found in Smithy model for service '{}'",
                args.action, args.service
            );
            return syn::Error::new(proc_macro2::Span::call_site(), msg)
                .to_compile_error()
                .into();
        }
    };

    // Compute and validate checksum
    let actual_checksum = checksum::compute_checksum(&root, &op);
    if actual_checksum != args.checksum {
        let msg = format!(
            "Checksum mismatch for {}.{}: expected '{}', got '{}'. \
             The Smithy model has changed — update the checksum or the conformance test.",
            args.service, args.action, args.checksum, actual_checksum
        );
        return syn::Error::new(proc_macro2::Span::call_site(), msg)
            .to_compile_error()
            .into();
    }

    // Pass through the original function unchanged
    let output = quote! {
        #input_fn
    };
    output.into()
}

use proc_macro::TokenStream;
use quote::quote;
use syn::{LitStr, parse_macro_input};

#[proc_macro]
pub fn upon(tokens: TokenStream) -> TokenStream {
    // Parse the input as a string literal
    let template_lit = parse_macro_input!(tokens as LitStr);
    let template_source = template_lit.value();

    // Validate the template at compile time using the upon crate
    let mut engine = upon::Engine::new();
    if let Err(err) = engine.add_template("macro_template", &template_source) {
        // Emit a compile error with the upon error message
        let err_msg = err.to_string().replace('"', "\\\"");
        return TokenStream::from(quote! {
            compile_error!(#err_msg);
        });
    }

    // Generate code that creates the engine with the template at runtime
    let template_source_str = template_source;
    let expanded = quote! {{
        let mut engine = upon::Engine::new();
        // SAFETY: the template was validated at compile time
        unsafe {
            engine.add_template("macro_template", #template_source_str).unwrap_unchecked();
        }
        engine
    }};

    TokenStream::from(expanded)
}

/// A proc macro for creating compile-time validated [`NameTemplate`]s.
///
/// This macro:
/// 1. Validates the template syntax at compile time using the upon crate
/// 2. Ensures the template references `{{ name }}` (required for name templates)
/// 3. Returns a `NameTemplate` by parsing at runtime (safe due to compile-time validation)
///
/// # Example
///
/// ```ignoreignore
/// use webshooter_macroswebshooter_macros::name_template;
/// use webshooter::config::NameTemplate;
///
/// // Valid name template - must contain {{ name }}
/// let template: NameTemplate = name_template!("{{ name }}-webshooter");
///
/// // Also valid - can optionally use {{ client_id }}
/// let template: NameTemplate = name_template!("{{ name }}-{{ client_id }}-display");
///
/// // Compile error: missing {{ name }}
/// // let template = name_template!("webshooter");
/// ```
#[proc_macro]
pub fn name_template(tokens: TokenStream) -> TokenStream {
    // Parse the input as a string literal
    let template_lit = parse_macro_input!(tokens as LitStr);
    let template_source = template_lit.value();

    // Validate the template syntax at compile time using the upon crate
    let mut engine = upon::Engine::new();
    if let Err(err) = engine.add_template("macro_template", &template_source) {
        let err_msg = err.to_string().replace('"', "\\\"");
        return TokenStream::from(quote! {
            compile_error!(#err_msg);
        });
    }

    // Check that the template contains {{ name }} (with optional whitespace)
    // We check several common spacing variations
    let has_name = template_source.contains("{{ name }}")
        || template_source.contains("{{name}}")
        || template_source.contains("{{  name }}")
        || template_source.contains("{{ name  }}")
        || template_source.contains("{{  name  }}")
        || template_source.contains("{{\tname}}")
        || template_source.contains("{{ name\t}}")
        || template_source.contains("{{\tname\t}}");

    if !has_name {
        return TokenStream::from(quote! {
            compile_error!("name template must reference {{ name }}");
        });
    }

    // Generate code that creates the NameTemplate at runtime
    // SAFETY: The template was validated by upon! and contains {{ name }}
    let template_source_str = template_source;
    let expanded = quote! {{
        // SAFETY: The template was validated at compile time
        crate::config::NameTemplate::parse(#template_source_str).unwrap()
    }};

    TokenStream::from(expanded)
}

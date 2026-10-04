//! Compile the Slint UI ahead of time (docs/ARCHITECTURE_PLAN.md §9:
//! "Slint UI compiled ahead of time" for sub-second cold start).
//!
//! All resources — IBM Plex fonts and translation catalogs — are embedded
//! into the binary, so the app renders identically on every OS with zero
//! system-font dependency.

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent-dark".into())
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles)
        .with_bundled_translations("translations")
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app-window.slint", config).expect("Slint compile failed");
    println!("cargo:rerun-if-changed=ui/");
    println!("cargo:rerun-if-changed=translations/");
    println!("cargo:rerun-if-changed=assets/fonts/");
}

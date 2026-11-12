// SPDX-License-Identifier: GPL-3.0-or-later
fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target == "android" || target == "ios" {
        return;
    }
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_bundled_translations("translations")
            .with_default_translation_context(slint_build::DefaultTranslationContext::None)
            .with_debug_info(std::env::var("PROFILE").is_ok_and(|p| p == "debug")),
    )
    .expect("Slint interface must compile");
}

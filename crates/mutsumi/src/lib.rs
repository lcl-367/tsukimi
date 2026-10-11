pub mod control;
pub mod video;

pub use control::*;
pub use danmakw::*;
pub use video::*;

/// Enable GTK's negotiated Wayland color management without forcing HDR output.
///
/// # Safety
/// Call before initializing GTK or starting threads, since this changes the
/// process environment. Explicit `GDK_DEBUG=all:color-mgmt` exclusions and
/// `GDK_WAYLAND_DISABLE` settings are left intact.
pub unsafe fn enable_color_management() {
    let mut flags = std::env::var_os("GDK_DEBUG").unwrap_or_default();
    if should_enable_color_management(&flags.to_string_lossy()) {
        if !flags.is_empty() {
            flags.push(":");
        }
        flags.push("color-mgmt");
        unsafe { std::env::set_var("GDK_DEBUG", flags) };
    }
}

fn should_enable_color_management(flags: &str) -> bool {
    // With `all`, GLib treats the other tokens as exclusions, not additions.
    !flags.split([':', ';', ',', ' ', '\t', '\n']).any(|flag| {
        ["all", "help", "color-mgmt"]
            .iter()
            .any(|name| flag.eq_ignore_ascii_case(name))
    })
}

pub fn force_gl_renderer() {
    unsafe {
        std::env::set_var("GSK_RENDERER", "gl");
    }
}

pub fn init() {
    video::init();
    control::init();
    danmakw::init();
}

#[cfg(test)]
mod tests {
    use super::should_enable_color_management;

    #[test]
    fn enabling_color_management_preserves_debug_flag_semantics() {
        assert!(should_enable_color_management(""));
        assert!(should_enable_color_management("misc:events"));
        assert!(!should_enable_color_management("color-mgmt"));
        assert!(!should_enable_color_management("all"));
        assert!(!should_enable_color_management("all:color-mgmt"));
        assert!(!should_enable_color_management("help"));
    }
}

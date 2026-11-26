use std::path::Path;

use objc2_core_foundation::{CFBundle, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL};
use objc2_core_graphics::{
    kCGNullWindowID, kCGWindowAlpha, kCGWindowLayer, kCGWindowNumber, kCGWindowOwnerPID, CGWindowListCopyWindowInfo,
    CGWindowListOption,
};

/// The owner pid and number of the window in front. Neither needs Screen Recording permission (only window names do,
/// and they are never asked for).
pub fn front_window() -> Option<(u32, u64)> {
    let options = CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements;
    let list = CGWindowListCopyWindowInfo(options, kCGNullWindowID)?;
    // SAFETY: CGWindowListCopyWindowInfo returns an array of window dictionaries keyed by CFString.
    let windows = unsafe { list.cast_unchecked::<CFDictionary<CFString, CFType>>() };
    // SAFETY: immutable CFString constants exported by CoreGraphics.
    let (layer, alpha, owner, id) = unsafe { (kCGWindowLayer, kCGWindowAlpha, kCGWindowOwnerPID, kCGWindowNumber) };
    windows.iter().find_map(|w| {
        let number = |key: &CFString| w.get(key).and_then(|v| v.downcast::<CFNumber>().ok());
        let visible = number(alpha).and_then(|a| a.as_f64()).is_none_or(|a| a > 0.0);
        if number(layer)?.as_i64()? != 0 || !visible {
            return None;
        }
        let pid = number(owner)?.as_i64().and_then(|pid| u32::try_from(pid).ok())?;
        let window = number(id).and_then(|n| n.as_i64()).and_then(|n| u64::try_from(n).ok()).unwrap_or(0);
        Some((pid, window))
    })
}

pub fn bundle_id(exe: &Path) -> Option<String> {
    let app = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let url = CFURL::from_directory_path(app)?;
    let bundle: CFRetained<CFBundle> = CFBundle::new(None, Some(&url))?;
    bundle.identifier().map(|id| id.to_string())
}

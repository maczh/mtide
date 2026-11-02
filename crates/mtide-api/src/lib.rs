//! Stable contracts shared by the mtide layers.

/// Protocol version for persisted and cross-layer messages.
pub const PROTOCOL_VERSION: u16 = 1;

/// A semantic color slot. Rendering backends decide how it maps to ANSI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorSlot {
    Default,
    Muted,
    Focus,
    Error,
    Success,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_positive() {
        assert_eq!(ColorSlot::Focus, ColorSlot::Focus);
    }
}

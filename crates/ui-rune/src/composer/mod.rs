//! Composer state. Filled in by the composer module port.

#[derive(Debug, Default)]
pub struct ComposerState {
    /// The composer's text draft for the selected chat (or the new-chat canvas).
    pub text: String,
}

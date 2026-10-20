pub mod adapter;
pub mod redact;
pub mod source;

pub use adapter::{Adapter, SourceFile};

/// One built-in runtime's adapter, by id — the embedded shipped descriptor.
/// There is no hand-written adapter per runtime: the descriptor is the only
/// implementation, so there is no second copy for a field to be missing from.
///
/// # Panics
/// On an id that is not a built-in runtime.
pub fn builtin(id: &str) -> adapter::decl::DeclAdapter {
    let cfg = match id {
        "claude_code" => adapter::decl::DeclConfig::claude_code(),
        "pi" => adapter::decl::DeclConfig::pi(),
        "codex" => adapter::decl::DeclConfig::codex(),
        "dsh" => adapter::decl::DeclConfig::dsh(),
        other => panic!("no built-in runtime {other:?}"),
    };
    adapter::decl::DeclAdapter::new(cfg)
}

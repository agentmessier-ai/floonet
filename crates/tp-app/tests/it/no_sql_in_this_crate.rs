//! `tp-app` holds no SQL, and this is what refuses rather than asks.
//!
//! The chokepoint means something only if the layer above storage cannot reach
//! around it. "tp-db owns the SQL" was written in a `Cargo.toml` comment and in
//! ARCHITECTURE, which is advice; advice does not survive a hurried patch.
//!
//! Scoped to production code rather than whole files, and the reason is a
//! difference between crates rather than leniency. `tp-db`'s own grep guard
//! says test fixtures may write whatever they like because its tests live under
//! `tests/`. This crate's fixtures live in `#[cfg(test)]` modules inside `src/`,
//! where a fixture legitimately inserts rows to build a case. Those modules are
//! last in every file here, so scanning stops at the first one.
//!
//! A grep rather than a type: the type that would express this is
//! `rusqlite::Connection`, and hiding it is the very thing being enforced.

/// Statement keywords, each with its trailing space so that prose like
/// "select the newest" and identifiers like `selected` do not match.
const NEEDLES: [&str; 6] = [
    "SELECT ", "INSERT ", "UPDATE ", "DELETE ", "CREATE ", "PRAGMA ",
];

#[test]
fn no_sql_literal_lives_in_this_crates_production_code() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|x| x != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (i, line) in text.lines().enumerate() {
                let code = line.trim_start();
                // Everything from here down is fixtures, which may speak SQL.
                if code.starts_with("#[cfg(test)]") {
                    break;
                }
                // A comment may quote an idiom; only code is hunted.
                if code.starts_with("//") || code.starts_with("*") {
                    continue;
                }
                if NEEDLES.iter().any(|n| line.contains(n)) {
                    offenders.push(format!("{}:{}  {}", path.display(), i + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "tp-app must not contain SQL — every query belongs in `tp_db::query` or \
         `tp_db::reach`, so that changing a column name or the storage engine \
         stops at the storage crate:\n{}",
        offenders.join("\n")
    );
}

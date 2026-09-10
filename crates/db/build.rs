// The migration set is compiled into this crate rather than read at runtime,
// so the migrations directory is one of its build inputs. Cargo does not infer
// that on its own: it tracks source files, and a newly added .sql file is not
// one, so an otherwise unchanged crate is served from cache and the new
// migration never reaches the binary.
//
// The directory entry catches additions and removals; the per-file entries
// catch edits to a migration that is already present, and do not depend on the
// directory's own timestamp surviving however the build context was assembled.
fn main() {
    let migrations = concat!(env!("CARGO_MANIFEST_DIR"), "/../../migrations");
    println!("cargo:rerun-if-changed={migrations}");

    let Ok(entries) = std::fs::read_dir(migrations) else {
        return;
    };
    for entry in entries.flatten() {
        println!("cargo:rerun-if-changed={}", entry.path().display());
    }
}

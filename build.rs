//! Build script: emits build and git metadata (via vergen) for the user agent.

use vergen_gitcl::{Build, Emitter, Gitcl};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build = Build::all_build();
    let gitcl = Gitcl::all_git();
    Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&gitcl)?
        .emit()?;
    Ok(())
}

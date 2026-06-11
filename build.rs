//! Build script: emits build and git metadata (via vergen) for the user agent.

use vergen_gitcl::{BuildBuilder, Emitter, GitclBuilder};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build = BuildBuilder::all_build()?;
    let gitcl = GitclBuilder::all_git()?;
    Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&gitcl)?
        .emit()?;
    Ok(())
}

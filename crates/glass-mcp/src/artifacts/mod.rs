mod fs;

#[cfg(test)]
mod tests;

#[cfg_attr(
    not(test),
    expect(
        unused_imports,
        reason = "Artifact APIs are consumed by response externalization."
    )
)]
pub(crate) use fs::*;

//! One sweep: read herdr, plan, then carry the plan out.

use crate::herdr::{Error, Herdr, MetaOp};
use crate::providers::SessionNames;
use crate::reconcile::{self, Rename};
use crate::state::StateStore;
use crate::warn;

pub struct Options<'a> {
    /// The home directory, for `~`-shortening `$dir`.
    pub home: &'a str,
    /// The workspace the reset action was invoked from.
    pub reset: Option<&'a str>,
    /// Report planned renames instead of making any change.
    pub dry: bool,
}

/// Runs a sweep. Any failure that stops it is reported as one stderr line
/// and nothing more: doing nothing is always a safe outcome.
pub fn run(herdr: &dyn Herdr, store: &StateStore, names: &mut dyn SessionNames, opts: &Options) {
    if let Err(e) = sweep(herdr, store, names, opts) {
        warn(e);
    }
}

fn sweep(
    herdr: &dyn Herdr,
    store: &StateStore,
    names: &mut dyn SessionNames,
    opts: &Options,
) -> Result<(), Error> {
    let snapshot = herdr.snapshot()?;
    let plan = reconcile::plan(&snapshot, store.read(), names, opts.home, opts.reset);

    if opts.dry {
        for r in plan.renames.iter().filter(|r| r.to != r.from) {
            warn(format!(
                "[dry-run] would rename {} \"{}\" -> \"{}\"",
                r.workspace, r.from, r.to
            ));
        }
        return Ok(());
    }

    for op in &plan.metadata {
        report(herdr, op);
    }
    if plan.persist
        && let Err(e) = store.write(&plan.state)
    {
        // No rename without recorded ownership.
        warn(format!("state write failed: {e}"));
        return Ok(());
    }
    for r in &plan.renames {
        if let Err(e) = rename(herdr, r) {
            warn(format!("rename {} failed: {e}", r.workspace));
        }
    }
    Ok(())
}

fn rename(herdr: &dyn Herdr, r: &Rename) -> Result<(), Error> {
    // Re-read the label at the last moment: the snapshot is stale by now,
    // and a manual rename landing mid-sweep must win.
    if herdr.label(&r.workspace)?.as_deref() != Some(r.from.as_str()) {
        return Ok(());
    }
    // Our own rename fires workspace.renamed; the sweep it triggers finds
    // the label in sync (or default with no session) and does nothing.
    if r.to != r.from {
        herdr.rename(&r.workspace, &r.to)?;
        warn(format!(
            "renamed {} \"{}\" -> \"{}\"",
            r.workspace, r.from, r.to
        ));
    }
    report(herdr, &r.dir_op());
    Ok(())
}

/// Metadata is display-only, so a failed report is logged and skipped.
fn report(herdr: &dyn Herdr, op: &MetaOp) {
    if let Err(e) = herdr.report_metadata(op) {
        let (verb, workspace, token) = match op {
            MetaOp::Set {
                workspace, token, ..
            } => ("report", workspace, token),
            MetaOp::Clear { workspace, token } => ("clear", workspace, token),
        };
        warn(format!(
            "{verb} {token} metadata for {workspace} failed: {e}"
        ));
    }
}

#[cfg(test)]
mod tests;

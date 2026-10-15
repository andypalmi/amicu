//! `amicu skill export` - render the canonical agent skills to a target layout.
//!
//! Runs without a project: it only reads the embedded manifest and writes files.
//! Idempotent - re-running overwrites the generated files.
//!
//! Without `--target`, every target is exported, each into its own default
//! directory. Those directories are distinct, so the exports never overlap.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::SkillTarget;
use crate::error::CliError;
use crate::skills::SKILLS;

pub(crate) fn export(
    target: Option<SkillTarget>,
    path: Option<&Path>,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let plan = match target {
        Some(target) => vec![(target, resolve_base(target, path)?)],
        None => all_targets_plan(path)?,
    };
    export_plan(&plan, out)?;
    Ok(0)
}

/// The export plan for every target, each at its default directory.
fn all_targets_plan(path: Option<&Path>) -> Result<Vec<(SkillTarget, PathBuf)>, CliError> {
    // Every target shares one `<skill>/SKILL.md` layout, so a single
    // `--path` would make them overwrite each other.
    if path.is_some() {
        return Err(CliError::Message(
            "--path needs --target when exporting all targets: every target would write the same <skill>/SKILL.md files into it".to_string(),
        ));
    }
    let plan = SkillTarget::ALL
        .iter()
        .map(|&target| Ok((target, resolve_base(target, None)?)))
        .collect::<Result<Vec<_>, CliError>>()?;
    check_disjoint(&plan)?;
    Ok(plan)
}

/// Write each target's skills into its base directory, reporting what was written.
pub(crate) fn export_plan(
    plan: &[(SkillTarget, PathBuf)],
    out: &mut dyn Write,
) -> Result<(), CliError> {
    for (target, base) in plan {
        writeln!(
            out,
            "Exporting {} skills for {} to {}",
            SKILLS.len(),
            target.label(),
            base.display()
        )
        .map_err(|e| CliError::io("write failed", e))?;
        write_skills(base, out)?;
    }
    Ok(())
}

/// Refuse a plan in which two targets would write into the same or nested
/// directories, rather than silently overwriting files.
pub(crate) fn check_disjoint(plan: &[(SkillTarget, PathBuf)]) -> Result<(), CliError> {
    for (i, (a, base_a)) in plan.iter().enumerate() {
        for (b, base_b) in &plan[i + 1..] {
            if base_a.starts_with(base_b) || base_b.starts_with(base_a) {
                return Err(CliError::Message(format!(
                    "skill export: {} and {} would write to overlapping directories ({} and {})",
                    a.label(),
                    b.label(),
                    base_a.display(),
                    base_b.display()
                )));
            }
        }
    }
    Ok(())
}

/// Resolve the output root: an explicit `--path`, else the target default,
/// made absolute against the current directory.
fn resolve_base(target: SkillTarget, path: Option<&Path>) -> Result<PathBuf, CliError> {
    let raw = path.map_or_else(|| PathBuf::from(target.default_root()), Path::to_path_buf);
    if raw.is_absolute() {
        return Ok(raw);
    }
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::io("cannot determine current directory", e))?;
    Ok(cwd.join(raw))
}

/// Write every skill as `<base>/<name>/SKILL.md` plus its `references/*.md`.
pub(crate) fn write_skills(base: &Path, out: &mut dyn Write) -> Result<(), CliError> {
    for skill in SKILLS {
        let dir = base.join(skill.name);
        fs::create_dir_all(&dir)
            .map_err(|e| CliError::io(format!("cannot create {}", dir.display()), e))?;
        let skill_md = dir.join("SKILL.md");
        fs::write(&skill_md, skill.md)
            .map_err(|e| CliError::io(format!("cannot write {}", skill_md.display()), e))?;

        if !skill.references.is_empty() {
            let refs = dir.join("references");
            fs::create_dir_all(&refs)
                .map_err(|e| CliError::io(format!("cannot create {}", refs.display()), e))?;
            for file in skill.references {
                let path = refs.join(file.name);
                fs::write(&path, file.contents)
                    .map_err(|e| CliError::io(format!("cannot write {}", path.display()), e))?;
            }
        }
        writeln!(out, "  export {}", skill_md.display())
            .map_err(|e| CliError::io("write failed", e))?;
    }
    Ok(())
}

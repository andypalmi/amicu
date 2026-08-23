//! `amicu skill export` - render the canonical agent skills to a target layout.
//!
//! Runs without a project: it only reads the embedded manifest and writes files.
//! Idempotent - re-running overwrites the generated files.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::SkillTarget;
use crate::error::CliError;
use crate::skills::SKILLS;

pub(crate) fn export(
    target: SkillTarget,
    path: Option<&Path>,
    out: &mut dyn Write,
) -> Result<u8, CliError> {
    let base = resolve_base(target, path)?;
    writeln!(
        out,
        "Exporting {} skills for {} to {}",
        SKILLS.len(),
        target.label(),
        base.display()
    )
    .map_err(|e| CliError::io("write failed", e))?;
    write_skills(&base, out)?;
    Ok(0)
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

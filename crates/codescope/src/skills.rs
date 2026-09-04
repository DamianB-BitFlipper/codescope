//! Distribution commands for Codescope's bundled agent skill.

use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

const SKILL_MD: &str = include_str!("../skills/codescope/SKILL.md");
const OPENAI_YAML: &str = include_str!("../skills/codescope/agents/openai.yaml");

/// Arguments for `codescope skills`.
#[derive(Args, Debug)]
pub struct SkillsArgs {
    /// Skill operation to perform.
    #[command(subcommand)]
    command: SkillsCommand,
}

#[derive(Subcommand, Debug)]
enum SkillsCommand {
    /// Print the bundled Codescope skill.
    Show,
    /// Install the skill, failing if it is already installed.
    Install(WriteArgs),
    /// Replace the installed skill files with this Codescope version.
    Update(WriteArgs),
}

#[derive(Args, Debug)]
struct WriteArgs {
    /// Skip the confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,
    /// Install for the current user instead of the current project.
    #[arg(short = 'g', long)]
    global: bool,
    /// Use Claude's `.claude/skills` directory instead of `.agents/skills`.
    #[arg(long)]
    claude: bool,
}

/// Run one bundled-skill command.
pub fn run(args: &SkillsArgs) -> Result<()> {
    match &args.command {
        SkillsCommand::Show => {
            print!("{SKILL_MD}");
            std::io::stdout().flush().context("flush skill output")
        }
        SkillsCommand::Install(args) => install_or_update(args, false),
        SkillsCommand::Update(args) => install_or_update(args, true),
    }
}

fn install_or_update(args: &WriteArgs, update: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("resolve the current directory")?;
    let home = args.global.then(user_home).transpose()?.unwrap_or_default();
    let target = skill_path(&cwd, &home, args.global, args.claude);
    if update {
        anyhow::ensure!(
            target.is_dir(),
            "Codescope skill is not installed at {}; run `codescope skills install{}` first",
            target.display(),
            install_flags(args)
        );
    } else {
        anyhow::ensure!(
            !target.exists(),
            "Codescope skill already exists at {}; run `codescope skills update{}` instead",
            target.display(),
            install_flags(args)
        );
    }
    let verb = if update { "Update" } else { "Install" };
    if !args.yes && !confirm(verb, &target)? {
        println!("Cancelled.");
        return Ok(());
    }
    write_skill(&target)?;
    println!(
        "{} Codescope skill at {}",
        if update { "Updated" } else { "Installed" },
        target.display()
    );
    Ok(())
}

fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot resolve the user home directory for --global")
}

fn skill_path(cwd: &Path, home: &Path, global: bool, claude: bool) -> PathBuf {
    let base = if global { home } else { cwd };
    let agent_dir = if claude { ".claude" } else { ".agents" };
    base.join(agent_dir).join("skills").join("codescope")
}

fn install_flags(args: &WriteArgs) -> String {
    let mut flags = String::new();
    if args.global {
        flags.push_str(" --global");
    }
    if args.claude {
        flags.push_str(" --claude");
    }
    flags
}

fn confirm(verb: &str, target: &Path) -> Result<bool> {
    anyhow::ensure!(
        std::io::stdin().is_terminal(),
        "confirmation requires a terminal; pass --yes for non-interactive use"
    );
    print!("{verb} the Codescope skill at {}? [y/N] ", target.display());
    std::io::stdout().flush().context("flush confirmation")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("read confirmation")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn write_skill(target: &Path) -> Result<()> {
    let agents = target.join("agents");
    std::fs::create_dir_all(&agents)
        .with_context(|| format!("create skill directory {}", agents.display()))?;
    std::fs::write(target.join("SKILL.md"), SKILL_MD)
        .with_context(|| format!("write {}", target.join("SKILL.md").display()))?;
    std::fs::write(agents.join("openai.yaml"), OPENAI_YAML)
        .with_context(|| format!("write {}", agents.join("openai.yaml").display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_matches_agents_and_claude_conventions() {
        let cwd = Path::new("/repo");
        let home = Path::new("/home/user");
        assert_eq!(
            skill_path(cwd, home, false, false),
            PathBuf::from("/repo/.agents/skills/codescope")
        );
        assert_eq!(
            skill_path(cwd, home, true, false),
            PathBuf::from("/home/user/.agents/skills/codescope")
        );
        assert_eq!(
            skill_path(cwd, home, false, true),
            PathBuf::from("/repo/.claude/skills/codescope")
        );
    }

    #[test]
    fn writes_the_complete_bundled_skill() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("codescope");
        write_skill(&target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("SKILL.md")).unwrap(),
            SKILL_MD
        );
        assert_eq!(
            std::fs::read_to_string(target.join("agents/openai.yaml")).unwrap(),
            OPENAI_YAML
        );
    }
}

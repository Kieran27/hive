//! Building the shell command lines hive runs: login-shell wrapping, the nvm
//! prelude (ported from portal-wt `runner.ts`), setup scripts and run procs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hive_core::config::{NodeCfg, ProjectConfig};
use hive_core::template::{render, shell_quote, Vars};

/// `source nvm.sh; nvm use (.nvmrc | default)`, quiet on success. A pinned
/// version (step/run `node = "…"`) skips .nvmrc.
pub fn nvm_prelude(node: &NodeCfg, pin: Option<&str>) -> String {
    if !node.nvm && pin.is_none() {
        return String::new();
    }
    let use_cmd = match pin {
        Some(v) => format!("nvm use {v} >/dev/null 2>&1 || echo \"hive: nvm use {v} failed\" >&2", v = shell_quote(v)),
        None => format!(
            "nvm use >/dev/null 2>&1 || nvm use {v} >/dev/null 2>&1 || echo \"hive: nvm use {v} failed\" >&2",
            v = shell_quote(&node.default_version)
        ),
    };
    format!(
        "export NVM_DIR=\"${{NVM_DIR:-$HOME/.nvm}}\"; \
         if [ -s \"$NVM_DIR/nvm.sh\" ]; then . \"$NVM_DIR/nvm.sh\"; {use_cmd}; \
         else echo \"hive: nvm not found at $NVM_DIR\" >&2; fi; "
    )
}

/// `$SHELL -l -c <script>`.
pub fn login_shell(shell: &str, script: String) -> (String, Vec<String>) {
    (shell.to_string(), vec!["-l".into(), "-c".into(), script])
}

/// Agents run through an interactive login shell so PATH/rc setup applies.
pub fn interactive_login_shell(shell: &str, script: String) -> (String, Vec<String>) {
    (
        shell.to_string(),
        vec!["-l".into(), "-i".into(), "-c".into(), script],
    )
}

pub struct SetupStepPlan {
    pub label: String,
    pub cwd: PathBuf,
    pub cmd: String,
    pub node: Option<String>,
}

pub fn plan_setup(
    cfg: &ProjectConfig,
    worktree: &Path,
    step_ids: &[String],
    vars: &Vars,
) -> Result<Vec<SetupStepPlan>> {
    let mut out = Vec::new();
    for id in step_ids {
        let step = cfg
            .step(id)
            .with_context(|| format!("unknown setup step {id:?}"))?;
        let cmd = render(&step.cmd, vars).with_context(|| format!("setup step {id:?}"))?;
        out.push(SetupStepPlan {
            label: step
                .label
                .as_deref()
                .map(|l| render(l, vars).unwrap_or_else(|_| l.to_string()))
                .unwrap_or_else(|| cmd.clone()),
            cwd: cfg.resolve_dir(worktree, step.cwd.as_deref()),
            cmd,
            node: step.node.clone(),
        });
    }
    Ok(out)
}

/// One script running every step in order, stopping at the first failure.
pub fn setup_script(node: &NodeCfg, notes: &[String], steps: &[SetupStepPlan]) -> String {
    let mut s = String::from("hive_fail() { printf '\\n\\033[31m✗ %s failed (exit %s)\\033[0m\\n' \"$1\" \"$2\"; exit \"$2\"; }; ");
    for n in notes {
        s.push_str(&format!("printf '%s\\n' {}; ", shell_quote(n)));
    }
    for (i, st) in steps.iter().enumerate() {
        let header = format!(
            "▶ [{}/{}] {}  ({})",
            i + 1,
            steps.len(),
            st.label,
            st.cwd.display()
        );
        s.push_str(&format!(
            "printf '\\n\\033[1;36m%s\\033[0m\\n' {h}; (cd {cwd} && {nvm}{cmd}) || hive_fail {l} $?; ",
            h = shell_quote(&header),
            cwd = shell_quote(&st.cwd.display().to_string()),
            nvm = nvm_prelude(node, st.node.as_deref()),
            cmd = st.cmd,
            l = shell_quote(&st.label),
        ));
    }
    s.push_str("printf '\\n\\033[32m✓ setup complete\\033[0m\\n'");
    s
}

/// Script for one run proc: exports, optional wait for a sibling's port,
/// nvm, then the command.
pub fn run_proc_script(
    node: &NodeCfg,
    pin: Option<&str>,
    env: &BTreeMap<String, String>,
    wait_port: Option<(String, u16)>,
    cmd: &str,
) -> String {
    let mut s = String::new();
    for (k, v) in env {
        s.push_str(&format!("export {k}={}; ", shell_quote(v)));
    }
    if let Some((name, port)) = wait_port {
        s.push_str(&format!(
            "printf 'waiting for {name} on :{port}…\\n'; while ! nc -z localhost {port} >/dev/null 2>&1; do sleep 0.5; done; "
        ));
    }
    s.push_str(&nvm_prelude(node, pin));
    s.push_str(&format!(
        "printf '\\033[2m$ %s\\033[0m\\n' {}; ",
        shell_quote(cmd)
    ));
    s.push_str(cmd);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use hive_core::config::ProjectFile;

    #[test]
    fn setup_script_runs_and_stops_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let file: ProjectFile = toml::from_str(
            r#"
[[setup_step]]
id = "a"
cmd = "echo env={env} > out.txt"
[[setup_step]]
id = "b"
cmd = "exit 7"
[[setup_step]]
id = "c"
cmd = "echo never > never.txt"
"#,
        )
        .unwrap();
        let cfg = ProjectConfig::resolve(tmp.path(), file);
        let vars: Vars = [("env".to_string(), "staging".to_string())].into();
        let plan = plan_setup(
            &cfg,
            tmp.path(),
            &["a".into(), "b".into(), "c".into()],
            &vars,
        )
        .unwrap();
        let script = setup_script(&NodeCfg::default(), &["copied .env.local".into()], &plan);
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out.txt"))
                .unwrap()
                .trim(),
            "env=staging"
        );
        assert!(!tmp.path().join("never.txt").exists());
        assert!(String::from_utf8_lossy(&out.stdout).contains("copied .env.local"));
    }

    #[test]
    fn run_script_exports() {
        let env: BTreeMap<_, _> = [("A".to_string(), "x y".to_string())].into();
        let s = run_proc_script(&NodeCfg::default(), None, &env, None, "echo \"$A\"");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&s)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).ends_with("x y\n"));
    }
}

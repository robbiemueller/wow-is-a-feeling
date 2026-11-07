//! The release workflow only runs when a tag is pushed, and a mistake in it can reach every
//! player, so these tests read the real workflow files. The ones that run its shell steps use
//! the text of each step as written, the real xtask binaries and a throwaway key, with
//! stand-ins for `gh`, `curl`, `cargo`, `rustup` and `sudo`.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard, PoisonError};

use feeling_core::selfupdate::verified_manifest;
use sha2::{Digest, Sha256};

const REPOSITORY: &str = "owner/repo";

/// What each build job uploads.
const BUILDS: [(&str, &[&str]); 3] = [
    ("windows", &["wow-is-a-feeling-windows-x64.exe"]),
    (
        "linux",
        &[
            "wow-is-a-feeling-linux-x64.gz",
            "wow-is-a-feeling-linux-x64.tar.gz",
            "wow-is-a-feeling-linux-x64.AppImage",
        ],
    ),
    (
        "macos",
        &[
            "wow-is-a-feeling-macos-universal.zip",
            "wow-is-a-feeling-macos-universal.dmg",
        ],
    ),
];

/// The files a checkout has to bring for the steps run here.
const CHECKOUT: [&str; 5] = [
    "CHANGELOG.md",
    "README.md",
    "LICENSE",
    "packaging/linux/wow-is-a-feeling.desktop",
    "assets/icon/png/icon-256.png",
];

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
}

fn release_tag() -> String {
    format!("v{}", env!("CARGO_PKG_VERSION"))
}

fn workflow(name: &str) -> String {
    fs::read_to_string(repo_root().join(".github/workflows").join(name)).unwrap()
}

/// Every workflow file, by name.
fn all_workflows() -> Vec<(String, String)> {
    let dir = repo_root().join(".github/workflows");
    let mut workflows: Vec<(String, String)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        })
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            (name, fs::read_to_string(path).unwrap())
        })
        .collect();
    workflows.sort();
    assert!(workflows.len() >= 4, "{workflows:?}");
    workflows
}

/// What a line says after `uses: `, with or without the `- ` that starts a step.
fn uses_value(line: &str) -> Option<&str> {
    line.trim_start()
        .trim_start_matches("- ")
        .strip_prefix("uses: ")
}

/// One job's lines. The workflows keep to one layout, so indentation is enough to find things
/// and these tests don't need a YAML parser in the dependency tree.
struct Job<'a> {
    name: &'a str,
    lines: Vec<&'a str>,
}

impl<'a> Job<'a> {
    fn needs(&self) -> Vec<&'a str> {
        self.lines
            .iter()
            .find_map(|line| line.strip_prefix("    needs: "))
            .map(|value| value.trim_matches(['[', ']']).split(", ").collect())
            .unwrap_or_default()
    }

    fn steps(&self) -> Vec<Step<'a>> {
        let mut steps: Vec<Step<'a>> = Vec::new();
        let mut in_steps = false;
        for &line in &self.lines {
            if line == "    steps:" {
                in_steps = true;
            } else if in_steps && line.starts_with("      - ") {
                steps.push(Step { lines: vec![line] });
            } else if in_steps
                && (line.starts_with("       ") || line.is_empty())
                && let Some(step) = steps.last_mut()
            {
                step.lines.push(line);
            }
        }
        steps
    }
}

fn jobs(workflow: &str) -> Vec<Job<'_>> {
    let mut jobs: Vec<Job> = Vec::new();
    let (_, after) = workflow.split_once("\njobs:\n").unwrap();
    for line in after.lines() {
        let is_job = line.starts_with("  ")
            && !line.starts_with("   ")
            && !line.trim_start().starts_with('#');
        if is_job {
            jobs.push(Job {
                name: line.trim().trim_end_matches(':'),
                lines: Vec::new(),
            });
        } else if let Some(job) = jobs.last_mut() {
            job.lines.push(line);
        }
    }
    jobs
}

fn find_job<'a, 'b>(jobs: &'b [Job<'a>], name: &str) -> &'b Job<'a> {
    jobs.iter()
        .find(|job| job.name == name)
        .unwrap_or_else(|| panic!("no job named {name}"))
}

struct Step<'a> {
    lines: Vec<&'a str>,
}

impl<'a> Step<'a> {
    /// A key directly on the step, such as `name`, `uses` or `run`.
    fn field(&self, key: &str) -> Option<&'a str> {
        self.lines.iter().find_map(|line| {
            let rest = line
                .strip_prefix("      - ")
                .or_else(|| line.strip_prefix("        "))?;
            rest.strip_prefix(key)?.strip_prefix(':').map(str::trim)
        })
    }

    /// The `key: value` lines under `with:` or `env:`.
    fn block(&self, key: &str) -> Vec<(&'a str, &'a str)> {
        let header = format!("        {key}:");
        let Some(at) = self.lines.iter().position(|line| **line == header) else {
            return Vec::new();
        };
        self.lines[at + 1..]
            .iter()
            .take_while(|line| line.starts_with("          "))
            .filter_map(|line| line.trim().split_once(": "))
            .collect()
    }

    fn input(&self, key: &str) -> Option<&'a str> {
        self.block("with")
            .into_iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }

    /// The step's shell script, unindented.
    fn script(&self) -> Option<String> {
        let first = self.field("run")?;
        if first != "|" {
            return Some(format!("{first}\n"));
        }
        let at = self
            .lines
            .iter()
            .position(|line| line.trim_end() == "        run: |")?;
        let body: Vec<&str> = self.lines[at + 1..]
            .iter()
            .take_while(|line| line.is_empty() || line.starts_with("          "))
            .map(|line| line.get(10..).unwrap_or(""))
            .collect();
        Some(format!("{}\n", body.join("\n").trim_end()))
    }

    fn label(&self) -> String {
        self.field("name")
            .or_else(|| self.field("uses"))
            .or_else(|| self.field("run"))
            .unwrap_or("a step")
            .to_string()
    }
}

/// The steps run on Linux runners, so they're tested on Linux. To run them anywhere else, set
/// RELEASE_TEST_BASH to a bash that can find sha256sum, diff and jq.
fn bash() -> Option<OsString> {
    if let Some(bash) = env::var_os("RELEASE_TEST_BASH") {
        return Some(bash);
    }
    let has_jq = || Command::new("jq").arg("--version").output().is_ok();
    if cfg!(target_os = "linux") && (env::var_os("CI").is_some() || has_jq()) {
        return Some("bash".into());
    }
    eprintln!("Not run: this needs Linux with jq, or RELEASE_TEST_BASH set.");
    None
}

/// Its "builds" of xtask are small programs that run the real binaries, because those are too
/// big to copy from job to job here.
const FAKE_CARGO: &str = r#"#!/bin/bash
echo "cargo $*" >> ../../log
mkdir -p target/debug target/release
build() {
  printf '#!/bin/bash\nexec "$%s" "$@"\n' "$2" > "target/debug/$1"
  chmod +x "target/debug/$1"
}
case "$*" in
  *"--bin xtask-sign"*) build xtask-sign REAL_XTASK_SIGN ;;
  *"-p xtask"*) build xtask REAL_XTASK ;;
  *"-p wow-is-a-feeling"*) echo "the app" > target/release/wow-is-a-feeling ;;
  *) echo "unexpected: cargo $*" >&2; exit 1 ;;
esac
"#;

const FAKE_GH: &str = r#"#!/bin/bash
echo "gh $*" >> ../../log
if [ "$GH_TOKEN" != fake-token ] || [ "$GH_REPO" != owner/repo ]; then
  echo "gh wasn't given a token and a repository" >&2
  exit 1
fi
case "$1 $2" in
  "release view")
    [ -d ../../github ]
    ;;
  "release create")
    mkdir -p ../../github
    for arg in "$@"; do
      case "$arg" in release/*) cp "$arg" ../../github/ ;; esac
    done
    ;;
  "release download")
    mkdir -p "$5"
    cp ../../github/* "$5/"
    if [ -n "$TAMPER_STORED" ]; then echo changed >> "$5/$TAMPER_STORED"; fi
    ;;
  "release edit") ;;
  *) echo "unexpected: gh $*" >&2; exit 1 ;;
esac
"#;

const FAKE_CURL: &str = r#"#!/bin/bash
echo "curl $*" >> ../../log
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out=$2; shift 2 ;;
    https://*) url=$1; shift ;;
    *) shift ;;
  esac
done
if [ -n "$BAD_DOWNLOADS" ]; then
  # What a bad upstream release would serve, which leaves a mark if anything ever runs it.
  printf '#!/bin/bash\ntouch ran-the-bad-download\n' > "$out"
  exit 0
fi
cp "../../github/${url##*/}" "$out"
if [ "${url##*/}" = "$TAMPER_PUBLIC" ]; then echo changed >> "$out"; fi
"#;

/// Bash reads this before each step, so the stand-ins win over anything already on the path.
const FAKES_FIRST: &str = r#"unset BASH_ENV
PATH="$(cd ../../bin && pwd):$PATH"
"#;

const NOTHING: &str = "#!/bin/bash\nexit 0\n";

/// Writing a program and then running it can fail with "text file busy" if another thread
/// forks while the file is open for writing, so the tests that run things take turns.
fn one_at_a_time() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A temp folder laid out like a runner: artifacts in `store`, GitHub's copy of the release in
/// `github`, a workspace per job in `work`, and what the stand-ins were asked to do in `log`.
struct Rig {
    _temp: tempfile::TempDir,
    _turn: MutexGuard<'static, ()>,
    root: PathBuf,
    bash: OsString,
    secret_key: String,
    public_key: String,
}

#[derive(Debug)]
struct Failure {
    step: String,
    output: String,
}

impl Rig {
    fn new() -> Option<Rig> {
        let bash = bash()?;
        let turn = one_at_a_time();
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for folder in ["bin", "store", "work"] {
            fs::create_dir(root.join(folder)).unwrap();
        }
        fs::write(root.join("log"), "").unwrap();
        fs::write(root.join("fakes-first.sh"), FAKES_FIRST).unwrap();
        for (name, script) in [
            ("cargo", FAKE_CARGO),
            ("gh", FAKE_GH),
            ("curl", FAKE_CURL),
            ("rustup", NOTHING),
            ("sudo", NOTHING),
        ] {
            write_program(&root.join("bin").join(name), script);
        }

        let key_file = root.join("throwaway.minisign.key");
        let output = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("keygen")
            .arg(&key_file)
            .output()
            .unwrap();
        assert!(output.status.success());
        let public_key = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let secret_key = fs::read_to_string(&key_file).unwrap();
        Some(Rig {
            _temp: temp,
            _turn: turn,
            root,
            bash,
            secret_key,
            public_key,
        })
    }

    fn store(&self, artifact: &str) -> PathBuf {
        self.root.join("store").join(artifact)
    }

    fn work(&self, job: &str) -> PathBuf {
        let work = self.root.join("work").join(job);
        fs::create_dir_all(&work).unwrap();
        work
    }

    fn log(&self) -> String {
        fs::read_to_string(self.root.join("log")).unwrap()
    }

    fn fake_builds(&self) {
        for (artifact, names) in BUILDS {
            fs::create_dir_all(self.store(artifact)).unwrap();
            for name in names {
                fs::write(self.store(artifact).join(name), format!("build of {name}")).unwrap();
            }
        }
    }

    /// Everything up to and including the signature.
    fn signed_release(&self) {
        self.fake_builds();
        for job in ["signer", "manifest", "sign"] {
            self.run_job(job, &[]).unwrap();
        }
    }

    /// Run a job's steps in order, like a runner would, stopping at the first that fails.
    fn run_job(&self, name: &str, extra_env: &[(&str, &str)]) -> Result<(), Failure> {
        self.run_job_except(name, "", extra_env)
    }

    fn run_job_except(
        &self,
        name: &str,
        skip: &str,
        extra_env: &[(&str, &str)],
    ) -> Result<(), Failure> {
        let text = workflow("release.yml");
        let jobs = jobs(&text);
        let work = self.work(name);
        for step in find_job(&jobs, name).steps() {
            if step.label() == skip {
                continue;
            }
            let result = match step.field("uses") {
                Some(action) => self.run_action(action, &step, &work),
                None => self.run_step(&step, &work, extra_env, true),
            };
            result.map_err(|output| Failure {
                step: step.label(),
                output,
            })?;
        }
        Ok(())
    }

    fn run_action(&self, uses: &str, step: &Step, work: &Path) -> Result<(), String> {
        let action = uses.split('@').next().unwrap();
        match action {
            "actions/checkout" => {
                for file in CHECKOUT {
                    let to = work.join(file);
                    fs::create_dir_all(to.parent().unwrap()).unwrap();
                    fs::copy(repo_root().join(file), to).unwrap();
                }
                Ok(())
            }
            "actions/upload-artifact" => {
                let name = step.input("name").unwrap();
                let path = step.input("path").unwrap();
                let files = if let Some(folder) = path.strip_suffix("/*") {
                    fs::read_dir(work.join(folder))
                        .map_err(|e| format!("nothing to upload at {path}: {e}"))?
                        .map(|entry| entry.unwrap().path())
                        .collect()
                } else {
                    vec![work.join(path)]
                };
                fs::create_dir(self.store(name))
                    .map_err(|e| format!("artifact {name} uploaded twice: {e}"))?;
                for file in files {
                    fs::copy(&file, self.store(name).join(file.file_name().unwrap()))
                        .map_err(|e| format!("nothing to upload at {path}: {e}"))?;
                }
                Ok(())
            }
            "actions/download-artifact" => {
                let name = step.input("name").unwrap();
                let into = work.join(step.input("path").unwrap());
                fs::create_dir_all(&into).unwrap();
                let entries =
                    fs::read_dir(self.store(name)).map_err(|_| format!("no artifact {name}"))?;
                for entry in entries {
                    let entry = entry.unwrap();
                    let to = into.join(entry.file_name());
                    fs::copy(entry.path(), &to).unwrap();
                    drop_exec_bit(&to);
                }
                Ok(())
            }
            "actions/attest-build-provenance" => Ok(()),
            other => panic!("{other} isn't expected in the release workflow"),
        }
    }

    fn run_step(
        &self,
        step: &Step,
        work: &Path,
        extra_env: &[(&str, &str)],
        fakes: bool,
    ) -> Result<(), String> {
        let script = step.script().unwrap();
        let file = self.root.join("step.sh");
        fs::write(&file, script).unwrap();
        // This is the command line a runner uses for `shell: bash`.
        let mut command = Command::new(&self.bash);
        command
            .args(["--noprofile", "--norc", "-eo", "pipefail"])
            .arg(&file)
            .current_dir(work)
            .env_remove("MINISIGN_SECRET_KEY")
            .env("GITHUB_REPOSITORY", REPOSITORY)
            .env("REAL_XTASK", env!("CARGO_BIN_EXE_xtask"))
            .env("REAL_XTASK_SIGN", env!("CARGO_BIN_EXE_xtask-sign"));
        if !extra_env.iter().any(|(key, _)| *key == "GITHUB_REF_NAME") {
            command.env("GITHUB_REF_NAME", release_tag());
        }
        if fakes {
            command.env("BASH_ENV", "../../fakes-first.sh");
        }
        for (key, value) in step.block("env") {
            let value = match value {
                "${{ secrets.MINISIGN_SECRET_KEY }}" => self.secret_key.clone(),
                "${{ github.token }}" => "fake-token".to_string(),
                "${{ github.repository }}" => REPOSITORY.to_string(),
                other => panic!("{key} is set to {other}, which this test doesn't know"),
            };
            command.env(key, value);
        }
        command.envs(extra_env.iter().copied());
        let output = command.output().unwrap();
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if output.status.success() {
            Ok(())
        } else {
            Err(printed)
        }
    }

    /// The tag check's own steps, with a real cargo in the real repository.
    fn check_tag(&self, tag: &str) -> Result<(), Failure> {
        let text = workflow("release.yml");
        let jobs = jobs(&text);
        let cargo_folder = Path::new(env!("CARGO")).parent().unwrap();
        let path = env::var_os("PATH").unwrap_or_default();
        let paths = env::split_paths(&path).chain([cargo_folder.to_path_buf()]);
        let path = env::join_paths(paths).unwrap();
        let path = path.to_str().unwrap();
        for step in find_job(&jobs, "check-tag").steps() {
            if step.script().is_some() {
                self.run_step(
                    &step,
                    repo_root(),
                    &[("GITHUB_REF_NAME", tag), ("PATH", path)],
                    false,
                )
                .map_err(|output| Failure {
                    step: step.label(),
                    output,
                })?;
            }
        }
        Ok(())
    }
}

fn write_program(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// An artifact is a zip, which doesn't keep the exec bit.
fn drop_exec_bit(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn file_names(folder: &Path) -> BTreeSet<String> {
    fs::read_dir(folder)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn every_action_is_pinned_to_a_full_commit_with_its_version_noted() {
    let mut pinned = 0;
    for (name, text) in all_workflows() {
        for value in text.lines().filter_map(uses_value) {
            let (reference, version) = value
                .split_once(" # ")
                .unwrap_or_else(|| panic!("{name}: `{value}` doesn't say which version it is"));
            let (_, commit) = reference
                .split_once('@')
                .unwrap_or_else(|| panic!("{name}: `{value}` isn't pinned"));
            assert!(
                commit.len() == 40
                    && commit
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{name}: `{value}` is pinned to something that can move"
            );
            assert!(!version.trim().is_empty(), "{name}: `{value}`");
            pinned += 1;
        }
    }
    assert!(pinned >= 10, "only found {pinned} actions");
}

/// The action's master branch has no default toolchain, and exits with an error without one.
#[test]
fn every_rust_toolchain_step_names_its_toolchain() {
    for (name, text) in all_workflows() {
        let lines: Vec<&str> = text.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            if uses_value(line).is_some_and(|value| value.starts_with("dtolnay/rust-toolchain@")) {
                let inputs = lines[at + 1..]
                    .iter()
                    .take_while(|next| !next.trim_start().starts_with("- "));
                assert!(
                    inputs
                        .into_iter()
                        .any(|input| input.trim_start().starts_with("toolchain: ")),
                    "{name}, line {}",
                    at + 1
                );
            }
        }
    }
}

#[test]
fn only_github_actions_run_in_the_release_and_nothing_restores_a_cache() {
    for value in workflow("release.yml").lines().filter_map(uses_value) {
        assert!(value.starts_with("actions/"), "{value}");
        assert!(!value.starts_with("actions/cache"), "{value}");
    }
}

#[test]
fn every_job_waits_for_the_tag_check() {
    let text = workflow("release.yml");
    let jobs = jobs(&text);
    assert_eq!(jobs[0].name, "check-tag");
    assert!(jobs[0].needs().is_empty());
    assert!(
        jobs[0].steps()[0].script().is_some(),
        "the shape of the tag is checked before anything else runs"
    );
    for job in &jobs[1..] {
        let mut seen = BTreeSet::new();
        let mut waiting = job.needs();
        while let Some(name) = waiting.pop() {
            if seen.insert(name) {
                waiting.extend(find_job(&jobs, name).needs());
            }
        }
        assert!(seen.contains("check-tag"), "{} doesn't wait", job.name);
    }
}

#[test]
fn only_the_version_in_cargo_toml_gets_past_the_first_job() {
    let Some(rig) = Rig::new() else { return };
    rig.check_tag(&release_tag()).unwrap();

    for bad_shape in [
        "vendor-sync",
        "v0.3.0-beta",
        "v1.2",
        "v1.2.3.4",
        "v1.2.x",
        "1.2.3",
        "v",
    ] {
        let failure = rig.check_tag(bad_shape).unwrap_err();
        assert_eq!(
            failure.step, "Check the tag looks like a release",
            "{failure:?}"
        );
        assert!(
            failure.output.contains("isn't a release tag"),
            "{failure:?}"
        );
    }

    let failure = rig.check_tag("v9999.9999.9999").unwrap_err();
    assert_eq!(failure.step, "Check the tag matches Cargo.toml");
    assert!(
        failure.output.contains(&format!(
            "version {} in Cargo.toml",
            env!("CARGO_PKG_VERSION")
        )),
        "{failure:?}"
    );
}

/// With the key in view, only one small binary and GitHub's own download and upload actions
/// ever run, and the key isn't in any other step's environment.
#[test]
fn the_key_is_in_one_step_of_one_small_job() {
    let mentions = all_workflows()
        .iter()
        .flat_map(|(_, text)| text.lines())
        .filter(|line| line.contains("secrets.MINISIGN_SECRET_KEY"))
        .count();
    assert_eq!(mentions, 1, "the key is referenced from somewhere else");

    let text = workflow("release.yml");
    let jobs = jobs(&text);
    let sign = find_job(&jobs, "sign");
    assert!(sign.lines.contains(&"    permissions: {}"));
    let mut keyed_steps = 0;
    for step in sign.steps() {
        if step.block("env") == [("MINISIGN_SECRET_KEY", "${{ secrets.MINISIGN_SECRET_KEY }}")] {
            keyed_steps += 1;
            assert_eq!(
                step.script().unwrap(),
                "signer/xtask-sign manifest/latest.json\n"
            );
        } else if let Some(uses) = step.field("uses") {
            assert!(
                uses.starts_with("actions/download-artifact@")
                    || uses.starts_with("actions/upload-artifact@"),
                "{uses}"
            );
        } else {
            assert_eq!(step.script().unwrap(), "chmod +x signer/xtask-sign\n");
        }
    }
    assert_eq!(keyed_steps, 1);
}

#[test]
fn the_signing_tool_is_built_alone_and_every_build_is_locked() {
    let text = workflow("release.yml");
    let jobs = jobs(&text);
    let builds: Vec<String> = find_job(&jobs, "signer")
        .steps()
        .iter()
        .filter_map(Step::script)
        .filter(|script| script.contains("cargo"))
        .collect();
    assert_eq!(
        builds,
        ["cargo build --locked -p xtask --no-default-features --bin xtask-sign\n"]
    );
    for line in text.lines().filter(|line| line.contains("cargo ")) {
        let line = line.trim_start().trim_start_matches("- run: ");
        if line.starts_with("cargo build") {
            assert!(line.contains("--locked"), "{line}");
        } else {
            assert!(line.contains("cargo metadata --no-deps"), "{line}");
        }
    }
}

#[test]
fn no_checkout_in_the_release_keeps_a_token() {
    let text = workflow("release.yml");
    let mut checkouts = 0;
    for job in jobs(&text) {
        for step in job.steps() {
            if step
                .field("uses")
                .is_some_and(|uses| uses.starts_with("actions/checkout@"))
            {
                checkouts += 1;
                assert_eq!(
                    step.input("persist-credentials"),
                    Some("false"),
                    "{}",
                    job.name
                );
            }
        }
    }
    assert!(checkouts > 0);
}

/// The job that can write to the repository runs only GitHub's actions, `gh` and small tools.
#[test]
fn only_publish_can_write_and_it_runs_nothing_built_here() {
    let text = workflow("release.yml");
    assert!(text.contains("\npermissions:\n  contents: read\n"));
    let jobs = jobs(&text);
    for job in &jobs {
        let writes = job.lines.iter().any(|line| line.ends_with(": write"));
        assert_eq!(writes, job.name == "publish", "{}", job.name);
    }
    for step in find_job(&jobs, "publish").steps() {
        assert_ne!(step.input("name"), Some("tool"));
        assert_ne!(step.input("name"), Some("signer"));
        if let Some(script) = step.script() {
            for built_here in ["xtask", "cargo", "chmod"] {
                assert!(!script.contains(built_here), "{built_here} in:\n{script}");
            }
        }
    }
}

/// Every artifact a job downloads was uploaded by a job it waits for.
#[test]
fn every_download_comes_from_a_job_that_ran_first() {
    let text = workflow("release.yml");
    let jobs = jobs(&text);
    for job in &jobs {
        let mut before = BTreeSet::new();
        let mut waiting = job.needs();
        while let Some(name) = waiting.pop() {
            if before.insert(name) {
                waiting.extend(find_job(&jobs, name).needs());
            }
        }
        let uploaded: BTreeSet<&str> = before
            .iter()
            .flat_map(|name| find_job(&jobs, name).steps())
            .filter(|step| {
                step.field("uses")
                    .is_some_and(|uses| uses.starts_with("actions/upload-artifact@"))
            })
            .filter_map(|step| step.input("name"))
            .collect();
        for step in job.steps() {
            if step
                .field("uses")
                .is_some_and(|uses| uses.starts_with("actions/download-artifact@"))
            {
                let name = step.input("name").expect("downloads name their artifact");
                assert!(uploaded.contains(name), "{}: {name}", job.name);
            }
        }
    }
}

#[test]
fn the_chain_publishes_exactly_what_was_hashed_and_signed() {
    let Some(rig) = Rig::new() else { return };
    rig.signed_release();
    let tag = release_tag();

    // The app's own check passes on what the sign job made, for the key it was given.
    let json = fs::read(rig.store("manifest").join("latest.json")).unwrap();
    let signature = fs::read(rig.store("signature").join("latest.json.minisig")).unwrap();
    let manifest = verified_manifest(&json, &signature, &rig.public_key).unwrap();
    assert_eq!(manifest.version, env!("CARGO_PKG_VERSION"));
    assert!(!manifest.notes.is_empty());
    assert_eq!(manifest.assets.len(), 4);

    // The verify job looks at that same signature, and refuses it because this isn't the key
    // built into the app. The first real release is what proves it passes with the real key.
    let failure = rig.run_job("verify", &[]).unwrap_err();
    assert!(
        failure.output.contains("failed verification"),
        "{failure:?}"
    );

    rig.run_job("publish", &[]).unwrap();

    let mut expected: BTreeSet<String> = BUILDS
        .iter()
        .flat_map(|(_, names)| names.iter().map(|name| name.to_string()))
        .collect();
    expected.extend(["latest.json", "latest.json.minisig", "SHA256SUMS"].map(String::from));
    assert_eq!(file_names(&rig.root.join("github")), expected);

    let base = format!("https://github.com/{REPOSITORY}/releases/download/{tag}/");
    for asset in manifest.assets.values() {
        let name = asset.url.strip_prefix(&base).unwrap();
        let published = fs::read(rig.root.join("github").join(name)).unwrap();
        let sha256: String = Sha256::digest(&published)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(sha256, asset.sha256, "{name}");
        assert_eq!(published.len() as u64, asset.size, "{name}");
    }

    // A draft first, read back, and only then public, and then every link gets followed.
    let log = rig.log();
    let order: Vec<usize> = [
        format!("gh release create {tag}"),
        format!("gh release download {tag} --dir stored"),
        format!("gh release edit {tag} --draft=false --latest"),
        "curl ".to_string(),
    ]
    .iter()
    .map(|needle| {
        log.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {log}"))
    })
    .collect();
    assert!(order.is_sorted(), "{log}");
    assert!(log.contains("--draft --verify-tag"), "{log}");
    let followed = log.lines().filter(|line| line.starts_with("curl ")).count();
    assert_eq!(
        followed, 6,
        "the 4 updates, latest.json and its signature: {log}"
    );
    assert!(!log.contains("--draft=true"), "{log}");
    assert_eq!(
        fs::read(rig.root.join("github").join("latest.json")).unwrap(),
        json
    );
}

/// With --draft, gh itself wouldn't notice, and a second draft would sit beside the first.
#[test]
fn a_tag_that_already_has_a_release_is_left_alone() {
    let Some(rig) = Rig::new() else { return };
    rig.signed_release();
    rig.run_job("publish", &[]).unwrap();
    fs::remove_dir_all(rig.work("publish")).unwrap();

    let failure = rig.run_job("publish", &[]).unwrap_err();
    assert_eq!(failure.step, "Publish");
    assert!(
        failure.output.contains("already has a release"),
        "{failure:?}"
    );
    assert_eq!(rig.log().matches("gh release create").count(), 1);
}

#[test]
fn a_changed_asset_never_reaches_github() {
    let Some(rig) = Rig::new() else { return };
    rig.signed_release();
    fs::write(
        rig.store("assets").join("wow-is-a-feeling-windows-x64.exe"),
        "not what was signed",
    )
    .unwrap();

    let failure = rig.run_job("publish", &[]).unwrap_err();
    assert_eq!(failure.step, "Check each asset against the signed hashes");
    assert!(!rig.log().contains("gh release"), "{}", rig.log());
}

#[test]
fn a_release_github_stored_differently_stays_a_draft() {
    let Some(rig) = Rig::new() else { return };
    rig.signed_release();

    let failure = rig
        .run_job("publish", &[("TAMPER_STORED", "latest.json")])
        .unwrap_err();
    assert_eq!(failure.step, "Publish");
    assert!(failure.output.contains("stays a draft"), "{failure:?}");
    assert!(rig.log().contains("gh release create"), "{}", rig.log());
    assert!(!rig.log().contains("gh release edit"), "{}", rig.log());
}

#[test]
fn a_wrong_public_link_takes_the_release_back_down() {
    let Some(rig) = Rig::new() else { return };
    rig.signed_release();

    let failure = rig
        .run_job(
            "publish",
            &[("TAMPER_PUBLIC", "wow-is-a-feeling-linux-x64.gz")],
        )
        .unwrap_err();
    assert_eq!(failure.step, "Publish");
    assert!(failure.output.contains("a draft again"), "{failure:?}");
    let log = rig.log();
    let released = log.find("--draft=false").unwrap();
    let pulled = log.find("--draft=true").unwrap();
    assert!(released < pulled, "{log}");
}

/// A build can't add a file to the release, swap in another build's file, or leave one out.
#[test]
fn gather_refuses_anything_but_the_expected_files() {
    type Spoil = fn(&Rig);
    let cases: [(&str, Spoil); 3] = [
        ("an extra file", |rig| {
            fs::write(rig.store("linux").join("extra.txt"), "surprise").unwrap();
        }),
        ("another build's name", |rig| {
            fs::write(
                rig.store("linux").join("wow-is-a-feeling-windows-x64.exe"),
                "not the Windows build",
            )
            .unwrap();
        }),
        ("a missing file", |rig| {
            fs::remove_file(
                rig.store("macos")
                    .join("wow-is-a-feeling-macos-universal.dmg"),
            )
            .unwrap();
        }),
    ];
    for (what, spoil) in cases {
        let Some(rig) = Rig::new() else { return };
        rig.fake_builds();
        spoil(&rig);
        let failure = rig.run_job("manifest", &[]).unwrap_err();
        assert_eq!(failure.step, "Gather assets", "{what}: {failure:?}");
        assert!(!rig.store("manifest").exists(), "{what}");
    }
}

/// The weekly check reads the catalog the app really fetches, needs no more than read access,
/// and its exact arguments fail the run on a broken catalog. No network needed to see that.
#[test]
fn the_weekly_catalog_check_fails_on_a_broken_catalog() {
    let text = workflow("catalog-check.yml");
    assert!(text.contains(feeling_core::catalog::CATALOG_URL), "{text}");
    assert!(text.contains("\n  schedule:\n    - cron: "), "{text}");
    assert!(
        text.contains("\npermissions:\n  contents: read\n\n"),
        "{text}"
    );
    assert_eq!(text.matches("permissions:").count(), 1, "{text}");

    let jobs = jobs(&text);
    let command = jobs[0]
        .steps()
        .iter()
        .filter_map(Step::script)
        .find(|script| script.contains("check-catalog"))
        .unwrap();
    let words: Vec<&str> = command.split_whitespace().collect();
    assert_eq!(
        words[..5],
        [
            "target/debug/xtask",
            "check-catalog",
            "catalog.json",
            "40",
            "100"
        ]
    );

    let _turn = one_at_a_time();
    let dir = tempfile::tempdir().unwrap();
    let check = |catalog: &str| {
        fs::write(dir.path().join("catalog.json"), catalog).unwrap();
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(&words[1..])
            .current_dir(dir.path())
            .output()
            .unwrap()
    };
    assert!(
        check(r#"{"schema":1,"generated":0,"addons":[]}"#)
            .status
            .success()
    );
    let broken = check(r#"{"schema":1,"generated":0,"addons":[],"featured":["gh:gone/addon"]}"#);
    assert!(!broken.status.success());
    let printed = String::from_utf8_lossy(&broken.stdout);
    assert!(printed.contains("gh:gone/addon"), "{printed}");
    assert!(!check("this isn't a catalog").status.success());
}

#[test]
fn appimagetool_and_its_runtime_come_from_fixed_releases_with_checked_hashes() {
    let text = workflow("release.yml");
    assert!(!text.contains("continuous"), "that release moves");
    let jobs = jobs(&text);
    let steps = find_job(&jobs, "linux").steps();
    let fetch = steps
        .iter()
        .find(|step| step.label() == "Fetch appimagetool and its runtime")
        .unwrap()
        .script()
        .unwrap();
    for (release, name) in [
        ("/AppImage/appimagetool/releases/download/", "appimagetool"),
        (
            "/AppImage/type2-runtime/releases/download/",
            "runtime-x86_64",
        ),
    ] {
        assert!(fetch.contains(release), "{fetch}");
        let hash = fetch
            .lines()
            .find_map(|line| line.strip_suffix(&format!("  {name}")))
            .unwrap_or_else(|| panic!("no hash for {name}: {fetch}"));
        assert_eq!(hash.len(), 64, "{hash}");
        assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()), "{hash}");
    }
}

/// The stand-in plays a bad upstream release: it gets caught before anything runs it, and
/// before the slow build starts.
#[test]
fn a_bad_download_stops_the_linux_job_before_anything_runs_it() {
    let Some(rig) = Rig::new() else { return };
    let failure = rig.run_job("linux", &[("BAD_DOWNLOADS", "1")]).unwrap_err();
    assert_eq!(failure.step, "Fetch appimagetool and its runtime");
    assert!(!rig.work("linux").join("ran-the-bad-download").exists());
    assert!(!rig.log().contains("cargo build"), "{}", rig.log());
    assert!(!rig.store("linux").exists());
}

/// The rest of the Linux job, with a stand-in for the tool, makes exactly what Gather takes
/// and hands the tool the checked runtime.
#[test]
fn the_linux_job_packages_with_the_checked_runtime() {
    let Some(rig) = Rig::new() else { return };
    write_program(
        &rig.work("linux").join("appimagetool"),
        "#!/bin/bash\necho \"appimagetool ARCH=$ARCH $*\" >> ../../log\nfor last; do :; done\necho an-appimage > \"$last\"\n",
    );
    rig.run_job_except("linux", "Fetch appimagetool and its runtime", &[])
        .unwrap();

    let expected: BTreeSet<String> = BUILDS
        .iter()
        .find(|(artifact, _)| *artifact == "linux")
        .unwrap()
        .1
        .iter()
        .map(|name| name.to_string())
        .collect();
    assert_eq!(file_names(&rig.store("linux")), expected);
    let args = "--appimage-extract-and-run --runtime-file runtime-x86_64 AppDir dist/wow-is-a-feeling-linux-x64.AppImage";
    assert!(
        rig.log()
            .contains(&format!("appimagetool ARCH=x86_64 {args}")),
        "{}",
        rig.log()
    );
}

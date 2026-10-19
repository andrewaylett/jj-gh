//! End-to-end: real `jj`, a real (local, bare) git remote, and a mock GitHub.
//! Skipped when `jj` isn't installed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tempfile::TempDir;

#[derive(Clone, Debug)]
struct Pr {
    number: u64,
    title: String,
    body: String,
    state: &'static str,
    base: String,
    head: String,
}

#[derive(Default)]
struct State {
    prs: BTreeMap<u64, Pr>,
    /// Native stacks, each bottom to top.
    stacks: Vec<Vec<u64>>,
}

struct Mock {
    url: String,
    state: Arc<Mutex<State>>,
}

fn sh(dir: &Path, program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("XDG_CONFIG_HOME", dir.join(".xdg"))
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Squash-merge `head` into main on the bare remote, as GitHub would.
fn squash_merge(bare: &Path, scratch: &Path, head: &str, title: &str) {
    let work = scratch.join("squash");
    let _ = std::fs::remove_dir_all(&work);
    sh(
        scratch,
        "git",
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    sh(&work, "git", &["checkout", "-q", "main"]);
    sh(
        &work,
        "git",
        &["merge", "--squash", &format!("origin/{head}")],
    );
    sh(&work, "git", &["commit", "-q", "-m", title]);
    sh(&work, "git", &["push", "-q", "origin", "main"]);
}

fn start_mock(bare: PathBuf, scratch: PathBuf) -> Mock {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let state = Arc::new(Mutex::new(State::default()));
    let st = state.clone();
    std::thread::spawn(move || {
        for mut req in server.incoming_requests() {
            let mut body = String::new();
            req.as_reader().read_to_string(&mut body).unwrap();
            let method = req.method().to_string();
            let path = req.url().to_string();
            let json_body: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let mut st = st.lock().unwrap();
            let reply: (u16, Value) = match (method.as_str(), path.as_str()) {
                ("POST", "/graphql") => {
                    let q = json_body["query"].as_str().unwrap_or_default();
                    (200, graphql(&st, &bare, q))
                }
                ("POST", "/repos/o/r/pulls") => {
                    let n = st.prs.len() as u64 + 1;
                    let pr = Pr {
                        number: n,
                        title: json_body["title"].as_str().unwrap().into(),
                        body: json_body["body"].as_str().unwrap().into(),
                        state: "OPEN",
                        base: json_body["base"].as_str().unwrap().into(),
                        head: json_body["head"].as_str().unwrap().into(),
                    };
                    st.prs.insert(n, pr);
                    (
                        201,
                        json!({"number": n, "html_url": format!("https://example.test/o/r/pull/{n}")}),
                    )
                }
                ("PATCH", p) if p.starts_with("/repos/o/r/pulls/") => {
                    let n: u64 = p.rsplit('/').next().unwrap().parse().unwrap();
                    let pr = st.prs.get_mut(&n).unwrap();
                    if let Some(v) = json_body["base"].as_str() {
                        pr.base = v.into();
                    }
                    if let Some(v) = json_body["title"].as_str() {
                        pr.title = v.into();
                    }
                    if let Some(v) = json_body["body"].as_str() {
                        pr.body = v.into();
                    }
                    (200, json!({"number": n}))
                }
                ("GET", p) if p.starts_with("/repos/o/r/stacks?pull_request=") => {
                    let n: u64 = p.rsplit('=').next().unwrap().parse().unwrap();
                    let found: Vec<Value> = st
                        .stacks
                        .iter()
                        .enumerate()
                        .filter(|(_, s)| s.contains(&n))
                        .map(|(i, s)| stack_json(i, s))
                        .collect();
                    (200, json!(found))
                }
                ("POST", "/repos/o/r/stacks") => {
                    let nums = stack_numbers(&json_body);
                    st.stacks.push(nums);
                    (
                        201,
                        stack_json(st.stacks.len() - 1, st.stacks.last().unwrap()),
                    )
                }
                ("POST", p) if p.ends_with("/add") => {
                    let i = stack_index(p);
                    st.stacks[i].extend(stack_numbers(&json_body));
                    (200, stack_json(i, &st.stacks[i]))
                }
                ("POST", p) if p.ends_with("/unstack") => {
                    let i = stack_index(p);
                    st.stacks[i].clear();
                    (204, Value::Null)
                }
                ("PUT", p) if p.ends_with("/merge") => {
                    let n: u64 = p.split('/').rev().nth(1).unwrap().parse().unwrap();
                    let pr = st.prs.get_mut(&n).unwrap();
                    squash_merge(&bare, &scratch, &pr.head, &format!("{} (#{n})", pr.title));
                    pr.state = "MERGED";
                    // GitHub retargets the PRs stacked on a merged one at its base.
                    let (head, base) = (pr.head.clone(), pr.base.clone());
                    for other in st.prs.values_mut().filter(|o| o.base == head) {
                        other.base = base.clone();
                    }
                    (200, json!({"merged": true}))
                }
                _ => (
                    404,
                    json!({"message": format!("mock: unhandled {method} {path}")}),
                ),
            };
            let resp = tiny_http::Response::from_string(reply.1.to_string())
                .with_status_code(reply.0)
                .with_header(
                    "Content-Type: application/json"
                        .parse::<tiny_http::Header>()
                        .unwrap(),
                );
            req.respond(resp).unwrap();
        }
    });
    Mock { url, state }
}

fn stack_numbers(body: &Value) -> Vec<u64> {
    body["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_u64().unwrap())
        .collect()
}

/// Stack numbers are 1-based indexes into `State::stacks`.
fn stack_index(path: &str) -> usize {
    path.split('/')
        .rev()
        .nth(1)
        .unwrap()
        .parse::<usize>()
        .unwrap()
        - 1
}

fn stack_json(index: usize, numbers: &[u64]) -> Value {
    json!({
        "number": index + 1,
        "pull_requests": numbers.iter().map(|n| json!({"number": n, "merged_at": null})).collect::<Vec<_>>(),
    })
}

fn graphql(st: &State, bare: &Path, q: &str) -> Value {
    if q.contains("viewer") {
        return json!({"data": {"viewer": {"login": "tester"}}});
    }
    if q.contains("defaultBranchRef") {
        return json!({"data": {"repository": {"defaultBranchRef": {"name": "main"}}}});
    }
    let mut repo = serde_json::Map::new();
    let mut rest = q;
    while let Some(i) = rest.find("pullRequests(headRefName: ") {
        let alias = rest[..i]
            .trim_end()
            .rsplit(|c: char| c.is_whitespace())
            .next()
            .unwrap();
        let alias = alias.trim_end_matches(':').to_string();
        rest = &rest[i + "pullRequests(headRefName: ".len()..];
        let head: String = serde_json::Deserializer::from_str(rest)
            .into_iter::<String>()
            .next()
            .unwrap()
            .unwrap();
        let mut nodes: Vec<&Pr> = st.prs.values().filter(|p| p.head == head).collect();
        nodes.reverse();
        let oid = Command::new("git")
            .args([
                "--git-dir",
                bare.to_str().unwrap(),
                "rev-parse",
                &format!("refs/heads/{head}"),
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let nodes: Vec<Value> = nodes
            .into_iter()
            .map(|p| {
                json!({
                    "number": p.number, "title": p.title, "body": p.body,
                    "url": format!("https://example.test/o/r/pull/{}", p.number),
                    "state": p.state, "isDraft": false,
                    "baseRefName": p.base, "headRefName": p.head, "headRefOid": oid,
                    "reviewDecision": null, "mergeable": "MERGEABLE",
                    "commits": {"nodes": []},
                })
            })
            .collect();
        repo.insert(alias, json!({"nodes": nodes}));
    }
    json!({"data": {"repository": repo}})
}

struct Env {
    dir: TempDir,
    mock: Mock,
}

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

impl Env {
    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn jj(&self, args: &[&str]) -> String {
        let out = Command::new("jj")
            .args(["--no-pager", "--color", "never"])
            .args(args)
            .current_dir(self.work())
            .env("XDG_CONFIG_HOME", self.dir.path().join(".xdg"))
            .env("JJ_USER", "Test")
            .env("JJ_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "jj {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn gh(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_jj-gh"))
            .args(args)
            .current_dir(self.work())
            .env("XDG_CONFIG_HOME", self.dir.path().join(".xdg"))
            .env("JJ_USER", "Test")
            .env("JJ_EMAIL", "test@example.com")
            .env("JJ_GH_API_URL", &self.mock.url)
            .env("JJ_GH_GRAPHQL_URL", format!("{}/graphql", self.mock.url))
            .env("JJ_GH_REPO", "o/r")
            .env("GITHUB_TOKEN", "test-token")
            .env("NO_COLOR", "1")
            .output()
            .unwrap()
    }

    fn gh_ok(&self, args: &[&str]) -> String {
        let out = self.gh(args);
        eprintln!(
            "$ jj-gh {args:?}\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "jj-gh {args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn change(&self, file: &str, message: &str) {
        std::fs::write(self.work().join(file), format!("{file}\n")).unwrap();
        self.jj(&["describe", "-m", message]);
        self.jj(&["new"]);
    }

    fn prs(&self) -> BTreeMap<u64, Pr> {
        self.mock.state.lock().unwrap().prs.clone()
    }
}

fn setup() -> Option<Env> {
    if !jj_available() {
        eprintln!("skipping: jj not installed");
        return None;
    }
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let bare = root.join("remote.git");
    let seed = root.join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    sh(
        root,
        "git",
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    sh(&seed, "git", &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("README"), "hello\n").unwrap();
    sh(&seed, "git", &["add", "."]);
    sh(&seed, "git", &["commit", "-q", "-m", "init"]);
    sh(
        &seed,
        "git",
        &["push", "-q", bare.to_str().unwrap(), "main"],
    );
    sh(
        root,
        "jj",
        &["git", "clone", bare.to_str().unwrap(), "work"],
    );
    let mock = start_mock(bare, root.to_path_buf());
    Some(Env { dir, mock })
}

#[test]
fn submit_merge_and_restack() {
    let Some(env) = setup() else { return };
    env.change("a.txt", "Add a\n\nThe first change.");
    env.change("b.txt", "Add b");

    // Dry run touches nothing.
    let plan = env.gh_ok(&["submit", "--dry-run"]);
    assert!(plan.contains("Add a") && plan.contains("Add b"), "{plan}");
    assert!(env.prs().is_empty());

    // Submit creates a stacked pair.
    env.gh_ok(&["submit"]);
    let prs = env.prs();
    assert_eq!(prs.len(), 2, "{prs:#?}");
    assert_eq!(prs[&1].base, "main");
    assert_eq!(
        prs[&2].base, prs[&1].head,
        "second PR must be based on the first"
    );
    assert!(prs[&1].body.starts_with("The first change."));
    for pr in prs.values() {
        assert!(!pr.body.contains("jj-gh:stack"), "{}", pr.body);
    }
    assert_eq!(env.mock.state.lock().unwrap().stacks, vec![vec![1, 2]]);

    // Status sees both, up to date, top of the stack first.
    let status = env.gh_ok(&["status"]);
    let (p2, p1) = (status.find("#2").unwrap(), status.find("#1").unwrap());
    assert!(p2 < p1, "{status}");
    assert!(!status.contains("not pushed"), "{status}");

    // Resubmitting is a no-op.
    let again = env.gh_ok(&["submit"]);
    assert_eq!(again.matches("up to date").count(), 2, "{again}");

    // Merge the bottom PR: it is squashed on the remote, then we restack.
    env.gh_ok(&["merge"]);
    assert_eq!(env.prs()[&1].state, "MERGED");
    let log = env.jj(&[
        "log",
        "--no-graph",
        "-r",
        "trunk()..@",
        "-T",
        "description.first_line() ++ \"\\n\"",
    ]);
    assert_eq!(log.trim(), "Add b", "stack after merge:\n{log}");
    let parent = env.jj(&[
        "log",
        "--no-graph",
        "-r",
        "@--",
        "-T",
        "description.first_line()",
    ]);
    assert!(
        parent.contains("Add a (#1)"),
        "restacked onto the squash commit: {parent}"
    );

    // GitHub has already retargeted the survivor at main; submit just pushes.
    assert_eq!(env.prs()[&2].base, "main");
    env.gh_ok(&["submit"]);
    assert_eq!(env.prs()[&2].base, "main");
}

#[test]
fn per_change_and_update_description() {
    let Some(env) = setup() else { return };
    env.change("a.txt", "Add a");
    env.change("b.txt", "Add b");
    env.gh_ok(&["submit", "--draft"]);
    assert_eq!(env.prs().len(), 2);

    env.jj(&["describe", "-r", "@--", "-m", "Add a, better"]);
    assert!(env.gh_ok(&["status"]).contains("description differs"));
    let out = env.gh_ok(&["submit"]);
    assert!(out.contains("description differs"), "{out}");
    assert_eq!(env.prs()[&1].title, "Add a");

    env.gh_ok(&["submit", "--update-description"]);
    assert_eq!(env.prs()[&1].title, "Add a, better");
    assert!(!env.gh_ok(&["status"]).contains("description differs"));
}

#[test]
fn rejects_nonlinear_and_undescribed() {
    let Some(env) = setup() else { return };
    env.jj(&["describe", "-m", "x"]);
    env.jj(&["new", "main", "-m", "y"]);
    env.jj(&["new", "description(exact:\"x\\n\")", "@", "-m", "merge"]);
    let out = env.gh(&["submit", "--dry-run"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("linear")
            || String::from_utf8_lossy(&out.stderr).contains("parents")
    );
}

#[test]
fn auth_reports_source() {
    let Some(env) = setup() else { return };
    let out = env.gh_ok(&["auth"]);
    assert!(
        out.contains("$GITHUB_TOKEN") && out.contains("tester"),
        "{out}"
    );
}

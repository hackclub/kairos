use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub enum Act {
    Write { path: &'static str, content: &'static str },
    /// `old` and `new` are whole lines that share none, so every harness counts the same lines.
    Edit { path: &'static str, old: &'static str, new: &'static str },
    Shell { cmd: &'static str },
}

pub struct Turn {
    pub prompt: &'static str,
    pub acts: Vec<Act>,
    pub say: &'static str,
    /// kairosd is stopped and started again before this turn, the way an update does it.
    pub restart: bool,
    /// The harness is closed and opened again on the same session before this turn.
    pub reopen: bool,
}

pub struct Scenario {
    pub name: &'static str,
    pub turns: Vec<Turn>,
}

/// What Kairos should report for one turn, worked out from the script alone.
#[derive(Debug, PartialEq)]
pub struct Want {
    pub prompt_words: usize,
    /// Kairos counts the agent as started at its first tool call.
    pub starts: usize,
    pub edits: Vec<(String, usize, usize)>,
    pub commits: usize,
    pub reply_words: usize,
    pub tools: usize,
    pub files: usize,
}

impl Turn {
    pub fn want(&self) -> Want {
        let mut edits = Vec::new();
        let mut commits = 0;
        for a in &self.acts {
            match a {
                Act::Write { path, content } => edits.push((path.to_string(), content.lines().count(), 0)),
                Act::Edit { path, old, new } => edits.push((path.to_string(), new.lines().count(), old.lines().count())),
                Act::Shell { cmd } => commits += usize::from(cmd.contains("git commit")),
            }
        }
        let mut files: Vec<&String> = edits.iter().map(|e| &e.0).collect();
        files.sort();
        files.dedup();
        Want {
            prompt_words: self.prompt.split_whitespace().count(),
            starts: usize::from(!self.acts.is_empty()),
            files: files.len(),
            edits,
            commits,
            reply_words: self.say.split_whitespace().count(),
            tools: self.acts.len(),
        }
    }
}

impl Scenario {
    /// The project's files after every turn ran as scripted.
    pub fn files(&self) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        for a in self.turns.iter().flat_map(|t| &t.acts) {
            match a {
                Act::Write { path, content } => {
                    files.insert(path.to_string(), content.to_string());
                }
                Act::Edit { path, old, new } => {
                    if let Some(f) = files.get_mut(*path) {
                        *f = f.replacen(old, new, 1);
                    }
                }
                Act::Shell { .. } => {}
            }
        }
        files
    }

    pub fn commits(&self) -> usize {
        self.turns.iter().map(|t| t.want().commits).sum()
    }
}

pub fn basic() -> Scenario {
    Scenario {
        name: "basic",
        turns: vec![
            Turn {
                prompt: "please write a small hello program in python, then commit it",
                acts: vec![
                    Act::Write { path: "hello.py", content: "def greet(name):\n    return \"hello \" + name\n\n\nprint(greet(\"world\"))\n" },
                    Act::Edit { path: "hello.py", old: "print(greet(\"world\"))", new: "print(greet(\"kairos\"))\nprint(greet(\"lab\"))" },
                    // Two plain commands, since Windows PowerShell has no `&&`.
                    Act::Shell { cmd: "git add -A" },
                    Act::Shell { cmd: "git commit -m \"add hello\"" },
                ],
                say: "Done. I wrote hello.py, changed the greeting and committed it.",
                restart: false,
                reopen: false,
            },
            Turn {
                prompt: "now greet one more name and leave it uncommitted",
                acts: vec![Act::Edit { path: "hello.py", old: "print(greet(\"lab\"))", new: "print(greet(\"lab!\"))\nprint(greet(\"again\"))" }],
                say: "Added one more greeting.",
                restart: false,
                reopen: false,
            },
            Turn { prompt: "what does hello.py print when it runs?", acts: vec![], say: "It prints four greetings, one on each line.", restart: false, reopen: false },
            Turn {
                prompt: "move the names into a small module in a folder called pkg",
                acts: vec![
                    Act::Write { path: "pkg/util.py", content: "NAMES = [\"kairos\", \"lab\"]\n" },
                    Act::Edit { path: "hello.py", old: "print(greet(\"again\"))", new: "print(greet(\"again!\"))\nprint(greet(\"util\"))" },
                ],
                say: "The names are in pkg/util.py now.",
                restart: false,
                reopen: false,
            },
            Turn {
                prompt: "commit that please",
                acts: vec![Act::Shell { cmd: "git add -A" }, Act::Shell { cmd: "git commit -m \"add util\"" }],
                say: "Committed.",
                restart: true,
                reopen: false,
            },
            Turn {
                prompt: "add one more small module next to it and leave it uncommitted",
                acts: vec![Act::Write { path: "pkg/more.py", content: "MORE = [\"again\"]\nLAST = \"util\"\n" }],
                say: "Added pkg/more.py.",
                restart: false,
                reopen: true,
            },
        ],
    }
}

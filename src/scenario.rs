use std::collections::BTreeMap;

#[derive(Clone, Copy)]
pub enum Act {
    Write { path: &'static str, content: &'static str },
    /// `old` and `new` are whole lines that share none, so every harness counts the same lines.
    Edit { path: &'static str, old: &'static str, new: &'static str },
    Shell { cmd: &'static str },
    /// A command outside the harness's allow list, so a harness that asks first (Claude Code) shows a permission prompt.
    Asked { cmd: &'static str },
}

#[derive(Default)]
pub struct Turn {
    /// Typed one key at a time.
    pub prompt: &'static str,
    /// Pasted after the typed part in one go, the way a terminal pastes.
    pub paste: &'static str,
    pub acts: Vec<Act>,
    pub say: &'static str,
    /// kairosd is stopped and started again before this turn, the way an update does it.
    pub restart: bool,
    /// The harness is closed and opened again on the same session before this turn.
    pub reopen: bool,
    /// The model stops answering after the acts and the person presses Esc.
    pub interrupt: bool,
}

pub struct Scenario {
    pub name: &'static str,
    pub turns: Vec<Turn>,
}

/// What Kairos should report for one turn, worked out from the script alone.
#[derive(Debug, PartialEq)]
pub struct Want {
    pub prompt_words: usize,
    pub pasted: bool,
    pub paste_words: usize,
    /// Permission prompts the person answered, for a harness that asks.
    pub asks: usize,
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
        let (mut commits, mut asks) = (0, 0);
        for a in &self.acts {
            match a {
                Act::Write { path, content } => edits.push((path.to_string(), content.lines().count(), 0)),
                Act::Edit { path, old, new } => edits.push((path.to_string(), new.lines().count(), old.lines().count())),
                Act::Shell { cmd } => commits += usize::from(cmd.contains("git commit")),
                Act::Asked { .. } => asks += 1,
            }
        }
        let mut files: Vec<&String> = edits.iter().map(|e| &e.0).collect();
        files.sort();
        files.dedup();
        let paste_words = self.paste.split_whitespace().count();
        Want {
            prompt_words: self.prompt.split_whitespace().count() + paste_words,
            // Kairos calls a prompt of more than 15 lines pasted, whoever typed it.
            pasted: self.paste.lines().count() > 15,
            paste_words,
            asks,
            starts: usize::from(!self.acts.is_empty()),
            files: files.len(),
            edits,
            commits,
            // An interrupted turn never gets its closing text.
            reply_words: if self.interrupt { 0 } else { self.say.split_whitespace().count() },
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
                Act::Shell { .. } | Act::Asked { .. } => {}
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
                ..Turn::default()
            },
            Turn {
                prompt: "now greet one more name and leave it uncommitted",
                acts: vec![Act::Edit { path: "hello.py", old: "print(greet(\"lab\"))", new: "print(greet(\"lab!\"))\nprint(greet(\"again\"))" }],
                say: "Added one more greeting.",
                ..Turn::default()
            },
            Turn { prompt: "what does hello.py print when it runs?", say: "It prints four greetings, one on each line.", ..Turn::default() },
            Turn {
                prompt: "move the names into a small module in a folder called pkg",
                acts: vec![
                    Act::Write { path: "pkg/util.py", content: "NAMES = [\"kairos\", \"lab\"]\n" },
                    Act::Edit { path: "hello.py", old: "print(greet(\"again\"))", new: "print(greet(\"again!\"))\nprint(greet(\"util\"))" },
                ],
                say: "The names are in pkg/util.py now.",
                ..Turn::default()
            },
            Turn {
                prompt: "commit that please",
                acts: vec![Act::Shell { cmd: "git add -A" }, Act::Shell { cmd: "git commit -m \"add util\"" }],
                say: "Committed.",
                restart: true,
                ..Turn::default()
            },
            Turn {
                prompt: "add one more small module next to it and leave it uncommitted",
                acts: vec![Act::Write { path: "pkg/more.py", content: "MORE = [\"again\"]\nLAST = \"util\"\n" }],
                say: "Added pkg/more.py.",
                reopen: true,
                ..Turn::default()
            },
            Turn {
                prompt: "this is what i get when i run it, can you keep it in a notes file? ",
                paste: LOG,
                acts: vec![Act::Write { path: "notes.txt", content: "the run printed four greetings\n" }],
                say: "I put a short note in notes.txt.",
                ..Turn::default()
            },
            Turn {
                prompt: "tag this version as lab-1",
                acts: vec![Act::Asked { cmd: "git tag lab-1" }],
                say: "Tagged it lab-1.",
                ..Turn::default()
            },
            // Claude Code's shell keeps the `cd` and its log follows it, the window stays on the folder it started in.
            Turn {
                prompt: "make an empty repo called inner in here and work from inside it",
                acts: vec![Act::Shell { cmd: "git init -q inner" }, Act::Shell { cmd: "cd inner" }],
                say: "Made inner and moved into it.",
                ..Turn::default()
            },
            Turn {
                prompt: "now say hi instead of hello everywhere",
                acts: vec![Act::Edit { path: "hello.py", old: "    return \"hello \" + name", new: "    return \"hi \" + name" }],
                say: "Changed hello to hi.",
                interrupt: true,
                ..Turn::default()
            },
            Turn { prompt: "ok stop there, that is enough for today", say: "Stopped. hello.py says hi now.", ..Turn::default() },
        ],
    }
}

/// What a person pastes from their terminal: 20 lines, more than Kairos takes for typed.
const LOG: &str = "$ python hello.py
hello kairos
hello lab!
hello again!
hello util
[run 1] started at 10:00:01
[run 1] loading names from pkg/util.py
[run 1] found 2 names
[run 1] greeting kairos
[run 1] greeting lab
[run 1] greeting again
[run 1] greeting util
[run 1] done in 0.02 s
[run 2] started at 10:00:05
[run 2] loading names from pkg/util.py
[run 2] found 2 names
[run 2] greeting kairos
[run 2] greeting lab
[run 2] done in 0.01 s
exit code 0
";

/// For an app: the first five turns, typed into its own window, with the kairosd restart.
pub fn app() -> Scenario {
    let mut s = basic();
    s.turns.truncate(5);
    Scenario { name: "app", turns: s.turns }
}

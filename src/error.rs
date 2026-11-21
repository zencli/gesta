//! Errors a program can branch on.
//!
//! Every message this tool prints is written for a person: it names the value, the rule and the
//! fix. That is what makes a failure readable and it is why the tests match on the text. A
//! program cannot branch on text, though, and an agent that retries the wrong thing is worse
//! than one that stops.
//!
//! So a failure carries a code as well. The code is stable and the message is not: wording
//! improves with every report, and a caller that matched on `no such branch` and then on `branch
//! not found` had to be rewritten for a change that helped people.
//!
//! ```text
//! $ gesta show main:missing.txt
//! gesta: no such file in main: missing.txt
//! $ gesta --json show main:missing.txt
//! {"error":{"code":"path-not-found","message":"no such file in main: missing.txt"}}
//! ```

use std::fmt;

/// The kinds of failure a caller can act on differently.
///
/// This is a short list on purpose. A code for every message would be one code per sentence, and
/// the point is to separate "ask for another branch" from "fix the credentials" from "the store
/// is damaged".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    /// A branch, tag or changeset was named and does not exist.
    NotFound,
    /// A path was named and is not in that tree.
    PathNotFound,
    /// The directory is not a repository, or has no repository above it.
    NotARepo,
    /// The working tree has changes that the command would overwrite.
    DirtyWorktree,
    /// A merge or rebase is unfinished and must be resolved first.
    Unmerged,
    /// Credentials were missing, wrong or refused.
    Auth,
    /// A remote could not be reached or answered badly.
    Remote,
    /// The command was understood and the arguments are wrong.
    Usage,
    /// The store or a remote object failed its own integrity check.
    Corrupt,
    /// Something else. Kept so a failure is still reported, never silent.
    Other,
}

impl Code {
    pub fn name(self) -> &'static str {
        match self {
            Code::NotFound => "not-found",
            Code::PathNotFound => "path-not-found",
            Code::NotARepo => "not-a-repository",
            Code::DirtyWorktree => "dirty-worktree",
            Code::Unmerged => "unmerged",
            Code::Auth => "auth",
            Code::Remote => "remote",
            Code::Usage => "usage",
            Code::Corrupt => "corrupt",
            Code::Other => "error",
        }
    }

    /// What a caller should do about it, short enough to print on one line.
    pub fn advice(self) -> &'static str {
        match self {
            Code::NotFound => "list what exists, then name one of those",
            Code::PathNotFound => "check the path against `gesta ls`, or the branch is older",
            Code::NotARepo => "run `gesta init` here, or move to a directory that has one",
            Code::DirtyWorktree => {
                "snapshot with `gesta changeset create`, or set it aside with `gesta stash save`"
            }
            Code::Unmerged => {
                "resolve the file, then `gesta rebase --continue` or `gesta merge --continue`"
            }
            Code::Auth => "check the token, or run `gesta secret list` to see which one is used",
            Code::Remote => "check the URL and the network, then try again",
            Code::Usage => "run the command with --help",
            Code::Corrupt => "run `gesta fsck` to see what is damaged before writing more",
            Code::Other => "read the message",
        }
    }
}

/// A failure with a code, a message for a person, and nothing else required.
#[derive(Debug)]
pub struct Error {
    pub code: Code,
    pub message: String,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(Code::NotFound, message)
    }

    /// One line of JSON, so an agent can read the code without parsing the sentence.
    ///
    /// The advice travels too. A caller that reads only the code knows which branch of its logic
    /// to take, and a caller that reads `advice` can print the next command without knowing the
    /// tool.
    pub fn json(&self) -> String {
        format!(
            "{{\"error\":{{\"code\":\"{}\",\"message\":\"{}\",\"advice\":\"{}\"}}}}",
            self.code.name(),
            escape(&self.message),
            escape(self.code.advice())
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The message alone. Every existing message is already a full sentence with the value and
        // the fix in it, and appending a code to text written for a person would make it longer
        // without making it clearer. The code travels in `json` and in `code`.
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Read the code out of any failure, so `main` can print the structured form without every call
/// site knowing about codes.
pub fn code_of(e: &(dyn std::error::Error + 'static)) -> Code {
    if let Some(ours) = e.downcast_ref::<Error>() {
        return ours.code;
    }
    Code::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_what_a_person_sees() {
        let e = Error::not_found("no such branch: nope; known branches: main");
        assert_eq!(e.to_string(), "no such branch: nope; known branches: main");
    }

    #[test]
    fn the_json_form_carries_the_code_and_the_message() {
        let e = Error::not_found("no such branch: nope");
        let json = e.json();
        assert!(json.contains("\"code\":\"not-found\""), "{}", json);
        assert!(
            json.contains("\"message\":\"no such branch: nope\""),
            "{}",
            json
        );
        assert!(
            json.contains("\"advice\":"),
            "an agent should get the next step: {}",
            json
        );
    }

    #[test]
    fn a_quote_in_a_message_does_not_break_the_json() {
        let e = Error::new(Code::Usage, "unknown flag \"--nope\"");
        let json = e.json();
        assert!(
            json.contains("\\\"--nope\\\""),
            "quote not escaped: {}",
            json
        );
        // The braces have to balance, or a parser fails in a way that hides the real message.
        assert_eq!(json.matches('{').count(), json.matches('}').count());
    }

    #[test]
    fn a_newline_in_a_message_stays_on_one_line() {
        let e = Error::new(Code::Other, "line one\nline two");
        assert!(!e.json().contains('\n'));
        assert!(e.json().contains("\\n"));
    }

    #[test]
    fn the_code_survives_the_box() {
        let boxed: Box<dyn std::error::Error> = Box::new(Error::new(Code::Auth, "no token"));
        assert_eq!(code_of(boxed.as_ref()), Code::Auth);
    }

    #[test]
    fn a_plain_message_has_no_code_of_its_own() {
        let boxed = crate::bad("something else");
        assert_eq!(code_of(boxed.as_ref()), Code::Other);
    }

    #[test]
    fn every_code_names_an_action() {
        for code in [
            Code::NotFound,
            Code::PathNotFound,
            Code::NotARepo,
            Code::DirtyWorktree,
            Code::Unmerged,
            Code::Auth,
            Code::Remote,
            Code::Usage,
            Code::Corrupt,
            Code::Other,
        ] {
            assert!(!code.advice().is_empty(), "{:?} has no advice", code);
            assert!(!code.name().is_empty());
            assert!(
                code.name()
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-'),
                "{:?} name is not a stable identifier: {}",
                code,
                code.name()
            );
        }
    }
}

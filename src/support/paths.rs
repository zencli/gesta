//! Whether a path that arrived from outside can name a file inside a repository.
//!
//! Three places take a path from data rather than from the code: a changeset entry with empty content
//! (the bytes are read from disk), a tree entry being checked out, and a bundle path being restored.
//! Each of them joined the value onto a root, and `Path::join` does not sanitise: an absolute value
//! discards the root and `..` climbs out of it. The three answers lived apart, and two of them did not
//! exist at all. One function, so the rule cannot differ by caller.
//!
//! The answer is deliberately about shape, not about the filesystem: it says whether the value could
//! name a path inside a tree, and says nothing about whether that file exists.

/// Could this value name a file inside a repository?
///
/// `src/main.rs` could. `/etc/passwd`, `../../etc/shadow`, `C:/Windows/win.ini` and `a\b` could not.
/// The empty string could not: it names the root, not a file in it.
/// The first `n` characters of `s`, cut only on a char boundary.
///
/// `&s[..n]` panics when byte `n` sits inside a multi-byte character, and three separate places were
/// doing exactly that on values that arrive from outside the program: an S3 key (#274), a signing key
/// name (#290) and a submodule pin (#313). Two of them panicked while formatting an error message,
/// which turns a bad input into a crash on the path whose job is to report the bad input.
///
/// Bytes are the wrong unit here anyway: the caller wants "a short prefix for a message", so the cut
/// belongs at a character. Keeping it in one function means the next caller that needs a prefix does
/// not write `&s[..16]` again.
pub fn head_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((byte, _)) => &s[..byte],
        None => s,
    }
}

pub fn is_repo_relative(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    // A leading separator or a drive letter makes it absolute on some platform.
    if path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return false;
    }
    // A backslash is a separator on Windows and an ordinary character elsewhere, so a path holding one
    // means different things on different machines. Refused rather than guessed.
    if path.contains('\\') {
        return false;
    }
    // No component may climb, and a component that is empty means the value is malformed rather than
    // merely unusual.
    for part in path.split('/') {
        if part == ".." {
            return false;
        }
    }
    true
}

/// Whether `name` is a ref name this program may write under `.git`.
///
/// Git's own rules, reduced to the ones that matter when the name is joined onto a directory and
/// written: it must be relative, it must not climb, and it must not carry a character that changes
/// what the path means or that git itself refuses.
///
/// `is_repo_relative` is not enough here because a ref name is joined under `.git`, not the working
/// tree, and because git refuses a handful of characters a file name allows.
pub fn is_git_ref_name(name: &str) -> bool {
    if !is_repo_relative(name) {
        return false;
    }
    if !name.starts_with("refs/") {
        // Every ref this program writes lives under `refs/`. A name outside it would be a file in
        // `.git` itself - `config`, `HEAD`, `packed-refs` - and overwriting those is a way to make a
        // repository stop working.
        return false;
    }
    if name.ends_with(".lock") || name.ends_with('/') || name.contains("//") {
        return false;
    }
    // Control characters, and the characters git refuses anywhere in a ref.
    if name.chars().any(|c| {
        c.is_control() || c == ' ' || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    // `..` inside a component is refused by git as well as `..` as a whole component.
    if name.contains("..") || name.contains("@{") {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_path_is_relative_and_does_not_climb() {
        for ok in [
            "a.txt",
            "src/main.rs",
            "a/b/c.rs",
            "./a.txt",
            "assets/x.bin",
        ] {
            assert!(is_repo_relative(ok), "{ok} must be accepted");
        }
        let backslash = "a\\b";
        let unc = "\\\\server\\share\\x";
        for bad in [
            "/etc/passwd",
            "../../etc/shadow",
            "a/../../b",
            unc,
            "C:/Windows/win.ini",
            "..",
            "",
            backslash,
        ] {
            assert!(
                !is_repo_relative(bad),
                "{bad:?} must be refused as a tree path"
            );
        }
    }

    #[test]
    fn head_chars_cuts_on_a_character_boundary() {
        // The whole reason this function exists (#274, #290, #313): `&s[..n]` panics when byte n sits
        // inside a multi-byte character, and every caller takes a string from outside the program.
        assert_eq!(head_chars("abcdef", 3), "abc");
        assert_eq!(
            head_chars("abc", 10),
            "abc",
            "a short string is returned whole"
        );
        assert_eq!(head_chars("", 4), "");

        // The pin shape from #313: 13 `a` then `é` is 15 BYTES and 14 CHARACTERS. Slicing at byte 14
        // split `é` and panicked. Counting characters, 14 characters is the whole string, so nothing
        // is cut and nothing can be split.
        let pin = format!("{}é", "a".repeat(13));
        assert_eq!(pin.len(), 15, "15 bytes");
        assert_eq!(pin.chars().count(), 14, "but 14 characters");
        assert_eq!(
            head_chars(&pin, 14),
            pin,
            "14 characters is the whole string"
        );
        assert_eq!(
            head_chars(&pin, 13),
            "a".repeat(13),
            "asking for 13 characters stops before the multi-byte one"
        );

        // Still multi-byte, cut after two characters rather than two bytes.
        assert_eq!(head_chars("éééé", 2), "éé");
        // A four-byte character counts as one.
        assert_eq!(head_chars("🙂🙂", 1), "🙂");
    }
    #[test]
    fn a_ref_name_must_stay_under_refs_and_avoid_gits_reserved_characters() {
        // #311: `write_ref` joined the name onto `.git` and wrote it, so `../../x` and `/etc/passwd`
        // both landed outside, and a name like `config` overwrote a file git needs.
        for ok in [
            "refs/heads/main",
            "refs/heads/feature-x",
            "refs/tags/v1.0",
            "refs/heads/a/b/c",
        ] {
            assert!(is_git_ref_name(ok), "{ok} must be accepted");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "refs/heads/../../outside",
            "refs/heads/a\\b",
            "config",
            "HEAD",
            "refs/heads/x.lock",
            "refs/heads/a b",
            "refs/heads/a~1",
            "refs/heads/a:b",
            "refs/heads/a..b",
            "refs/heads/a@{b",
            "refs/heads/",
            "refs//heads/main",
        ] {
            assert!(!is_git_ref_name(bad), "{bad:?} must be refused");
        }
        let control = "refs/heads/a\nb";
        assert!(
            !is_git_ref_name(control),
            "a newline in a ref name must be refused"
        );
    }
}

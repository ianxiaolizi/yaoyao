//! Command-line quoting under the Windows rules.
//!
//! `CreateProcessW` takes one string, not an argv array, and the child splits it
//! itself. The split is not "split on spaces": backslash runs before a quote are
//! consumed to decide whether the quote opens a quoted section. Quoting an
//! argument that already contains a quote — `powershell -Command "echo \"hi\""`
//! is the ordinary case here — is exactly where a naive quote-and-join produces
//! a command that means something else.
//!
//! The rules implemented are the ones `CommandLineToArgvW` and the C runtime
//! reverse. The test at the bottom parses the result back with
//! `CommandLineToArgvW`, so the two halves are checked against each other rather
//! than against a comment.

/// Quote one argument so the child parses it back as the same string.
pub fn quote_argument(argument: &str) -> String {
    let needs_quotes =
        argument.is_empty() || argument.chars().any(|c| matches!(c, ' ' | '\t' | '"'));
    if !needs_quotes {
        return argument.to_string();
    }

    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                // Even backslashes are literal; the odd one escapes the quote.
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(other);
            }
        }
    }
    // Before the closing quote every backslash must be doubled, or the trailing
    // run would escape the quote that is supposed to end the argument.
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

/// The full command line: the program followed by its arguments.
pub fn build(program: &str, args: &[String]) -> String {
    let mut line = quote_argument(program);
    for argument in args {
        line.push(' ');
        line.push_str(&quote_argument(argument));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Vec<String> {
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut count = 0i32;
        // SAFETY: `wide` is NUL-terminated and outlives the returned block; the
        // returned argument vector is owned by the caller and freed below by the
        // allocator the API documents (`LocalFree`).
        let argv = unsafe { CommandLineToArgvW(wide.as_ptr(), &mut count) };
        assert!(!argv.is_null(), "CommandLineToArgvW refused {line:?}");
        let mut arguments = Vec::new();
        for index in 0..count {
            // SAFETY: `argv` has `count` entries, each a NUL-terminated string.
            let entry = unsafe { *argv.add(index as usize) };
            let mut length = 0usize;
            // SAFETY: same as above; the scan stops at the NUL terminator.
            while unsafe { *entry.add(length) } != 0 {
                length += 1;
            }
            // SAFETY: the first `length` units are readable and are valid UTF-16
            // from a command line built out of `&str`s.
            let text = unsafe { std::slice::from_raw_parts(entry, length) };
            arguments.push(String::from_utf16_lossy(text));
        }
        // SAFETY: `argv` came from `CommandLineToArgvW`, whose contract is that
        // the block is released with `LocalFree`.
        unsafe { windows_sys::Win32::Foundation::LocalFree(argv as _) };
        arguments
    }

    #[test]
    fn plain_arguments_are_left_alone() {
        assert_eq!(quote_argument("hello"), "hello");
        assert_eq!(build("cmd", &["/C".into(), "echo hi".into()]), r#"cmd /C "echo hi""#);
    }

    #[test]
    fn empty_arguments_survive_the_round_trip() {
        let line = build("prog", &[String::new()]);
        assert_eq!(parse(&line), vec!["prog", ""]);
    }

    #[test]
    fn embedded_quotes_and_trailing_backslashes_round_trip() {
        let args = vec![
            "plain".to_string(),
            "with space".to_string(),
            r#"say "hi""#.to_string(),
            r"C:\path with spaces\".to_string(),
            r#"\a\"quote"#.to_string(),
            String::new(),
        ];
        let line = build("prog.exe", &args);
        let mut expected = vec!["prog.exe".to_string()];
        expected.extend(args);
        assert_eq!(parse(&line), expected, "line was {line:?}");
    }

    #[test]
    fn a_powershell_command_keeps_its_inner_quotes() {
        // The shape `shell.run` produces inside an AppContainer: one argument
        // after `-Command` that itself quotes.
        let command = r#"echo "hello world""#;
        let line = build(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            &["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(), command.into()],
        );
        assert_eq!(
            parse(&line),
            vec![
                r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ],
            "line was {line:?}"
        );
    }
}

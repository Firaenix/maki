//! Lays a shell command out one step per line for the expanded permission
//! prompt. It only adds a line break after `&&`, `||`, `|` and `;`, where the
//! shell reads the command the same either way, so what the user reviews is
//! still what runs.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// `(`, `$(`, `<(` and `((`: a nested command, laid out as written.
    Paren,
    DQuote,
    Backtick,
    /// `[[ ... ]]`, where `&&` and `||` are test operators.
    Test,
    /// `case ... esac`, where `|` separates patterns and `;;` ends an arm.
    Case,
}

const CONTINUATION_INDENT: &str = "  ";
/// A `;` right before one of these stays on its line: `if x; then` reads
/// worse broken in two.
const KEEP_AFTER_SEMI: &[&str] = &["then", "do"];

pub(crate) fn format_command(src: &str) -> String {
    let mut f = Formatter {
        chars: src.chars().collect(),
        i: 0,
        out: String::with_capacity(src.len() + src.len() / 8),
        stack: Vec::new(),
        heredocs: Vec::new(),
        line_indent: String::new(),
    };
    f.line_indent = f.indent_at(0);
    f.run();
    f.out
}

struct Formatter {
    chars: Vec<char>,
    i: usize,
    out: String,
    stack: Vec<Ctx>,
    /// Delimiters (and whether `<<-` strips tabs) whose bodies start after
    /// the next newline. No break goes in while one is pending, or the body
    /// would start on the wrong line.
    heredocs: Vec<(String, bool)>,
    line_indent: String,
}

impl Formatter {
    fn peek(&self, off: usize) -> Option<char> {
        self.chars.get(self.i + off).copied()
    }

    fn take(&mut self, n: usize) {
        for _ in 0..n {
            if let Some(c) = self.peek(0) {
                self.out.push(c);
                self.i += 1;
            }
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(k, c)| self.peek(k) == Some(c))
    }

    /// Whether {word} sits at the cursor as a whole word.
    fn word_at(&self, word: &str) -> bool {
        let n = word.chars().count();
        self.at_word_start()
            && self.starts_with(word)
            && self
                .peek(n)
                .is_none_or(|c| c.is_whitespace() || ";|&()".contains(c))
    }

    fn at_word_start(&self) -> bool {
        self.out
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || ";|&()<>".contains(c))
    }

    fn indent_at(&self, from: usize) -> String {
        self.chars[from.min(self.chars.len())..]
            .iter()
            .take_while(|c| **c == ' ' || **c == '\t')
            .collect()
    }

    fn run(&mut self) {
        while let Some(c) = self.peek(0) {
            match self.stack.last() {
                Some(Ctx::DQuote) => self.dquote(c),
                Some(Ctx::Backtick) => self.backtick(c),
                _ => self.code(c),
            }
        }
    }

    fn dquote(&mut self, c: char) {
        match c {
            '\\' => self.take(2),
            '"' => {
                self.stack.pop();
                self.take(1);
            }
            '$' if self.peek(1) == Some('(') => {
                self.stack.push(Ctx::Paren);
                self.take(2);
            }
            '`' => {
                self.stack.push(Ctx::Backtick);
                self.take(1);
            }
            _ => self.take(1),
        }
    }

    fn backtick(&mut self, c: char) {
        match c {
            '\\' => self.take(2),
            '`' => {
                self.stack.pop();
                self.take(1);
            }
            _ => self.take(1),
        }
    }

    fn code(&mut self, c: char) {
        let top_level = self.stack.is_empty() && self.heredocs.is_empty();
        match c {
            '\\' => self.take(2),
            '\'' => {
                self.take(1);
                while let Some(c) = self.peek(0) {
                    self.take(1);
                    if c == '\'' {
                        break;
                    }
                }
            }
            '"' => {
                self.stack.push(Ctx::DQuote);
                self.take(1);
            }
            '`' => {
                self.stack.push(Ctx::Backtick);
                self.take(1);
            }
            '(' => {
                self.stack.push(Ctx::Paren);
                self.take(1);
            }
            ')' => {
                if self.stack.last() == Some(&Ctx::Paren) {
                    self.stack.pop();
                }
                self.take(1);
            }
            '[' if self.peek(1) == Some('[') && self.at_word_start() => {
                self.stack.push(Ctx::Test);
                self.take(2);
            }
            ']' if self.peek(1) == Some(']') && self.stack.last() == Some(&Ctx::Test) => {
                self.stack.pop();
                self.take(2);
            }
            'c' if self.word_at("case") => {
                self.stack.push(Ctx::Case);
                self.take(4);
            }
            'e' if self.stack.last() == Some(&Ctx::Case) && self.word_at("esac") => {
                self.stack.pop();
                self.take(4);
            }
            '#' if self.at_word_start() => {
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.take(1);
                }
            }
            '<' if self.starts_with("<<<") => self.take(3),
            '<' if self.peek(1) == Some('<') => self.heredoc_start(),
            '\n' => {
                self.take(1);
                self.heredoc_bodies();
                self.line_indent = self.indent_at(self.i);
            }
            '&' if top_level && self.peek(1) == Some('&') => {
                self.take(2);
                self.break_line(true);
            }
            '|' if top_level && self.out.ends_with('>') => self.take(1),
            '|' if top_level && matches!(self.peek(1), Some('|' | '&')) => {
                self.take(2);
                self.break_line(true);
            }
            '|' if top_level => {
                self.take(1);
                self.break_line(true);
            }
            ';' if top_level && matches!(self.peek(1), Some(';' | '&')) => self.take(2),
            ';' if top_level => {
                self.take(1);
                self.break_line(false);
            }
            _ => self.take(1),
        }
    }

    /// Breaks after an operator unless the line already ends there.
    fn break_line(&mut self, continuation: bool) {
        let mut j = self.i;
        while matches!(self.chars.get(j), Some(' ' | '\t')) {
            j += 1;
        }
        match self.chars.get(j) {
            None | Some('\n' | '#') => {}
            Some(_) => {
                if !continuation {
                    let saved = self.i;
                    self.i = j;
                    let keep = KEEP_AFTER_SEMI.iter().any(|w| self.word_at(w));
                    self.i = saved;
                    if keep {
                        return;
                    }
                }
                self.i = j;
                self.out.push('\n');
                self.out.push_str(&self.line_indent);
                if continuation {
                    self.out.push_str(CONTINUATION_INDENT);
                }
            }
        }
    }

    fn heredoc_start(&mut self) {
        self.take(2);
        let strip = self.peek(0) == Some('-');
        if strip {
            self.take(1);
        }
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.take(1);
        }
        let mut delim = String::new();
        while let Some(c) = self.peek(0) {
            if c.is_whitespace() || ";|&<>()".contains(c) {
                break;
            }
            match c {
                '\'' | '"' => {
                    self.take(1);
                    while let Some(q) = self.peek(0) {
                        self.take(1);
                        if q == c {
                            break;
                        }
                        delim.push(q);
                    }
                }
                '\\' => {
                    self.take(1);
                    if let Some(n) = self.peek(0) {
                        delim.push(n);
                        self.take(1);
                    }
                }
                _ => {
                    delim.push(c);
                    self.take(1);
                }
            }
        }
        if !delim.is_empty() {
            self.heredocs.push((delim, strip));
        }
    }

    /// Copies each pending heredoc body through its closing delimiter as is.
    fn heredoc_bodies(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            while self.peek(0).is_some() {
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.take(1);
                }
                let line: String = self.chars[start..self.i].iter().collect();
                self.take(1);
                let line = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delim {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::format_command;

    #[test_case("cd foo && make test || echo fail", "cd foo &&\n  make test ||\n  echo fail" ; "and_or")]
    #[test_case("cat x | grep y |& wc -l", "cat x |\n  grep y |&\n  wc -l" ; "pipes")]
    #[test_case("a; b;c", "a;\nb;\nc" ; "semicolons_start_a_new_statement")]
    #[test_case("echo 'a && b' \"c | d; e\" && x", "echo 'a && b' \"c | d; e\" &&\n  x" ; "quotes")]
    #[test_case("echo a\\;b \\| c && d", "echo a\\;b \\| c &&\n  d" ; "escapes")]
    #[test_case(
        "(cd a && make) && echo \"$(git log | head -1 && echo \"ok;\")\" | tee out",
        "(cd a && make) &&\n  echo \"$(git log | head -1 && echo \"ok;\")\" |\n  tee out" ;
        "nested_subshells"
    )]
    #[test_case("x=`a | b` && y", "x=`a | b` &&\n  y" ; "backticks")]
    #[test_case(
        "cat <<'EOF' | grep x && echo done\na && b | c\nEOF\necho after; echo more",
        "cat <<'EOF' | grep x && echo done\na && b | c\nEOF\necho after;\necho more" ;
        "heredoc_body_and_its_line_stay_put"
    )]
    #[test_case(
        "cat <<-END\n\tx; y\n\tEND\nz && w",
        "cat <<-END\n\tx; y\n\tEND\nz &&\n  w" ;
        "dash_heredoc"
    )]
    #[test_case("tr a b <<< 'x | y' && z", "tr a b <<< 'x | y' &&\n  z" ; "herestring")]
    #[test_case(
        "set -e\nif [ -f x ]; then\n  rm x && touch y\nfi",
        "set -e\nif [ -f x ]; then\n  rm x &&\n    touch y\nfi" ;
        "already_multiline"
    )]
    #[test_case("a &&\n  b", "a &&\n  b" ; "operator_already_ends_the_line")]
    #[test_case("[[ -n $a && -z $b ]] && echo y", "[[ -n $a && -z $b ]] &&\n  echo y" ; "test_brackets")]
    #[test_case(
        "case $x in a|b) echo hi;; *) :;; esac && echo z",
        "case $x in a|b) echo hi;; *) :;; esac &&\n  echo z" ;
        "case_arms"
    )]
    #[test_case("make 2>&1 | tee log", "make 2>&1 |\n  tee log" ; "fd_redirect")]
    #[test_case("echo hi >| f && x", "echo hi >| f &&\n  x" ; "clobber_redirect")]
    #[test_case("a && b # x && y", "a &&\n  b # x && y" ; "comment")]
    #[test_case("echo a#b && c", "echo a#b &&\n  c" ; "hash_inside_word")]
    #[test_case("a && # why\nb", "a && # why\nb" ; "comment_after_operator")]
    #[test_case("for f in *; do echo $f; done", "for f in *; do echo $f;\ndone" ; "loop")]
    #[test_case("sleep 1 & wait", "sleep 1 & wait" ; "background")]
    #[test_case("echo 'unterminated && x", "echo 'unterminated && x" ; "unterminated_quote")]
    fn formats(src: &str, expected: &str) {
        assert_eq!(format_command(src), expected);
    }

    #[test_case("cd foo && make | tee log; echo done" ; "flat")]
    #[test_case("cat <<EOF\na;b\nEOF\nx || y" ; "heredoc")]
    fn formatting_is_idempotent(src: &str) {
        let once = format_command(src);
        assert_eq!(format_command(&once), once);
    }

    /// Breaks are the only change, so dropping whitespace gives the input back.
    #[test]
    fn only_whitespace_changes() {
        let src = "(a && b) | c; d || e && \"f | g\"";
        let strip = |s: &str| s.split_whitespace().collect::<String>();
        assert_eq!(strip(&format_command(src)), strip(src));
    }
}

// SPDX-License-Identifier: EUPL-1.2
//! LaTeX math to Typst math markup.
//!
//! The output is plain Typst math: single letters, numbers, Unicode symbols,
//! escaped punctuation, string literals for text, and calls of a fixed set of
//! Typst's math functions. Every atom is followed by a space, so letters never
//! join into identifiers and punctuation never into shorthands. Nothing the
//! user typed reaches Typst unescaped: there is no way to write code (`#`),
//! leave math mode (`$`) or name a function. [`check`] verifies that of every
//! result before it is compiled.

use std::fmt::Write as _;

/// The longest formula accepted, in bytes of LaTeX.
pub const MAX_SOURCE_BYTES: usize = 4096;
/// Groups, arguments, environments and `\left` … `\right` pairs open at once.
pub const MAX_DEPTH: usize = 32;
/// Cells of all matrices, cases and aligned rows in one formula.
pub const MAX_CELLS: usize = 400;
/// Typst markup made from one formula.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Why a formula cannot be converted, in words for a tooltip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn error<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error(message.into()))
}

/// Typst math markup for the LaTeX math `tex` (without its delimiters).
pub fn to_typst(tex: &str) -> Result<String, Error> {
    if tex.len() > MAX_SOURCE_BYTES {
        return error(format!("longer than {MAX_SOURCE_BYTES} bytes"));
    }
    let mut parser = Parser {
        text: tex,
        at: 0,
        out: String::with_capacity(tex.len() * 2),
        depth: 0,
        cells: 0,
    };
    match parser.list(Stop::Eof, Table::No)? {
        Ended::Eof => {}
        _ => return error("unexpected end"),
    }
    let out = trimmed(&parser.out).to_owned();
    check(&out)?;
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Token<'a> {
    /// A control sequence without its backslash: letters, or one other character.
    Command(&'a str),
    Open,
    Close,
    Sup,
    Sub,
    Align,
    Prime,
    Tilde,
    Char(char),
    End,
}

/// What ends a list of atoms.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    Eof,
    Brace,
    Bracket,
    Right,
    Env,
}

/// Inside a matrix or cases, `&` and `\\` end a cell or a row; elsewhere
/// they are alignment points and line breaks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    No,
    Yes,
}

#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Eof,
    Brace,
    Bracket,
    Right,
    End(String),
    Cell,
    Row,
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
    out: String,
    depth: usize,
    cells: usize,
}

impl<'a> Parser<'a> {
    /// Skip spaces and `%` comments, which mean nothing in math.
    fn skip_space(&mut self) {
        loop {
            let rest = &self.text[self.at..];
            let trimmed = rest.trim_start();
            self.at += rest.len() - trimmed.len();
            if trimmed.starts_with('%') {
                self.at += trimmed.find('\n').unwrap_or(trimmed.len());
            } else {
                return;
            }
        }
    }

    /// The next token and its length, after spaces.
    fn peek(&mut self) -> Result<(Token<'a>, usize), Error> {
        self.skip_space();
        let text: &'a str = self.text;
        let rest = &text[self.at..];
        let Some(c) = rest.chars().next() else {
            return Ok((Token::End, 0));
        };
        let token = match c {
            '\\' => {
                let after = &rest[1..];
                let letters = after
                    .find(|c: char| !c.is_ascii_alphabetic())
                    .unwrap_or(after.len());
                let name = if letters > 0 {
                    &after[..letters]
                } else {
                    let Some(other) = after.chars().next() else {
                        return error("a backslash at the end");
                    };
                    &after[..other.len_utf8()]
                };
                return Ok((Token::Command(name), 1 + name.len()));
            }
            '{' => Token::Open,
            '}' => Token::Close,
            '^' => Token::Sup,
            '_' => Token::Sub,
            '&' => Token::Align,
            '\'' => Token::Prime,
            '~' => Token::Tilde,
            other => Token::Char(other),
        };
        Ok((token, c.len_utf8()))
    }

    fn next(&mut self) -> Result<Token<'a>, Error> {
        let (token, length) = self.peek()?;
        self.at += length;
        Ok(token)
    }

    /// One atom of output, followed by a space.
    fn emit(&mut self, atom: &str) -> Result<(), Error> {
        self.out.push_str(atom);
        self.out.push(' ');
        self.limit()
    }

    fn limit(&self) -> Result<(), Error> {
        if self.out.len() > MAX_OUTPUT_BYTES {
            return error("too long");
        }
        Ok(())
    }

    fn trim(&mut self) {
        let length = trimmed(&self.out).len();
        self.out.truncate(length);
    }

    fn enter(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return error("nested too deeply");
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Run `f` with its output collected separately.
    fn capture(&mut self, f: impl FnOnce(&mut Self) -> Result<(), Error>) -> Result<String, Error> {
        let outer = std::mem::take(&mut self.out);
        let result = f(self);
        let inner = std::mem::replace(&mut self.out, outer);
        result?;
        Ok(trimmed(&inner).to_owned())
    }

    /// Atoms with their attachments until `stop`.
    fn list(&mut self, stop: Stop, table: Table) -> Result<Ended, Error> {
        loop {
            let (token, length) = self.peek()?;
            match token {
                Token::End => {
                    return match stop {
                        Stop::Eof => Ok(Ended::Eof),
                        Stop::Brace => error("a group is not closed"),
                        Stop::Bracket => error("an optional argument is not closed"),
                        Stop::Right => error("\\left without \\right"),
                        Stop::Env => error("an environment is not ended"),
                    };
                }
                Token::Close if stop == Stop::Brace => {
                    self.at += length;
                    return Ok(Ended::Brace);
                }
                Token::Close => return error("an unopened }"),
                Token::Char(']') if stop == Stop::Bracket => {
                    self.at += length;
                    return Ok(Ended::Bracket);
                }
                Token::Command("right") if stop == Stop::Right => {
                    self.at += length;
                    return Ok(Ended::Right);
                }
                Token::Command("right") => return error("\\right without \\left"),
                Token::Command("end") if stop == Stop::Env => {
                    self.at += length;
                    let name = self.raw_group()?;
                    return Ok(Ended::End(name.to_owned()));
                }
                Token::Command("end") => return error("\\end without \\begin"),
                Token::Align if table == Table::Yes => {
                    self.at += length;
                    return Ok(Ended::Cell);
                }
                Token::Command("\\") | Token::Command("cr") if table == Table::Yes => {
                    self.at += length;
                    self.skip_optional()?;
                    return Ok(Ended::Row);
                }
                Token::Command(name) if switch(name).is_some() => {
                    self.at += length;
                    if name == "color" {
                        self.raw_group()?;
                    }
                    let (before, after) = switch(name).unwrap_or_default();
                    self.enter()?;
                    let start = self.out.len();
                    let ended = self.list(stop, table)?;
                    self.leave();
                    if !before.is_empty() && self.out.len() > start {
                        self.out.insert_str(start, before);
                        self.trim();
                        self.out.push_str(after);
                        self.out.push(' ');
                    }
                    return Ok(ended);
                }
                Token::Sup | Token::Sub | Token::Prime => {
                    // Nothing to attach to, as in `{}^{14}C`.
                    let start = self.out.len();
                    self.emit("\"\"")?;
                    self.attachments(start)?;
                }
                Token::Char(c) if c.is_ascii_digit() => {
                    let start = self.out.len();
                    self.number()?;
                    self.attachments(start)?;
                }
                _ => {
                    let start = self.out.len();
                    self.atom()?;
                    if self.out.len() == start {
                        let (next, _) = self.peek()?;
                        if !matches!(next, Token::Sup | Token::Sub | Token::Prime) {
                            continue;
                        }
                        self.emit("\"\"")?;
                    }
                    self.attachments(start)?;
                }
            }
        }
    }

    /// Digits and a decimal point as one number, except that the digit
    /// before an attachment carries it on its own, as in TeX.
    fn number(&mut self) -> Result<(), Error> {
        let rest = &self.text[self.at..];
        let mut length = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let digits = rest[..length].trim_end_matches('.');
        length = digits.len();
        self.at += length;
        let (next, _) = self.peek()?;
        let attached = matches!(next, Token::Sup | Token::Sub | Token::Prime);
        if attached && digits.len() > 1 {
            let (head, last) = digits.split_at(digits.len() - 1);
            let head = head.trim_end_matches('.');
            if !head.is_empty() {
                self.emit(head)?;
            }
            if digits[..digits.len() - 1].ends_with('.') {
                self.emit(".")?;
            }
            self.emit(last)
        } else {
            self.emit(digits)
        }
    }

    /// Superscripts, subscripts, primes and `\limits` after an atom that
    /// starts at `start` in the output.
    fn attachments(&mut self, start: usize) -> Result<(), Error> {
        let (mut sup, mut sub) = (false, false);
        loop {
            let (token, length) = self.peek()?;
            match token {
                Token::Prime => {
                    self.at += length;
                    self.trim();
                    self.out.push_str("' ");
                }
                Token::Sup | Token::Sub => {
                    self.at += length;
                    let seen = if token == Token::Sup {
                        &mut sup
                    } else {
                        &mut sub
                    };
                    if std::mem::replace(seen, true) {
                        return error("a double superscript or subscript");
                    }
                    self.trim();
                    self.out
                        .push_str(if token == Token::Sup { "^(" } else { "_(" });
                    self.argument()?;
                    self.trim();
                    self.out.push_str(") ");
                }
                Token::Command(name @ ("limits" | "nolimits")) => {
                    self.at += length;
                    let function = if name == "limits" {
                        "limits("
                    } else {
                        "scripts("
                    };
                    self.trim();
                    self.out.insert_str(start, function);
                    self.out.push_str(") ");
                }
                _ => return self.limit(),
            }
        }
    }

    /// A command's argument or an attachment: a group, or one token (with
    /// the arguments of a command).
    fn argument(&mut self) -> Result<(), Error> {
        self.enter()?;
        let (token, length) = self.peek()?;
        let result = match token {
            Token::Open => {
                self.at += length;
                match self.list(Stop::Brace, Table::No)? {
                    Ended::Brace => Ok(()),
                    _ => error("a group is not closed"),
                }
            }
            Token::End | Token::Close | Token::Sup | Token::Sub | Token::Align => {
                error("an argument is missing")
            }
            Token::Char(c) if c.is_ascii_digit() => {
                self.at += length;
                self.emit(&c.to_string())
            }
            _ => self.atom(),
        };
        self.leave();
        result
    }

    /// An argument collected on its own.
    fn captured_argument(&mut self) -> Result<String, Error> {
        let argument = self.capture(Self::argument)?;
        Ok(if argument.is_empty() {
            "\"\"".into()
        } else {
            argument
        })
    }

    /// A group's text as written (`{…}`, or a single token), for names,
    /// colours and text.
    fn raw_group(&mut self) -> Result<&'a str, Error> {
        let text: &'a str = self.text;
        let (token, length) = self.peek()?;
        match token {
            Token::Open => {
                let start = self.at + length;
                let mut depth = 0_usize;
                let mut escaped = false;
                for (index, c) in text[start..].char_indices() {
                    match c {
                        _ if escaped => escaped = false,
                        '\\' => escaped = true,
                        '{' => {
                            depth += 1;
                            if depth > MAX_DEPTH {
                                return error("nested too deeply");
                            }
                        }
                        '}' if depth == 0 => {
                            self.at = start + index + 1;
                            return Ok(&text[start..start + index]);
                        }
                        '}' => depth -= 1,
                        _ => {}
                    }
                }
                error("a group is not closed")
            }
            Token::End | Token::Close => error("an argument is missing"),
            _ => {
                let start = self.at;
                self.at += length;
                Ok(&text[start..self.at])
            }
        }
    }

    /// Skip an optional `[…]` argument (`\\[2pt]`, column alignment).
    fn skip_optional(&mut self) -> Result<(), Error> {
        let rest = &self.text[self.at..];
        let trimmed = rest.trim_start();
        if let Some(inner) = trimmed.strip_prefix('[') {
            let Some(end) = inner.find(']') else {
                return error("an optional argument is not closed");
            };
            self.at += rest.len() - trimmed.len() + 1 + end + 1;
        }
        Ok(())
    }

    /// An optional `[…]` argument in math, if there is one.
    fn optional_math(&mut self) -> Result<Option<String>, Error> {
        if self.peek()?.0 != Token::Char('[') {
            return Ok(None);
        }
        self.next()?;
        self.enter()?;
        let inner = self.capture(|parser| match parser.list(Stop::Bracket, Table::No)? {
            Ended::Bracket => Ok(()),
            _ => error("an optional argument is not closed"),
        })?;
        self.leave();
        Ok(Some(inner))
    }

    /// One atom: a character, a group or a command with its arguments.
    fn atom(&mut self) -> Result<(), Error> {
        match self.next()? {
            Token::Open => {
                self.enter()?;
                let inner =
                    self.capture(|parser| match parser.list(Stop::Brace, Table::No)? {
                        Ended::Brace => Ok(()),
                        _ => error("a group is not closed"),
                    })?;
                self.leave();
                if inner.is_empty() {
                    self.emit("\"\"")
                } else {
                    // A TeX group is one ordinary atom.
                    self.emit(&format!("class(\"normal\", {inner})"))
                }
            }
            Token::Char(c) => self.character(c),
            Token::Tilde => self.emit("space.nobreak"),
            Token::Command(name) => self.command(name),
            Token::Close | Token::End => error("unexpected end"),
            Token::Sup | Token::Sub | Token::Prime => error("a misplaced ^ or _"),
            Token::Align => self.emit("&"),
        }
    }

    fn character(&mut self, c: char) -> Result<(), Error> {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => self.emit(c.encode_utf8(&mut [0; 4])),
            '+' | '-' | '=' | '<' | '>' | '*' | '!' | '?' | '.' | ':' => {
                self.emit(c.encode_utf8(&mut [0; 4]))
            }
            '|' | '/' | ',' | ';' | '(' | ')' | '[' | ']' | '"' | '@' | '`' | '#' | '$' => {
                self.emit(&format!("\\{c}"))
            }
            c if c.is_control() || c.is_whitespace() => Ok(()),
            c if c.is_ascii() => self.emit(&format!("\\{c}")),
            c => self.emit(c.encode_utf8(&mut [0; 4])),
        }
    }

    fn command(&mut self, name: &'a str) -> Result<(), Error> {
        if let Some(symbol) = symbol(name) {
            return self.emit(symbol);
        }
        if let Some((text, limits)) = operator(name) {
            return self.emit(&if limits {
                format!("limits(op(\"{text}\"))")
            } else {
                format!("op(\"{text}\")")
            });
        }
        if let Some(function) = accent(name) {
            let argument = self.captured_argument()?;
            return self.emit(&format!("{function}({argument})"));
        }
        if let Some(function) = font(name) {
            let argument = self.captured_argument()?;
            return self.emit(&function.replace('@', &argument));
        }
        if let Some(function) = text_font(name) {
            let text = self.raw_group()?;
            let text = self.text_mode(text)?;
            return self.emit(&function.replace('@', &text));
        }
        match name {
            "{" | "lbrace" => self.emit("\\{"),
            "}" | "rbrace" => self.emit("\\}"),
            "_" => self.emit("\\_"),
            "&" => self.emit("\\&"),
            "#" => self.emit("\\#"),
            "$" => self.emit("\\$"),
            "%" => self.emit("%"),
            "|" => self.emit("‖"),
            "\\" | "cr" | "newline" => {
                self.skip_optional()?;
                self.emit("\\")
            }
            "," | "thinspace" => self.emit("thin"),
            ":" | ">" | "medspace" => self.emit("med"),
            ";" | "thickspace" => self.emit("thick"),
            "!" | "negthinspace" | "negmedspace" | "negthickspace" => Ok(()),
            " " => self.emit("space"),
            "quad" => self.emit("quad"),
            "qquad" => self.emit("wide"),
            "enspace" => self.emit("space.en"),
            "hspace" | "hskip" | "kern" | "mkern" | "mskip" | "mspace" | "hphantom" => {
                if name == "hspace" && self.peek()?.0 == Token::Char('*') {
                    self.next()?;
                }
                self.raw_group()?;
                self.emit("med")
            }
            "frac" | "dfrac" | "tfrac" | "cfrac" | "binom" | "dbinom" | "tbinom" => {
                let numerator = self.captured_argument()?;
                let denominator = self.captured_argument()?;
                let function = if name.ends_with("binom") {
                    "binom"
                } else {
                    "frac"
                };
                let call = format!("{function}({numerator}, {denominator})");
                self.emit(&match name.as_bytes()[0] {
                    b'd' | b'c' => format!("display({call})"),
                    b't' => format!("inline({call})"),
                    _ => call,
                })
            }
            "sqrt" => {
                let index = self.optional_math()?;
                let radicand = self.captured_argument()?;
                self.emit(&match index {
                    Some(index) if !index.is_empty() => format!("root({index}, {radicand})"),
                    _ => format!("sqrt({radicand})"),
                })
            }
            "operatorname" | "operatornamewithlimits" | "DeclareMathOperator" => {
                let mut limits = name == "operatornamewithlimits";
                if self.peek()?.0 == Token::Char('*') {
                    self.next()?;
                    limits = true;
                }
                let raw = self.raw_group()?;
                let text = plain_text(raw);
                let call = format!("op({})", string_literal(text.trim()));
                self.emit(&if limits {
                    format!("limits({call})")
                } else {
                    call
                })
            }
            "mathop" | "mathbin" | "mathrel" | "mathord" | "mathopen" | "mathclose"
            | "mathpunct" | "mathinner" => {
                let class = match name {
                    "mathop" => "large",
                    "mathbin" => "binary",
                    "mathrel" => "relation",
                    "mathopen" => "opening",
                    "mathclose" => "closing",
                    "mathpunct" => "punctuation",
                    _ => "normal",
                };
                let argument = self.captured_argument()?;
                self.emit(&format!("class(\"{class}\", {argument})"))
            }
            "left" => self.left_right(),
            "middle" => {
                let delimiter = self.delimiter()?;
                if delimiter.is_empty() {
                    Ok(())
                } else {
                    self.emit(&format!("mid({delimiter})"))
                }
            }
            "big" | "Big" | "bigg" | "Bigg" | "bigl" | "Bigl" | "biggl" | "Biggl" | "bigr"
            | "Bigr" | "biggr" | "Biggr" | "bigm" | "Bigm" | "biggm" | "Biggm" => {
                // Typst sizes delimiters only through code; they keep their size.
                let delimiter = self.delimiter()?;
                if delimiter.is_empty() {
                    Ok(())
                } else {
                    self.emit(&delimiter)
                }
            }
            "begin" => self.environment(),
            "not" => self.negation(),
            "overset" | "stackrel" | "underset" => {
                let annotation = self.captured_argument()?;
                let base = self.captured_argument()?;
                let attach = if name == "underset" { '_' } else { '^' };
                self.emit(&format!("limits({base}){attach}({annotation})"))
            }
            "overbrace" | "underbrace" | "overbracket" | "underbracket" | "overparen"
            | "underparen" => {
                let body = self.captured_argument()?;
                let attach = if name.starts_with("over") {
                    Token::Sup
                } else {
                    Token::Sub
                };
                if self.peek()?.0 == attach {
                    self.next()?;
                    let annotation = self.captured_argument()?;
                    self.emit(&format!("{name}({body}, {annotation})"))
                } else {
                    self.emit(&format!("{name}({body})"))
                }
            }
            "xrightarrow" | "xleftarrow" | "xRightarrow" | "xLeftarrow" | "xleftrightarrow"
            | "xLeftrightarrow" | "xmapsto" | "xhookrightarrow" | "xhookleftarrow"
            | "xtwoheadrightarrow" | "xtwoheadleftarrow" | "xlongequal" => {
                let arrow = match name {
                    "xrightarrow" => "→",
                    "xleftarrow" => "←",
                    "xRightarrow" => "⇒",
                    "xLeftarrow" => "⇐",
                    "xleftrightarrow" => "↔",
                    "xLeftrightarrow" => "⇔",
                    "xmapsto" => "↦",
                    "xhookrightarrow" => "↪",
                    "xhookleftarrow" => "↩",
                    "xtwoheadrightarrow" => "↠",
                    "xtwoheadleftarrow" => "↞",
                    _ => "=",
                };
                let below = self.optional_math()?.filter(|below| !below.is_empty());
                let above = self.captured_argument()?;
                let mut call = format!("stretch({arrow})^({above})");
                if let Some(below) = below {
                    let _ = write!(call, "_({below})");
                }
                self.emit(&call)
            }
            "pmod" | "pod" => {
                let argument = self.captured_argument()?;
                let operator = if name == "pmod" {
                    "op(\"mod\") thin "
                } else {
                    ""
                };
                self.emit(&format!("quad \\( {operator}{argument} \\)"))
            }
            "bmod" => self.emit("op(\"mod\")"),
            "mod" => self.emit("quad op(\"mod\") thin"),
            "textcolor" | "colorbox" => {
                self.raw_group()?;
                if name == "colorbox" {
                    let text = self.raw_group()?;
                    let text = self.text_mode(text)?;
                    self.emit(&text)
                } else {
                    let argument = self.captured_argument()?;
                    self.emit(&format!("class(\"normal\", {argument})"))
                }
            }
            "boxed" | "smash" => {
                let argument = self.captured_argument()?;
                self.emit(&format!("class(\"normal\", {argument})"))
            }
            "fbox" | "framebox" => {
                let text = self.raw_group()?;
                let text = self.text_mode(text)?;
                self.emit(&text)
            }
            // Typst hides content only through code: a phantom takes no room.
            "phantom" | "vphantom" => self.capture(Self::argument).map(drop),
            "label" | "tag" | "eqref" | "ref" => {
                if self.peek()?.0 == Token::Char('*') {
                    self.next()?;
                }
                self.raw_group().map(drop)
            }
            "substack" => {
                let argument = self.captured_argument()?;
                self.emit(&argument)
            }
            "nonumber" | "notag" | "limits" | "nolimits" | "mathstrut" | "strut" | "relax"
            | "nobreak" | "allowbreak" | "displaybreak" | "protect" | "nolinebreak" => Ok(()),
            "right" | "end" => error(format!("\\{name} without its opening")),
            _ => error(format!("unknown command \\{name}")),
        }
    }

    /// `\left` delimiter, the list, `\right` delimiter.
    fn left_right(&mut self) -> Result<(), Error> {
        let left = self.delimiter()?;
        self.enter()?;
        let body = self.capture(|parser| match parser.list(Stop::Right, Table::No)? {
            Ended::Right => Ok(()),
            _ => error("\\left without \\right"),
        })?;
        self.leave();
        let right = self.delimiter()?;
        let mut call = String::from("lr(");
        for part in [left.as_str(), body.as_str(), right.as_str()] {
            if !part.is_empty() {
                call.push_str(part);
                call.push(' ');
            }
        }
        if call.len() == 3 {
            return self.emit("\"\"");
        }
        call.truncate(trimmed(&call).len());
        call.push(')');
        self.emit(&call)
    }

    /// The delimiter after `\left`, `\right`, `\middle` or `\big`; empty for `.`.
    fn delimiter(&mut self) -> Result<String, Error> {
        let token = self.next()?;
        let delimiter = match token {
            Token::Char('.') => "",
            Token::Char('(') => "\\(",
            Token::Char(')') => "\\)",
            Token::Char('[') => "\\[",
            Token::Char(']') => "\\]",
            Token::Char('|') => "|",
            Token::Char('/') => "\\/",
            Token::Char('<') => "⟨",
            Token::Char('>') => "⟩",
            Token::Command(name) => match name {
                "{" | "lbrace" => "\\{",
                "}" | "rbrace" => "\\}",
                "lbrack" => "\\[",
                "rbrack" => "\\]",
                "|" | "Vert" | "lVert" | "rVert" => "‖",
                "vert" | "lvert" | "rvert" => "|",
                "langle" => "⟨",
                "rangle" => "⟩",
                "lceil" => "⌈",
                "rceil" => "⌉",
                "lfloor" => "⌊",
                "rfloor" => "⌋",
                "lgroup" => "⟮",
                "rgroup" => "⟯",
                "llbracket" => "⟦",
                "rrbracket" => "⟧",
                "lmoustache" => "⎰",
                "rmoustache" => "⎱",
                "uparrow" => "↑",
                "downarrow" => "↓",
                "updownarrow" => "↕",
                "Uparrow" => "⇑",
                "Downarrow" => "⇓",
                "Updownarrow" => "⇕",
                "backslash" => "\\\\",
                "ulcorner" => "⌜",
                "urcorner" => "⌝",
                "llcorner" => "⌞",
                "lrcorner" => "⌟",
                _ => return error(format!("\\{name} is not a delimiter")),
            },
            _ => return error("a delimiter is missing"),
        };
        Ok(delimiter.to_owned())
    }

    /// `\not` before a relation: its negated form, or a slash through it.
    fn negation(&mut self) -> Result<(), Error> {
        let start = self.out.len();
        let (token, _) = self.peek()?;
        let negated = match token {
            Token::Char('=') => Some("≠"),
            Token::Char('<') => Some("≮"),
            Token::Char('>') => Some("≯"),
            Token::Command(name) => match name {
                "in" => Some("∉"),
                "ni" => Some("∌"),
                "equiv" => Some("≢"),
                "subset" => Some("⊄"),
                "supset" => Some("⊅"),
                "subseteq" => Some("⊈"),
                "supseteq" => Some("⊉"),
                "le" | "leq" => Some("≰"),
                "ge" | "geq" => Some("≱"),
                "sim" => Some("≁"),
                "simeq" => Some("≄"),
                "approx" => Some("≉"),
                "cong" => Some("≇"),
                "exists" => Some("∄"),
                "mid" => Some("∤"),
                "parallel" => Some("∦"),
                "prec" => Some("⊀"),
                "succ" => Some("⊁"),
                "vdash" => Some("⊬"),
                "models" => Some("⊭"),
                _ => None,
            },
            _ => None,
        };
        if let Some(negated) = negated {
            self.next()?;
            return self.emit(negated);
        }
        self.atom()?;
        self.trim();
        // U+0338 COMBINING LONG SOLIDUS OVERLAY on what was just written:
        // after a symbol or an escaped character. After a letter it would
        // join it into an identifier, so a negated letter is text; anything
        // else (a name like `space`, a call) stays as it is.
        let atom = &self.out[start..];
        let mut chars = atom.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) if c.is_alphanumeric() => {
                let negated = string_literal(&format!("{c}\u{338}"));
                self.out.truncate(start);
                self.out.push_str(&negated);
            }
            _ if atom.ends_with(|c: char| c.is_alphanumeric() || c == ')') => {}
            _ if !atom.is_empty() => self.out.push('\u{338}'),
            _ => {}
        }
        self.out.push(' ');
        Ok(())
    }

    /// `\begin{name}` … `\end{name}`.
    fn environment(&mut self) -> Result<(), Error> {
        let name = self.raw_group()?.trim();
        self.enter()?;
        let kind = match name {
            "matrix" | "smallmatrix" | "array" | "subarray" => Kind::Matrix("#none"),
            "pmatrix" | "pmatrix*" => Kind::Matrix("\"(\""),
            "bmatrix" | "bmatrix*" => Kind::Matrix("\"[\""),
            "Bmatrix" | "Bmatrix*" => Kind::Matrix("\"{\""),
            "vmatrix" | "vmatrix*" => Kind::Matrix("\"|\""),
            "Vmatrix" | "Vmatrix*" => Kind::Matrix("\"‖\""),
            "cases" | "cases*" | "dcases" | "dcases*" | "rcases" | "rcases*" => Kind::Cases,
            "aligned" | "align" | "align*" | "alignat" | "alignat*" | "alignedat" | "gathered"
            | "gather" | "gather*" | "split" | "multline" | "multline*" | "eqnarray"
            | "eqnarray*" | "flalign" | "flalign*" | "equation" | "equation*" => Kind::Lines,
            _ => return error(format!("unknown environment {name}")),
        };
        match name {
            "array" | "subarray" | "alignat" | "alignat*" | "alignedat" => {
                self.raw_group()?;
            }
            _ if name.ends_with("matrix*") => self.skip_optional()?,
            _ => {}
        }
        let body = self.capture(|parser| parser.rows(name, kind))?;
        self.leave();
        self.emit(&body)
    }

    /// The rows of an environment, up to its `\end`.
    fn rows(&mut self, name: &str, kind: Kind) -> Result<(), Error> {
        match kind {
            Kind::Matrix(delimiter) => {
                let _ = write!(self.out, "mat(delim: {delimiter}, ");
            }
            Kind::Cases => self.out.push_str("cases("),
            Kind::Lines => {}
        }
        let mut row_start = self.out.len();
        loop {
            self.cells += 1;
            if self.cells > MAX_CELLS {
                return error(format!("more than {MAX_CELLS} cells"));
            }
            // LaTeX sets both columns of cases left-aligned, a quad apart;
            // Typst aligns at every other point to the right.
            if kind == Kind::Cases && self.out.len() == row_start {
                self.out.push_str("& ");
                row_start = self.out.len();
            }
            let cell_start = self.out.len();
            let ended = self.list(Stop::Env, Table::Yes)?;
            let empty = self.out[cell_start..].trim().is_empty();
            if empty {
                self.out.truncate(cell_start);
            } else {
                self.trim();
            }
            let separator = match kind {
                Kind::Matrix(_) => ";",
                Kind::Cases => ",",
                Kind::Lines => "\\",
            };
            // A final `\\` makes no row; other empty cells hold an empty text.
            let last = matches!(&ended, Ended::End(_)) && empty && cell_start == row_start;
            if empty && !last && matches!(kind, Kind::Matrix(_)) {
                self.out.push_str("\"\"");
            }
            match ended {
                Ended::Cell => {
                    self.trim();
                    let separator = match kind {
                        Kind::Matrix(_) => ", ",
                        Kind::Cases => " && quad ",
                        Kind::Lines => " & ",
                    };
                    // After a line break's space, no second one.
                    if self.out.ends_with(' ') {
                        self.out.push_str(separator.trim_start());
                    } else {
                        self.out.push_str(separator);
                    }
                }
                Ended::Row => {
                    self.trim();
                    if kind == Kind::Lines {
                        self.out.push(' ');
                    }
                    self.out.push_str(separator);
                    self.out.push(' ');
                    row_start = self.out.len();
                }
                Ended::End(end) if end.trim() == name => {
                    if last {
                        let kept = self.out[..row_start].trim_end();
                        let kept = kept.strip_suffix(separator).unwrap_or(kept).trim_end();
                        self.out.truncate(kept.len());
                    }
                    if kind != Kind::Lines {
                        self.out.push(')');
                    }
                    return self.limit();
                }
                Ended::End(end) => {
                    return error(format!("\\begin{{{name}}} ended by \\end{{{end}}}"));
                }
                _ => return error(format!("\\begin{{{name}}} is not ended")),
            }
            self.limit()?;
        }
    }

    /// Text-mode content as Typst strings, with `$…$` inside it as math.
    fn text_mode(&mut self, raw: &str) -> Result<String, Error> {
        let mut out = String::new();
        let mut text = String::new();
        let mut parts = split_dollars(raw).into_iter();
        if let Some(first) = parts.next() {
            text.push_str(first);
        }
        let mut math = true;
        for part in parts {
            if math {
                if !text.is_empty() {
                    out.push_str(&string_literal(&plain_text(&text)));
                    out.push(' ');
                    text.clear();
                }
                self.enter()?;
                let mut inner = Parser {
                    text: part,
                    at: 0,
                    out: String::new(),
                    depth: self.depth,
                    cells: self.cells,
                };
                if inner.list(Stop::Eof, Table::No)? != Ended::Eof {
                    return error("unexpected end");
                }
                self.cells = inner.cells;
                self.leave();
                out.push_str(&inner.out);
            } else {
                text.push_str(part);
            }
            math = !math;
        }
        if !math {
            return error("a $ in text is not closed");
        }
        if !text.is_empty() || out.is_empty() {
            out.push_str(&string_literal(&plain_text(&text)));
        }
        Ok(trimmed(&out).to_owned())
    }
}

/// `text` without trailing spaces, except the one after a line break: a
/// `\` (an odd one, not half of an escaped backslash) before anything but
/// a space escapes it.
fn trimmed(text: &str) -> &str {
    let trimmed = text.trim_end();
    let backslashes = trimmed.len() - trimmed.trim_end_matches('\\').len();
    if backslashes % 2 == 1 && trimmed.len() < text.len() {
        &text[..trimmed.len() + 1]
    } else {
        trimmed
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `mat` with this `delim` value.
    Matrix(&'static str),
    Cases,
    /// Rows of the equation itself, with alignment points.
    Lines,
}

/// `raw` split at every `$` that is not escaped.
fn split_dollars(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut start, mut escaped) = (0, false);
    for (index, c) in raw.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '$' => {
                parts.push(&raw[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);
    parts
}

/// Text as LaTeX shows it: escaped characters as themselves, groups and
/// unknown commands without their markup.
fn plain_text(raw: &str) -> String {
    let mut text = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek().copied() {
                Some(next) if next.is_ascii_alphabetic() => {
                    let mut name = String::new();
                    while let Some(&letter) = chars.peek().filter(|c| c.is_ascii_alphabetic()) {
                        name.push(letter);
                        chars.next();
                    }
                    match name.as_str() {
                        "quad" | "qquad" | "space" | "enspace" => text.push(' '),
                        "textbackslash" | "backslash" => text.push('\\'),
                        "LaTeX" => text.push_str("LaTeX"),
                        "TeX" => text.push_str("TeX"),
                        _ => {}
                    }
                    while chars.peek() == Some(&' ') {
                        chars.next();
                    }
                }
                Some('\\') => {
                    chars.next();
                    text.push(' ');
                }
                Some(',' | ':' | ';' | ' ' | '!') => {
                    chars.next();
                    text.push(' ');
                }
                Some(next) => {
                    chars.next();
                    text.push(next);
                }
                None => {}
            },
            '{' | '}' => {}
            '~' => text.push('\u{a0}'),
            c if c.is_control() => text.push(' '),
            c => text.push(c),
        }
    }
    text
}

/// A Typst string literal: its only escapes are `\\` and `\"`.
fn string_literal(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for c in text.chars() {
        match c {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            c if c.is_control() => literal.push(' '),
            c => literal.push(c),
        }
    }
    literal.push('"');
    literal
}

/// Commands that change the style of the rest of their group: what goes
/// before and after it.
fn switch(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "displaystyle" => ("display(", ")"),
        "textstyle" => ("inline(", ")"),
        "scriptstyle" => ("script(", ")"),
        "scriptscriptstyle" => ("sscript(", ")"),
        "rm" => ("upright(", ")"),
        "bf" => ("upright(bold(", "))"),
        "it" => ("italic(", ")"),
        "sf" => ("sans(", ")"),
        "tt" => ("mono(", ")"),
        "cal" => ("cal(", ")"),
        // A colour cannot show in a mask the interface tints.
        "color" => ("", ""),
        _ => return None,
    })
}

/// Accents and other functions of one argument.
fn accent(name: &str) -> Option<&'static str> {
    Some(match name {
        "hat" | "widehat" => "hat",
        "tilde" | "widetilde" => "tilde",
        "bar" => "macron",
        "overline" => "overline",
        "underline" => "underline",
        "vec" | "overrightarrow" => "arrow",
        "overleftarrow" => "arrow.l",
        "overleftrightarrow" => "arrow.l.r",
        "dot" => "dot",
        "ddot" => "dot.double",
        "dddot" => "dot.triple",
        "ddddot" => "dot.quad",
        "acute" => "acute",
        "grave" => "grave",
        "breve" => "breve",
        "check" | "widecheck" => "caron",
        "mathring" => "circle",
        "cancel" | "bcancel" | "xcancel" | "sout" => "cancel",
        _ => return None,
    })
}

/// Math fonts; `@` stands for the argument.
fn font(name: &str) -> Option<&'static str> {
    Some(match name {
        "mathrm" | "mathup" => "upright(@)",
        "mathbf" => "upright(bold(@))",
        "boldsymbol" | "bm" | "pmb" | "mathbfit" => "bold(@)",
        "mathit" => "italic(@)",
        "mathbb" | "Bbb" | "mathds" => "bb(@)",
        "mathcal" => "cal(@)",
        "mathscr" => "scr(@)",
        "mathfrak" => "frak(@)",
        "mathsf" => "sans(@)",
        "mathtt" => "mono(@)",
        "mathnormal" => "@",
        _ => return None,
    })
}

/// Text in a font; `@` stands for the text.
fn text_font(name: &str) -> Option<&'static str> {
    Some(match name {
        "text" | "textrm" | "textnormal" | "textup" | "textmd" | "mbox" | "hbox" => "@",
        "textbf" => "bold(@)",
        "textit" | "textsl" | "emph" => "italic(@)",
        "texttt" => "mono(@)",
        "textsf" => "sans(@)",
        _ => return None,
    })
}

/// Named functions as upright operators, and whether limits go above and below.
fn operator(name: &str) -> Option<(&'static str, bool)> {
    Some(match name {
        "arccos" => ("arccos", false),
        "arcsin" => ("arcsin", false),
        "arctan" => ("arctan", false),
        "arg" => ("arg", false),
        "cos" => ("cos", false),
        "cosh" => ("cosh", false),
        "cot" => ("cot", false),
        "coth" => ("coth", false),
        "csc" => ("csc", false),
        "deg" => ("deg", false),
        "dim" => ("dim", false),
        "exp" => ("exp", false),
        "hom" => ("hom", false),
        "ker" => ("ker", false),
        "lg" => ("lg", false),
        "ln" => ("ln", false),
        "log" => ("log", false),
        "sec" => ("sec", false),
        "sin" => ("sin", false),
        "sinh" => ("sinh", false),
        "tan" => ("tan", false),
        "tanh" => ("tanh", false),
        "det" => ("det", true),
        "gcd" => ("gcd", true),
        "inf" => ("inf", true),
        "lim" => ("lim", true),
        "liminf" => ("lim inf", true),
        "limsup" => ("lim sup", true),
        "max" => ("max", true),
        "min" => ("min", true),
        "Pr" => ("Pr", true),
        "sup" => ("sup", true),
        "injlim" => ("inj lim", true),
        "projlim" => ("proj lim", true),
        _ => return None,
    })
}

/// Commands that stand for one symbol (Unicode, which Typst lays out by its
/// math class: `∑` takes limits, `≤` is a relation) or a Typst space.
fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        // Greek
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "digamma" => "ϝ",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "varGamma" => "𝛤",
        "varDelta" => "𝛥",
        "varTheta" => "𝛩",
        "varLambda" => "𝛬",
        "varXi" => "𝛯",
        "varPi" => "𝛱",
        "varSigma" => "𝛴",
        "varUpsilon" => "𝛶",
        "varPhi" => "𝛷",
        "varPsi" => "𝛹",
        "varOmega" => "𝛺",
        // Hebrew and letter-like
        "aleph" => "ℵ",
        "beth" => "ℶ",
        "gimel" => "ℷ",
        "daleth" => "ℸ",
        "hbar" | "hslash" => "ℏ",
        "ell" => "ℓ",
        "wp" => "℘",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "partial" => "∂",
        "nabla" => "∇",
        "infty" => "∞",
        "imath" => "ı",
        "jmath" => "ȷ",
        "eth" => "ð",
        "mho" => "℧",
        "Finv" => "Ⅎ",
        "Game" => "⅁",
        "complement" => "∁",
        // Binary operators
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "⋅",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "∙",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "circledast" => "⊛",
        "circledcirc" => "⊚",
        "boxplus" => "⊞",
        "boxminus" => "⊟",
        "boxtimes" => "⊠",
        "boxdot" => "⊡",
        "cap" => "∩",
        "cup" => "∪",
        "uplus" => "⊎",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "vee" | "lor" => "∨",
        "wedge" | "land" => "∧",
        "setminus" | "smallsetminus" => "∖",
        "wr" => "≀",
        "diamond" => "⋄",
        "bigtriangleup" => "△",
        "bigtriangledown" => "▽",
        "triangleleft" => "◁",
        "triangleright" => "▷",
        "lhd" => "⊲",
        "rhd" => "⊳",
        "unlhd" => "⊴",
        "unrhd" => "⊵",
        "amalg" => "⨿",
        "dagger" | "dag" => "†",
        "ddagger" | "ddag" => "‡",
        "ltimes" => "⋉",
        "rtimes" => "⋊",
        "barwedge" => "⊼",
        "veebar" => "⊻",
        "intercal" => "⊺",
        "dotplus" => "∔",
        "divideontimes" => "⋇",
        // Relations
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "leqslant" => "⩽",
        "geqslant" => "⩾",
        "lt" => "<",
        "gt" => ">",
        "ne" | "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "approxeq" => "≊",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "propto" | "varpropto" => "∝",
        "ll" => "≪",
        "gg" => "≫",
        "lll" => "⋘",
        "ggg" => "⋙",
        "lesssim" => "≲",
        "gtrsim" => "≳",
        "lessgtr" => "≶",
        "gtrless" => "≷",
        "prec" => "≺",
        "succ" => "≻",
        "preceq" => "⪯",
        "succeq" => "⪰",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "subsetneq" => "⊊",
        "supsetneq" => "⊋",
        "nsubseteq" => "⊈",
        "nsupseteq" => "⊉",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "sqsubseteq" => "⊑",
        "sqsupseteq" => "⊒",
        "in" | "isin" => "∈",
        "ni" | "owns" => "∋",
        "notin" => "∉",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "vDash" => "⊨",
        "Vdash" => "⊩",
        "perp" => "⟂",
        "parallel" => "∥",
        "nparallel" => "∦",
        "mid" => "∣",
        "nmid" => "∤",
        "asymp" => "≍",
        "doteq" => "≐",
        "bowtie" | "Join" => "⋈",
        "smile" => "⌣",
        "frown" => "⌢",
        "coloneqq" | "coloneq" => "≔",
        "eqqcolon" | "eqcolon" => "≕",
        "triangleq" => "≜",
        "nsim" => "≁",
        "ncong" => "≇",
        "nleq" => "≰",
        "ngeq" => "≱",
        "nless" => "≮",
        "ngtr" => "≯",
        "nprec" => "⊀",
        "nsucc" => "⊁",
        "nvdash" => "⊬",
        "nvDash" => "⊭",
        "therefore" => "∴",
        "because" => "∵",
        // Arrows
        "to" | "rightarrow" => "→",
        "gets" | "leftarrow" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "implies" | "Longrightarrow" => "⟹",
        "impliedby" | "Longleftarrow" => "⟸",
        "iff" | "Longleftrightarrow" => "⟺",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "longleftrightarrow" => "⟷",
        "mapsto" => "↦",
        "longmapsto" => "⟼",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "updownarrow" => "↕",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "Updownarrow" => "⇕",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "hookrightarrow" => "↪",
        "hookleftarrow" => "↩",
        "rightharpoonup" => "⇀",
        "rightharpoondown" => "⇁",
        "leftharpoonup" => "↼",
        "leftharpoondown" => "↽",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        "leadsto" | "rightsquigarrow" => "⇝",
        "twoheadrightarrow" => "↠",
        "twoheadleftarrow" => "↞",
        "rightrightarrows" => "⇉",
        "leftleftarrows" => "⇇",
        "rightleftarrows" => "⇄",
        "leftrightarrows" => "⇆",
        "circlearrowleft" => "↺",
        "circlearrowright" => "↻",
        "curvearrowleft" => "↶",
        "curvearrowright" => "↷",
        "Rrightarrow" => "⇛",
        "Lleftarrow" => "⇚",
        "nrightarrow" => "↛",
        "nleftarrow" => "↚",
        "nRightarrow" => "⇏",
        "nLeftarrow" => "⇍",
        "nleftrightarrow" => "↮",
        "nLeftrightarrow" => "⇎",
        "upharpoonleft" => "↿",
        "upharpoonright" => "↾",
        "downharpoonleft" => "⇃",
        "downharpoonright" => "⇂",
        // Big operators
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" | "intop" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "iiiint" => "⨌",
        "oint" => "∮",
        "oiint" => "∯",
        "oiiint" => "∰",
        "smallint" => "∫",
        "bigcup" => "⋃",
        "bigcap" => "⋂",
        "bigoplus" => "⨁",
        "bigotimes" => "⨂",
        "bigodot" => "⨀",
        "biguplus" => "⨄",
        "bigsqcup" => "⨆",
        "bigvee" => "⋁",
        "bigwedge" => "⋀",
        // Delimiters as symbols
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "vert" | "lvert" | "rvert" => "\\|",
        "Vert" | "lVert" | "rVert" => "‖",
        "lbrack" => "\\[",
        "rbrack" => "\\]",
        "lgroup" => "⟮",
        "rgroup" => "⟯",
        "llbracket" => "⟦",
        "rrbracket" => "⟧",
        "ulcorner" => "⌜",
        "urcorner" => "⌝",
        "llcorner" => "⌞",
        "lrcorner" => "⌟",
        "backslash" => "\\\\",
        // Dots
        "dots" | "ldots" | "dotsc" | "dotso" | "mathellipsis" => "…",
        "cdots" | "dotsb" | "dotsm" | "dotsi" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "iddots" | "adots" => "⋰",
        "cdotp" => "·",
        "ldotp" => ".",
        // Miscellaneous
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "emptyset" | "varnothing" => "∅",
        "neg" | "lnot" => "¬",
        "top" => "⊤",
        "bot" => "⊥",
        "angle" => "∠",
        "measuredangle" => "∡",
        "sphericalangle" => "∢",
        "triangle" | "vartriangle" => "△",
        "triangledown" => "▽",
        "prime" => "′",
        "backprime" => "‵",
        "surd" => "√",
        "flat" => "♭",
        "natural" => "♮",
        "sharp" => "♯",
        "clubsuit" => "♣",
        "diamondsuit" => "♢",
        "heartsuit" => "♡",
        "spadesuit" => "♠",
        "checkmark" => "✓",
        "square" | "Box" => "□",
        "blacksquare" => "■",
        "Diamond" => "◇",
        "lozenge" => "◊",
        "blacklozenge" => "⧫",
        "bigstar" => "★",
        "blacktriangle" => "▴",
        "S" | "sect" => "§",
        "P" => "¶",
        "copyright" => "©",
        "circledR" => "®",
        "pounds" => "£",
        "yen" => "¥",
        "euro" => "€",
        "degree" => "°",
        "colon" => ":",
        "diagup" => "╱",
        "diagdown" => "╲",
        "maltese" => "✠",
        _ => return None,
    })
}

/// Identifiers the converter writes, and therefore all that may appear.
const ALLOWED: &[&str] = &[
    "frac",
    "sqrt",
    "root",
    "binom",
    "mat",
    "cases",
    "lr",
    "mid",
    "class",
    "limits",
    "scripts",
    "op",
    "upright",
    "bold",
    "italic",
    "bb",
    "cal",
    "frak",
    "sans",
    "mono",
    "scr",
    "hat",
    "tilde",
    "macron",
    "overline",
    "underline",
    "overbrace",
    "underbrace",
    "overbracket",
    "underbracket",
    "overparen",
    "underparen",
    "arrow",
    "arrow.l",
    "arrow.l.r",
    "dot",
    "dot.double",
    "dot.triple",
    "dot.quad",
    "acute",
    "grave",
    "breve",
    "caron",
    "circle",
    "cancel",
    "thin",
    "med",
    "thick",
    "quad",
    "wide",
    "space",
    "space.nobreak",
    "space.en",
    "display",
    "inline",
    "script",
    "sscript",
    "stretch",
    "delim",
];

/// Whether `typst` is math the converter can have written: no code apart from
/// the `#none` of a matrix without delimiters, no `$` that would end the
/// equation, no syntax errors, and no identifiers but those in [`ALLOWED`].
pub fn check(typst: &str) -> Result<(), Error> {
    use typst::syntax::{LinkedNode, SyntaxKind};
    fn walk(node: &LinkedNode) -> Result<(), Error> {
        match node.kind() {
            SyntaxKind::Error | SyntaxKind::Dollar => {
                return error(format!("internal: rejected Typst {:?}", node.full_text()));
            }
            SyntaxKind::Hash => {
                let delimiter = node.parent().is_some_and(|parent| {
                    parent.kind() == SyntaxKind::Named
                        && parent
                            .children()
                            .next()
                            .is_some_and(|name| name.leaf_text() == "delim")
                });
                let none = node
                    .next_sibling()
                    .is_some_and(|value| value.kind() == SyntaxKind::None);
                if !(delimiter && none) {
                    return error("internal: code in converted math");
                }
            }
            SyntaxKind::MathFieldAccess | SyntaxKind::FieldAccess => {
                let text = node.full_text();
                if !ALLOWED.contains(&text.as_str()) {
                    return error(format!("internal: identifier {text}"));
                }
                return Ok(());
            }
            SyntaxKind::MathIdent | SyntaxKind::Ident => {
                if !ALLOWED.contains(&node.leaf_text().as_str()) {
                    return error(format!("internal: identifier {}", node.leaf_text()));
                }
            }
            SyntaxKind::Math
            | SyntaxKind::MathText
            | SyntaxKind::MathShorthand
            | SyntaxKind::MathAlignPoint
            | SyntaxKind::MathCall
            | SyntaxKind::MathArgs
            | SyntaxKind::MathDelimited
            | SyntaxKind::MathAttach
            | SyntaxKind::MathPrimes
            | SyntaxKind::MathFrac
            | SyntaxKind::MathRoot
            | SyntaxKind::Text
            | SyntaxKind::Space
            | SyntaxKind::Linebreak
            | SyntaxKind::Escape
            | SyntaxKind::Shorthand
            | SyntaxKind::Str
            | SyntaxKind::FuncCall
            | SyntaxKind::Args
            | SyntaxKind::Named
            | SyntaxKind::None
            | SyntaxKind::Colon
            | SyntaxKind::Comma
            | SyntaxKind::Semicolon
            | SyntaxKind::LeftParen
            | SyntaxKind::RightParen
            | SyntaxKind::Underscore
            | SyntaxKind::Hat
            | SyntaxKind::Dot
            | SyntaxKind::Slash
            | SyntaxKind::Root
            | SyntaxKind::Star
            | SyntaxKind::Bang
            | SyntaxKind::Plus
            | SyntaxKind::Minus
            | SyntaxKind::Eq
            | SyntaxKind::Lt
            | SyntaxKind::Gt => {}
            kind => return error(format!("internal: Typst {kind:?} in converted math")),
        }
        for child in node.children() {
            walk(&child)?;
        }
        Ok(())
    }
    let root = typst::syntax::parse_math(typst);
    walk(&LinkedNode::new(&root))
}

#[cfg(test)]
mod tests {
    use super::{Error, to_typst};

    fn ok(tex: &str) -> String {
        to_typst(tex).unwrap_or_else(|Error(error)| panic!("{tex:?}: {error}"))
    }

    #[test]
    fn letters_and_punctuation_cannot_form_identifiers_or_shorthands() {
        assert_eq!(ok("ab"), "a b");
        assert_eq!(ok("a->b"), "a - > b");
        assert_eq!(ok("x_1^{10}"), "x_(1)^(10)");
        assert_eq!(ok("x^10"), "x^(1) 0");
        assert_eq!(ok("12^2"), "1 2^(2)");
        assert_eq!(ok("3.14"), "3.14");
        assert_eq!(ok("f'(x)"), "f' \\( x \\)");
        assert_eq!(ok("a/b, c;d"), "a \\/ b \\, c \\; d");
        assert_eq!(ok("\\{x\\}"), "\\{ x \\}");
    }

    #[test]
    fn converts_common_constructs() {
        assert_eq!(ok("\\frac{a}{b}"), "frac(a, b)");
        assert_eq!(ok("\\frac12"), "frac(1, 2)");
        assert_eq!(ok("\\sqrt[3]{x}"), "root(3, x)");
        assert_eq!(ok("\\sum_{i=1}^n i"), "∑_(i = 1)^(n) i");
        assert_eq!(ok("\\sin x"), "op(\"sin\") x");
        assert_eq!(ok("\\lim_{x\\to 0}"), "limits(op(\"lim\"))_(x → 0)");
        assert_eq!(ok("\\text{if } x"), "\"if \" x");
        assert_eq!(ok("\\mathbf{v}"), "upright(bold(v))");
        assert_eq!(
            ok("\\left( \\frac{a}{b} \\right)"),
            "lr(\\( frac(a, b) \\))"
        );
        assert_eq!(ok("\\left. x \\right|"), "lr(x |)");
        assert_eq!(
            ok("\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}"),
            "mat(delim: \"(\", 1, 2; 3, 4)"
        );
        assert_eq!(
            ok("\\begin{matrix} a \\\\ b \\\\ \\end{matrix}"),
            "mat(delim: #none, a; b)"
        );
        assert_eq!(
            ok("f(x) = \\begin{cases} 1 & x > 0 \\\\ 0 & \\text{otherwise} \\end{cases}"),
            "f \\( x \\) = cases(& 1 && quad x > 0, & 0 && quad \"otherwise\")"
        );
        assert_eq!(
            ok("\\begin{aligned} a &= b \\\\ &= c \\end{aligned}"),
            "a & = b \\ & = c"
        );
        assert_eq!(ok("\\not\\in \\not\\prec"), "∉ ⊀");
        assert_eq!(
            ok("\\not\\star \\not a \\not 1"),
            "⋆\u{338} \"a\u{338}\" \"1\u{338}\""
        );
        // Found by fuzzing: a line break at the end of a group escaped the
        // parenthesis closing it.
        assert_eq!(ok("{a \\\\}"), "class(\"normal\", a \\ )");
        super::check(&ok("\\frac{a \\\\}{b} \\sqrt{x \\\\}")).unwrap();
        // Found by fuzzing: a negated space was an identifier `space̸`.
        assert_eq!(ok("\\not\\ "), "space");
        assert_eq!(ok("\\not\\frac12 \\not("), "frac(1, 2) \\(\u{338}");
        assert_eq!(ok("\\sum\\limits_0"), "limits(∑)_(0)");
        assert_eq!(ok("{}^{14}C"), "\"\"^(14) C");
        assert_eq!(ok("{a+b}^2"), "class(\"normal\", a + b)^(2)");
        assert_eq!(ok("\\underbrace{a+b}_{n}"), "underbrace(a + b, n)");
        assert_eq!(ok("x \\pmod{n}"), "x quad \\( op(\"mod\") thin n \\)");
        assert_eq!(
            ok("\\operatorname*{arg\\,max}_x"),
            "limits(op(\"arg max\"))_(x)"
        );
        assert_eq!(ok("{\\rm d}x"), "class(\"normal\", upright(d)) x");
        assert_eq!(ok("\\text{a $x$ b}"), "\"a \" x \" b\"");
    }

    #[test]
    fn rejects_invalid_input_with_a_reason() {
        for (tex, reason) in [
            ("\\foo", "unknown command \\foo"),
            ("{x", "a group is not closed"),
            ("x}", "an unopened }"),
            ("\\frac{a}", "an argument is missing"),
            ("\\left( x", "\\left without \\right"),
            ("\\begin{pmatrix} x", "an environment is not ended"),
            ("x^a^b", "a double superscript or subscript"),
            ("\\begin{foo} x \\end{foo}", "unknown environment foo"),
            (
                "\\begin{matrix} x \\end{pmatrix}",
                "\\begin{matrix} ended by \\end{pmatrix}",
            ),
            ("x\\", "a backslash at the end"),
        ] {
            assert_eq!(to_typst(tex), Err(Error(reason.into())), "{tex}");
        }
    }

    #[test]
    fn hostile_input_stays_math() {
        // Typst syntax in LaTeX is just characters: escaped, never code.
        for tex in [
            "#read(\"/etc/passwd\")",
            "$ #import \"@preview/x:1.0.0\" $",
            "\\text{\" + read(\"/etc/passwd\") + \"}",
            "\\text{\\\"#read(\"x\")}",
            "\\operatorname{\"); #panic(); (\"}",
            "\\iftypst #read(\"x\") \\fi",
            "read(\"x\")",
            "\\mathrm{read}(1)",
            "x \\\\ #y",
            "`#x` @label <label> \\$ $",
        ] {
            match to_typst(tex) {
                Ok(typst) => super::check(&typst).unwrap(),
                Err(Error(reason)) => assert!(!reason.starts_with("internal"), "{tex}: {reason}"),
            }
        }
        assert_eq!(ok("#x"), "\\# x");
        assert_eq!(ok("\\text{\"}"), "\"\\\"\"");
    }

    #[test]
    fn limits_depth_size_and_cells() {
        let deep = "{".repeat(40) + &"}".repeat(40);
        assert_eq!(to_typst(&deep), Err(Error("nested too deeply".into())));
        let deep = "\\frac{".repeat(40) + &"}{x}".repeat(40);
        assert_eq!(to_typst(&deep), Err(Error("nested too deeply".into())));
        let scripts = "x^{".repeat(40) + "x" + &"}".repeat(40);
        assert_eq!(to_typst(&scripts), Err(Error("nested too deeply".into())));
        let long = "x".repeat(super::MAX_SOURCE_BYTES + 1);
        assert!(to_typst(&long).is_err());
        let row = vec!["1"; 30].join(" & ");
        let big = format!(
            "\\begin{{matrix}}{}\\end{{matrix}}",
            vec![row; 30].join("\\\\")
        );
        assert_eq!(to_typst(&big), Err(Error("more than 400 cells".into())));
        // Text groups count their nesting too.
        let text = format!("\\text{{{}}}", "{".repeat(40) + &"}".repeat(40));
        assert_eq!(to_typst(&text), Err(Error("nested too deeply".into())));
    }

    #[test]
    fn check_rejects_what_the_converter_never_writes() {
        for typst in [
            "#read(\"x\")",
            "x $ #read(\"x\") $",
            "read(\"x\")",
            "mat(delim: #read(\"x\"), 1)",
            "eval(\"1\")",
            "frac(a",
        ] {
            assert!(super::check(typst).is_err(), "{typst}");
        }
        super::check("mat(delim: #none, 1, 2)").unwrap();
    }
}

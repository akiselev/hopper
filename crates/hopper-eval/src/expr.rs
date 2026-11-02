//! The expression language: identifiers (and `a::b` paths for statics), `.field`, `.0`, `[n]`,
//! `*e`, `&e`, `-n`, integer literals and parentheses.

use hopper_core::{HopperError, Result};

/// A parsed expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A variable or a static: `x`, `values::GLOBAL`.
    Name(String),
    Int(i128),
    /// `e.name` or `e.0` (tuple fields are written as their number).
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Deref(Box<Expr>),
    AddressOf(Box<Expr>),
    Negate(Box<Expr>),
}

impl Expr {
    /// The pointers this expression dereferences, nearest the value first: `(*n).vec[0]` gives
    /// `[n]`, `**p` gives `[*p, p]`. A deref inside an index expression is not on the value's
    /// path and is left out.
    pub fn dereferenced(&self) -> Vec<&Expr> {
        let mut out = Vec::new();
        let mut e = self;
        loop {
            e = match e {
                Expr::Field(base, _) | Expr::Index(base, _) => base,
                Expr::Deref(inner) => {
                    out.push(&**inner);
                    inner
                }
                Expr::AddressOf(_) | Expr::Negate(_) | Expr::Name(_) | Expr::Int(_) => {
                    return out;
                }
            };
        }
    }
}

/// Source text that [`parse`] reads back as the same expression.
impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Name(n) => f.write_str(n),
            Expr::Int(i) => write!(f, "{i}"),
            Expr::Field(base, name) => match **base {
                Expr::Deref(_) | Expr::AddressOf(_) | Expr::Negate(_) => {
                    write!(f, "({base}).{name}")
                }
                _ => write!(f, "{base}.{name}"),
            },
            Expr::Index(base, i) => match **base {
                Expr::Deref(_) | Expr::AddressOf(_) | Expr::Negate(_) => {
                    write!(f, "({base})[{i}]")
                }
                _ => write!(f, "{base}[{i}]"),
            },
            Expr::Deref(e) => write!(f, "*{e}"),
            Expr::AddressOf(e) => write!(f, "&{e}"),
            Expr::Negate(e) => write!(f, "-{e}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Int(i128),
    Dot,
    PathSep,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Star,
    Amp,
    Minus,
}

fn lex(src: &str) -> Result<Vec<Token>> {
    let bad = |m: String| HopperError::Invalid(format!("expression {src:?}: {m}"));
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '.' => {
                out.push(Token::Dot);
                i += 1;
            }
            ':' if chars.get(i + 1) == Some(&':') => {
                out.push(Token::PathSep);
                i += 2;
            }
            '[' => {
                out.push(Token::LBracket);
                i += 1;
            }
            ']' => {
                out.push(Token::RBracket);
                i += 1;
            }
            '(' => {
                out.push(Token::LParen);
                i += 1;
            }
            ')' => {
                out.push(Token::RParen);
                i += 1;
            }
            '*' => {
                out.push(Token::Star);
                i += 1;
            }
            '&' => {
                out.push(Token::Amp);
                i += 1;
            }
            '-' => {
                out.push(Token::Minus);
                i += 1;
            }
            c if c.is_ascii_digit() => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let text: String = chars[start..i].iter().filter(|c| **c != '_').collect();
                let value = if let Some(hex) = text.strip_prefix("0x") {
                    i128::from_str_radix(hex, 16)
                } else if let Some(bin) = text.strip_prefix("0b") {
                    i128::from_str_radix(bin, 2)
                } else if let Some(oct) = text.strip_prefix("0o") {
                    i128::from_str_radix(oct, 8)
                } else {
                    // Allow a type suffix such as `3usize` or `7u8`.
                    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
                    let suffix = &text[digits.len()..];
                    if !suffix.is_empty()
                        && !matches!(
                            suffix,
                            "u8" | "u16"
                                | "u32"
                                | "u64"
                                | "u128"
                                | "usize"
                                | "i8"
                                | "i16"
                                | "i32"
                                | "i64"
                                | "i128"
                                | "isize"
                        )
                    {
                        return Err(bad(format!("bad number {text:?}")));
                    }
                    digits.parse()
                };
                out.push(Token::Int(
                    value.map_err(|_| bad(format!("bad number {text:?}")))?,
                ));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                out.push(Token::Ident(chars[start..i].iter().collect()));
            }
            other => return Err(bad(format!("unexpected {other:?}"))),
        }
    }
    Ok(out)
}

/// Parse an expression.
pub fn parse(src: &str) -> Result<Expr> {
    let tokens = lex(src)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        src,
    };
    let e = p.unary()?;
    if p.pos != p.tokens.len() {
        return Err(p.error("trailing input"));
    }
    Ok(e)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    src: &'a str,
}

impl Parser<'_> {
    fn error(&self, m: &str) -> HopperError {
        HopperError::Invalid(format!(
            "expression {:?}: {m} at token {}",
            self.src, self.pos
        ))
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.eat(&Token::Star) {
            return Ok(Expr::Deref(Box::new(self.unary()?)));
        }
        if self.eat(&Token::Amp) {
            return Ok(Expr::AddressOf(Box::new(self.unary()?)));
        }
        if self.eat(&Token::Minus) {
            return Ok(Expr::Negate(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.eat(&Token::Dot) {
                match self.peek().cloned() {
                    Some(Token::Ident(name)) => {
                        self.pos += 1;
                        e = Expr::Field(Box::new(e), name);
                    }
                    Some(Token::Int(n)) if n >= 0 => {
                        self.pos += 1;
                        e = Expr::Field(Box::new(e), n.to_string());
                    }
                    _ => return Err(self.error("expected a field name after '.'")),
                }
            } else if self.eat(&Token::LBracket) {
                let index = self.unary()?;
                if !self.eat(&Token::RBracket) {
                    return Err(self.error("expected ']'"));
                }
                e = Expr::Index(Box::new(e), Box::new(index));
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().cloned() {
            Some(Token::Int(n)) => {
                self.pos += 1;
                Ok(Expr::Int(n))
            }
            Some(Token::Ident(first)) => {
                self.pos += 1;
                let mut path = first;
                while self.eat(&Token::PathSep) {
                    match self.peek().cloned() {
                        Some(Token::Ident(next)) => {
                            self.pos += 1;
                            path.push_str("::");
                            path.push_str(&next);
                        }
                        _ => return Err(self.error("expected a name after '::'")),
                    }
                }
                Ok(Expr::Name(path))
            }
            Some(Token::LParen) => {
                self.pos += 1;
                let e = self.unary()?;
                if !self.eat(&Token::RParen) {
                    return Err(self.error("expected ')'"));
                }
                Ok(e)
            }
            _ => Err(self.error("expected a name, a number or '('")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str) -> Box<Expr> {
        Box::new(Expr::Name(n.into()))
    }

    #[test]
    fn parses_postfix_chains() {
        assert_eq!(
            parse("v[0].name").unwrap(),
            Expr::Field(
                Box::new(Expr::Index(name("v"), Box::new(Expr::Int(0)))),
                "name".into()
            )
        );
        assert_eq!(
            parse("tuple.1").unwrap(),
            Expr::Field(name("tuple"), "1".into())
        );
        assert_eq!(
            parse("pair.0.1").unwrap(),
            Expr::Field(Box::new(Expr::Field(name("pair"), "0".into())), "1".into())
        );
    }

    #[test]
    fn prefix_operators_bind_looser_than_postfix() {
        assert_eq!(
            parse("*boxed.x").unwrap(),
            Expr::Deref(Box::new(Expr::Field(name("boxed"), "x".into())))
        );
        assert_eq!(
            parse("(*boxed).x").unwrap(),
            Expr::Field(Box::new(Expr::Deref(name("boxed"))), "x".into())
        );
        assert_eq!(
            parse("&a[1]").unwrap(),
            Expr::AddressOf(Box::new(Expr::Index(name("a"), Box::new(Expr::Int(1)))))
        );
        assert_eq!(parse("-42").unwrap(), Expr::Negate(Box::new(Expr::Int(42))));
    }

    #[test]
    fn literals_and_paths() {
        assert_eq!(parse("0x1f").unwrap(), Expr::Int(31));
        assert_eq!(parse("1_000usize").unwrap(), Expr::Int(1000));
        assert_eq!(
            parse("values::GLOBAL").unwrap(),
            Expr::Name("values::GLOBAL".into())
        );
        assert_eq!(
            parse("arr[i]").unwrap(),
            Expr::Index(name("arr"), name("i"))
        );
    }

    #[test]
    fn display_round_trips_and_derefs_are_listed() {
        for src in [
            "(*n).vec[0]",
            "**p",
            "a.b[3].c",
            "*v[0]",
            "&a[1]",
            "(*boxed).x",
            "-42",
        ] {
            let e = parse(src).unwrap();
            assert_eq!(e.to_string(), src);
            assert_eq!(parse(&e.to_string()).unwrap(), e);
        }
        let texts = |src: &str| -> Vec<String> {
            parse(src)
                .unwrap()
                .dereferenced()
                .iter()
                .map(|e| e.to_string())
                .collect()
        };
        assert_eq!(texts("(*n).vec[0]"), ["n"]);
        assert_eq!(texts("**p"), ["*p", "p"]);
        assert_eq!(texts("*v[0]"), ["v[0]"]);
        assert!(texts("a.b").is_empty());
        assert!(texts("&*a").is_empty());
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "a.", "a[1", "(a", "a b", "3x", "a + b", "::a"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }
}
